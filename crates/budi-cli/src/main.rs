use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use budi_core::config::{self, BudiConfig, CLAUDE_LOCAL_SETTINGS};
use budi_core::hooks::{
    AsyncSystemMessageOutput, PostToolUseInput, UserPromptSubmitInput, UserPromptSubmitOutput,
};
use budi_core::index;
use budi_core::reason_codes::{
    HOOK_REASON_DAEMON_UNAVAILABLE, HOOK_REASON_OK, HOOK_REASON_QUERY_ERROR,
    HOOK_REASON_QUERY_HTTP_ERROR, HOOK_REASON_QUERY_TIMEOUT, HOOK_REASON_QUERY_TRANSPORT_ERROR,
    HOOK_REASON_RESPONSE_PARSE_ERROR, HOOK_REASON_UPDATE_CONNECT_ERROR, HOOK_REASON_UPDATE_FAILED,
    HOOK_REASON_UPDATE_HTTP_ERROR, HOOK_REASON_UPDATE_TIMEOUT, SKIP_REASON_FORCED_SKIP,
    format_skip_hook_reason,
};
use budi_core::rpc::{
    IndexProgressRequest, IndexProgressResponse, IndexRequest, IndexResponse, QueryDiagnostics,
    QueryRequest, QueryResponse, QueryResultItem, StatusRequest, StatusResponse, UpdateRequest,
};
use clap::{ArgAction, Parser, Subcommand, ValueEnum};
use reqwest::blocking::Client;
use rusqlite::Connection;
use serde::Serialize;
use serde_json::{Value, json};
use tracing_subscriber::EnvFilter;

mod prompt_controls;
mod retrieval_eval;
#[cfg(test)]
use prompt_controls::PromptDirectives;
use prompt_controls::{
    evaluate_context_skip, excerpt, parse_prompt_directives, sanitize_prompt_for_query,
};
use retrieval_eval::{RetrievalEvalReport, load_retrieval_eval_report, run_retrieval_eval};

const HEALTH_TIMEOUT_SECS: u64 = 3;
const PREVIEW_QUERY_TIMEOUT_SECS: u64 = 180;
const SEARCH_QUERY_TIMEOUT_SECS: u64 = 30;
const BENCH_QUERY_TIMEOUT_SECS: u64 = 30;
const EVAL_QUERY_TIMEOUT_SECS: u64 = 45;
const DOCTOR_QUERY_TIMEOUT_SECS: u64 = 8;
const HOOK_QUERY_TIMEOUT_SECS: u64 = 12;
const HOOK_QUERY_RETRY_TIMEOUT_SECS: u64 = 45;
const STATUS_TIMEOUT_SECS: u64 = 120;
const UPDATE_TIMEOUT_SECS: u64 = 180;
const INDEX_TIMEOUT_SECS: u64 = 21_600;
const HOOK_LOG_LOCK_TIMEOUT_MS: u64 = 800;
const HOOK_LOG_LOCK_STALE_SECS: u64 = 30;

#[derive(Debug, Parser)]
#[command(name = "budi")]
#[command(about = "Deterministic local RAG hooks for Claude Code")]
#[command(version)]
struct Cli {
    #[arg(long, short = 'v', action = ArgAction::Count, global = true)]
    verbose: u8,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Init {
        #[arg(long)]
        repo_root: Option<PathBuf>,
        #[arg(long)]
        no_daemon: bool,
    },
    Index {
        #[arg(long)]
        repo_root: Option<PathBuf>,
        #[arg(long, default_value_t = false)]
        hard: bool,
        #[arg(long, default_value_t = false)]
        progress: bool,
        #[arg(long = "ignore-pattern", action = ArgAction::Append)]
        ignore_patterns: Vec<String>,
        #[arg(long = "include-ext", action = ArgAction::Append)]
        include_extensions: Vec<String>,
    },
    Doctor {
        #[arg(long)]
        repo_root: Option<PathBuf>,
        #[arg(long, default_value_t = false)]
        deep: bool,
    },
    Bench {
        #[arg(long)]
        repo_root: Option<PathBuf>,
        #[arg(long)]
        prompt: String,
        #[arg(long, default_value_t = 20)]
        iterations: usize,
    },
    Eval {
        #[command(subcommand)]
        command: EvalCommands,
    },
    Repo {
        #[command(subcommand)]
        command: RepoCommands,
    },
    #[command(hide = true)]
    Hook {
        #[command(subcommand)]
        command: HookCommands,
    },
}

#[derive(Debug, Subcommand)]
enum HookCommands {
    #[command(hide = true)]
    UserPromptSubmit,
    #[command(hide = true)]
    PostToolUse,
    #[command(hide = true)]
    SessionStart,
    #[command(hide = true)]
    SessionEnd,
}

#[derive(Debug, Subcommand)]
enum EvalCommands {
    Retrieval {
        #[arg(long)]
        repo_root: Option<PathBuf>,
        #[arg(long)]
        fixtures: Option<PathBuf>,
        #[arg(long, default_value_t = 8)]
        limit: usize,
        #[arg(long, value_enum, default_value_t = RetrievalModeArg::Hybrid)]
        mode: RetrievalModeArg,
        #[arg(long, default_value_t = false)]
        json: bool,
        #[arg(long)]
        out_dir: Option<PathBuf>,
        #[arg(long)]
        baseline: Option<PathBuf>,
        #[arg(long, default_value_t = false)]
        fail_on_regression: bool,
        #[arg(long, default_value_t = 0.0)]
        max_regression: f64,
    },
}

#[derive(Debug, Clone)]
struct EvalRetrievalOptions {
    json_output: bool,
    out_dir: Option<PathBuf>,
    baseline: Option<PathBuf>,
    fail_on_regression: bool,
    max_regression: f64,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum RetrievalModeArg {
    Hybrid,
    Lexical,
    Vector,
    #[value(name = "symbol-graph")]
    SymbolGraph,
}

impl RetrievalModeArg {
    fn as_rpc_mode(self) -> &'static str {
        match self {
            RetrievalModeArg::Hybrid => "hybrid",
            RetrievalModeArg::Lexical => "lexical",
            RetrievalModeArg::Vector => "vector",
            RetrievalModeArg::SymbolGraph => "symbol-graph",
        }
    }
}

#[derive(Debug, Subcommand)]
enum RepoCommands {
    List {
        #[arg(long, default_value_t = false)]
        stale_only: bool,
    },
    Remove {
        #[arg(long)]
        repo_root: PathBuf,
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
    Wipe {
        #[arg(long, default_value_t = false)]
        confirm: bool,
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
    Status {
        #[arg(long)]
        repo_root: Option<PathBuf>,
    },
    Stats {
        #[arg(long)]
        repo_root: Option<PathBuf>,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    Search {
        query: String,
        #[arg(long)]
        repo_root: Option<PathBuf>,
        #[arg(long, default_value_t = 8)]
        limit: usize,
        #[arg(long, value_enum, default_value_t = RetrievalModeArg::Hybrid)]
        mode: RetrievalModeArg,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    Preview {
        prompt: String,
        #[arg(long)]
        repo_root: Option<PathBuf>,
        #[arg(long, value_enum, default_value_t = RetrievalModeArg::Hybrid)]
        mode: RetrievalModeArg,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

#[derive(Debug, Serialize)]
struct BenchReport {
    repo_root: String,
    iterations: usize,
    latency_ms_p50: f64,
    latency_ms_p95: f64,
    avg_context_chars: f64,
    avg_context_tokens_estimate: f64,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let default_level = match cli.verbose {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();

    match cli.command {
        Commands::Init {
            repo_root,
            no_daemon,
        } => cmd_init(repo_root, no_daemon),
        Commands::Index {
            repo_root,
            hard,
            progress,
            ignore_patterns,
            include_extensions,
        } => cmd_index(
            repo_root,
            hard,
            progress,
            &ignore_patterns,
            &include_extensions,
        ),
        Commands::Doctor { repo_root, deep } => cmd_doctor(repo_root, deep),
        Commands::Bench {
            repo_root,
            prompt,
            iterations,
        } => cmd_bench(repo_root, &prompt, iterations),
        Commands::Eval { command } => match command {
            EvalCommands::Retrieval {
                repo_root,
                fixtures,
                limit,
                mode,
                json,
                out_dir,
                baseline,
                fail_on_regression,
                max_regression,
            } => {
                let options = EvalRetrievalOptions {
                    json_output: json,
                    out_dir,
                    baseline,
                    fail_on_regression,
                    max_regression,
                };
                cmd_eval_retrieval(repo_root, fixtures, limit, mode, options)
            }
        },
        Commands::Repo { command } => match command {
            RepoCommands::List { stale_only } => cmd_repo_list(stale_only),
            RepoCommands::Remove { repo_root, dry_run } => cmd_repo_remove(repo_root, dry_run),
            RepoCommands::Wipe { confirm, dry_run } => cmd_repo_wipe(confirm, dry_run),
            RepoCommands::Status { repo_root } => cmd_status(repo_root),
            RepoCommands::Stats { repo_root, json } => cmd_stats(repo_root, json),
            RepoCommands::Search {
                query,
                repo_root,
                limit,
                mode,
                json,
            } => cmd_search(repo_root, &query, limit, mode, json),
            RepoCommands::Preview {
                prompt,
                repo_root,
                mode,
                json,
            } => cmd_preview(repo_root, &prompt, mode, json),
        },
        Commands::Hook { command } => match command {
            HookCommands::UserPromptSubmit => cmd_hook_user_prompt_submit(),
            HookCommands::PostToolUse => cmd_hook_post_tool_use(),
            HookCommands::SessionStart => cmd_hook_session_start(),
            HookCommands::SessionEnd => cmd_hook_session_end(),
        },
    }
}

fn cmd_init(repo_root: Option<PathBuf>, no_daemon: bool) -> Result<()> {
    let repo_root = resolve_repo_root(repo_root)?;
    let config = config::load_or_default(&repo_root)?;
    config::ensure_repo_layout(&repo_root)?;
    config::save(&repo_root, &config)?;
    install_hooks(&repo_root)?;

    if !no_daemon {
        ensure_daemon_running(&repo_root, &config)?;
    }

    println!("Initialized budi in {}", repo_root.display());
    println!("Config: {}", config::config_path(&repo_root)?.display());
    println!(
        "Local data: {}",
        config::repo_paths(&repo_root)?.data_dir.display()
    );
    println!("Hooks: {}", repo_root.join(CLAUDE_LOCAL_SETTINGS).display());

    println!("Run `budi index --hard` to prewarm the first index.");
    println!("Restart Claude Code or review `/hooks` so updated hook settings are applied.");
    Ok(())
}

fn cmd_index(
    repo_root: Option<PathBuf>,
    hard: bool,
    progress: bool,
    ignore_patterns: &[String],
    include_extensions: &[String],
) -> Result<()> {
    let repo_root = resolve_repo_root(repo_root)?;
    let config = config::load_or_default(&repo_root)?;
    ensure_daemon_running(&repo_root, &config)?;
    let response = if progress {
        run_index_with_progress(
            &repo_root,
            &config,
            hard,
            ignore_patterns,
            include_extensions,
        )?
    } else {
        send_index_request(
            &config.daemon_base_url(),
            &repo_root.display().to_string(),
            hard,
            ignore_patterns,
            include_extensions,
        )?
    };
    println!(
        "Index {}: files={}, chunks={}, embedded={}, missing_embeddings={}, repaired_embeddings={}, invalid_embeddings={}, changed_files={}",
        response.index_status,
        response.indexed_files,
        response.indexed_chunks,
        response.embedded_chunks,
        response.missing_embeddings,
        response.repaired_embeddings,
        response.invalid_embeddings,
        response.changed_files
    );
    if let Some(job_id) = &response.job_id {
        let job_state = if response.job_state.trim().is_empty() {
            "unknown"
        } else {
            response.job_state.as_str()
        };
        println!("index job: {job_id} ({job_state})");
    }
    if let Some(outcome) = &response.terminal_outcome {
        println!("index outcome: {outcome}");
    }
    Ok(())
}

fn cmd_status(repo_root: Option<PathBuf>) -> Result<()> {
    let repo_root = resolve_repo_root(repo_root)?;
    let config = config::load_or_default(&repo_root)?;
    ensure_daemon_running(&repo_root, &config)?;
    let response =
        fetch_status_snapshot(&config.daemon_base_url(), &repo_root.display().to_string())
            .context("Status endpoint returned error")?;

    println!("budi daemon {}", response.daemon_version);
    println!("repo: {}", response.repo_root);
    println!("tracked files: {}", response.tracked_files);
    println!("indexed chunks: {}", response.indexed_chunks);
    println!("embedded chunks: {}", response.embedded_chunks);
    println!("missing embeddings: {}", response.missing_embeddings);
    println!("invalid embeddings: {}", response.invalid_embeddings);
    println!("update retries: {}", response.update_retries);
    println!("update failures: {}", response.update_failures);
    println!("updates noop: {}", response.updates_noop);
    println!("updates applied: {}", response.updates_applied);
    println!("watch events seen: {}", response.watch_events_seen);
    println!("watch events accepted: {}", response.watch_events_accepted);
    println!("watch events dropped: {}", response.watch_events_dropped);
    println!("index state: {}", response.index_state);
    println!("index job state: {}", response.index_job_state);
    if let Some(job_id) = &response.index_job_id {
        println!("index job id: {job_id}");
    }
    if let Some(outcome) = &response.index_terminal_outcome {
        println!("index terminal outcome: {outcome}");
    }
    println!("hooks detected: {}", response.hooks_detected);
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepoStorageEntryKind {
    Active,
    Stale,
    MarkerMissing,
}

#[derive(Debug, Clone)]
struct RepoStorageEntry {
    repo_id: String,
    data_dir: PathBuf,
    marker_repo_root: Option<PathBuf>,
    kind: RepoStorageEntryKind,
}

fn collect_repo_storage_entries() -> Result<(PathBuf, Vec<RepoStorageEntry>)> {
    let repos_root = config::repos_root_dir()?;
    if !repos_root.exists() {
        return Ok((repos_root, Vec::new()));
    }

    let mut entries = Vec::new();
    for entry in fs::read_dir(&repos_root)
        .with_context(|| format!("Failed reading {}", repos_root.display()))?
    {
        let entry = entry?;
        let data_dir = entry.path();
        if !data_dir.is_dir() {
            continue;
        }
        let repo_id = entry.file_name().to_string_lossy().to_string();
        let marker_repo_root = config::read_repo_root_marker(&data_dir);
        let kind = match marker_repo_root.as_ref() {
            Some(repo_root) if repo_root.join(".git").exists() => RepoStorageEntryKind::Active,
            Some(_) => RepoStorageEntryKind::Stale,
            None => RepoStorageEntryKind::MarkerMissing,
        };
        entries.push(RepoStorageEntry {
            repo_id,
            data_dir,
            marker_repo_root,
            kind,
        });
    }
    entries.sort_by(|left, right| left.repo_id.cmp(&right.repo_id));
    Ok((repos_root, entries))
}

fn cmd_repo_list(stale_only: bool) -> Result<()> {
    let (repos_root, entries) = collect_repo_storage_entries()?;
    if entries.is_empty() {
        println!("No local repo storage found at {}", repos_root.display());
        return Ok(());
    }
    let scanned = entries.len();
    let active = entries
        .iter()
        .filter(|entry| entry.kind == RepoStorageEntryKind::Active)
        .count();
    let stale = entries
        .iter()
        .filter(|entry| entry.kind == RepoStorageEntryKind::Stale)
        .count();
    let marker_missing = entries
        .iter()
        .filter(|entry| entry.kind == RepoStorageEntryKind::MarkerMissing)
        .count();

    println!("repo storage root: {}", repos_root.display());
    println!(
        "scanned={} active={} stale={} unknown_without_marker={}",
        scanned, active, stale, marker_missing
    );
    let filtered = entries
        .iter()
        .filter(|entry| !stale_only || entry.kind == RepoStorageEntryKind::Stale)
        .collect::<Vec<_>>();
    if filtered.is_empty() {
        if stale_only {
            println!("No stale repo state directories found.");
        } else {
            println!("No repo state directories found.");
        }
        return Ok(());
    }
    for entry in filtered {
        match entry.kind {
            RepoStorageEntryKind::Active => {
                if let Some(root) = &entry.marker_repo_root {
                    println!(
                        "- active  {} data_dir={} repo_root={}",
                        entry.repo_id,
                        entry.data_dir.display(),
                        root.display()
                    );
                }
            }
            RepoStorageEntryKind::Stale => {
                if let Some(root) = &entry.marker_repo_root {
                    println!(
                        "- stale   {} data_dir={} repo_root={}",
                        entry.repo_id,
                        entry.data_dir.display(),
                        root.display()
                    );
                }
            }
            RepoStorageEntryKind::MarkerMissing => {
                println!(
                    "- unknown {} data_dir={} repo_root=missing-marker",
                    entry.repo_id,
                    entry.data_dir.display()
                );
            }
        }
    }
    Ok(())
}

fn cmd_repo_remove(repo_root: PathBuf, dry_run: bool) -> Result<()> {
    let locator = if repo_root.is_absolute() {
        repo_root
    } else {
        std::env::current_dir()
            .context("Failed resolving current directory")?
            .join(repo_root)
    };
    let data_dir = config::repo_paths(&locator)?.data_dir;
    if !data_dir.exists() {
        println!("No local repo state found at {}", data_dir.display());
        return Ok(());
    }
    if dry_run {
        println!("Dry run: would remove {}", data_dir.display());
        return Ok(());
    }
    fs::remove_dir_all(&data_dir)
        .with_context(|| format!("Failed removing repo state {}", data_dir.display()))?;
    println!("Removed repo state {}", data_dir.display());
    Ok(())
}

fn cmd_repo_wipe(confirm: bool, dry_run: bool) -> Result<()> {
    let (repos_root, entries) = collect_repo_storage_entries()?;
    if entries.is_empty() {
        println!("No local repo storage found at {}", repos_root.display());
        return Ok(());
    }
    if !confirm {
        anyhow::bail!("Refusing to wipe repo storage without --confirm");
    }
    if dry_run {
        println!(
            "Dry run: would remove {} repo state directorie(s) from {}",
            entries.len(),
            repos_root.display()
        );
        for entry in &entries {
            println!(
                "- {} ({})",
                entry.data_dir.display(),
                match entry.kind {
                    RepoStorageEntryKind::Active => "active",
                    RepoStorageEntryKind::Stale => "stale",
                    RepoStorageEntryKind::MarkerMissing => "unknown",
                }
            );
        }
        return Ok(());
    }
    let mut removed = 0usize;
    for entry in entries {
        fs::remove_dir_all(&entry.data_dir)
            .with_context(|| format!("Failed removing repo state {}", entry.data_dir.display()))?;
        removed = removed.saturating_add(1);
    }
    println!("Removed {} repo state directorie(s).", removed);
    Ok(())
}

fn cmd_doctor(repo_root: Option<PathBuf>, deep: bool) -> Result<()> {
    let repo_root = resolve_repo_root(repo_root)?;
    let config = config::load_or_default(&repo_root)?;
    let paths = config::repo_paths(&repo_root)?;
    println!("repo root: {}", repo_root.display());
    println!(".git: {}", repo_root.join(".git").exists());
    println!("local data dir: {}", paths.data_dir.display());
    println!("config: {}", paths.config_file.exists());
    println!(
        "repo budi ignore: {}",
        config::ignore_path(&repo_root)?.exists()
    );
    println!(
        "global budi ignore: {}",
        config::global_ignore_path()?.exists()
    );
    println!(
        "hook settings: {}",
        repo_root.join(CLAUDE_LOCAL_SETTINGS).exists()
    );

    let health = daemon_health(&config);
    println!("daemon health: {health}");
    if !health {
        println!("Attempting daemon start...");
        ensure_daemon_running(&repo_root, &config)?;
        println!("daemon health after start: {}", daemon_health(&config));
    }
    if deep {
        run_deep_doctor_checks(&repo_root, &config)?;
    }
    Ok(())
}

fn cmd_preview(repo_root: Option<PathBuf>, prompt: &str, mode: RetrievalModeArg, json_output: bool) -> Result<()> {
    let repo_root = resolve_repo_root(repo_root)?;
    let config = config::load_or_default(&repo_root)?;
    ensure_daemon_running(&repo_root, &config)?;
    let directives = parse_prompt_directives(prompt);
    let sanitized_prompt = sanitize_prompt_for_query(prompt);
    let response = query_daemon_with_timeout_mode(
        &repo_root,
        &config,
        &sanitized_prompt,
        Some(&repo_root),
        PREVIEW_QUERY_TIMEOUT_SECS,
        Some(mode.as_rpc_mode()),
    )?;
    let effective_skip_reason = evaluate_context_skip(&config, &directives, &response.diagnostics);
    let forced_inject = directives.force_inject && !directives.force_skip;
    let recommended_injection = if forced_inject {
        true
    } else {
        effective_skip_reason.is_none() && response.diagnostics.recommended_injection
    };

    if json_output {
        let mut out = serde_json::to_value(&response)?;
        // Patch recommended_injection and skip_reason to reflect effective (post-directive) values
        if let Some(diag) = out.get_mut("diagnostics") {
            diag["recommended_injection"] = serde_json::Value::Bool(recommended_injection);
            if forced_inject {
                diag["skip_reason"] = serde_json::Value::String("forced_inject".to_string());
            } else if let Some(reason) = &effective_skip_reason {
                diag["skip_reason"] = serde_json::Value::String(reason.clone());
            }
        }
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    let skip_reason_display = if forced_inject {
        "forced_inject"
    } else {
        effective_skip_reason
            .as_deref()
            .or(response.diagnostics.skip_reason.as_deref())
            .unwrap_or("none")
    };
    let context_preview = if effective_skip_reason.is_none() {
        response.context.as_str()
    } else {
        ""
    };

    println!("retrieval_mode={}", mode.as_rpc_mode());
    println!("total candidates={}", response.total_candidates);
    if !response.diagnostics.intent.is_empty() {
        println!(
            "intent={} confidence={:.3} recommended_injection={} skip_reason={}",
            response.diagnostics.intent,
            response.diagnostics.confidence,
            recommended_injection,
            skip_reason_display
        );
    }
    for item in &response.snippets {
        println!(
            "- {}:{}-{} score={:.4} reasons={} channels={}",
            item.path,
            item.start_line,
            item.end_line,
            item.score,
            format_snippet_reasons(item),
            format_snippet_channels(item)
        );
    }
    println!("\n--- injected context preview ---\n{}", context_preview);
    Ok(())
}

fn cmd_search(
    repo_root: Option<PathBuf>,
    query: &str,
    limit: usize,
    mode: RetrievalModeArg,
    json_output: bool,
) -> Result<()> {
    if limit == 0 {
        anyhow::bail!("--limit must be at least 1");
    }
    let repo_root = resolve_repo_root(repo_root)?;
    let config = config::load_or_default(&repo_root)?;
    ensure_daemon_running(&repo_root, &config)?;
    let sanitized_query = sanitize_prompt_for_query(query);
    let response = query_daemon_with_timeout_mode(
        &repo_root,
        &config,
        &sanitized_query,
        Some(&repo_root),
        SEARCH_QUERY_TIMEOUT_SECS,
        Some(mode.as_rpc_mode()),
    )?;
    let limited_snippets = response
        .snippets
        .iter()
        .take(limit)
        .cloned()
        .collect::<Vec<_>>();
    if json_output {
        let payload = json!({
            "query": query,
            "retrieval_mode": mode.as_rpc_mode(),
            "total_candidates": response.total_candidates,
            "returned": limited_snippets.len(),
            "diagnostics": response.diagnostics,
            "snippets": limited_snippets,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    println!("query: {}", query);
    println!("retrieval_mode={}", mode.as_rpc_mode());
    println!(
        "total candidates={} returned={}",
        response.total_candidates,
        limited_snippets.len()
    );
    if !response.diagnostics.intent.is_empty() {
        println!(
            "intent={} confidence={:.3} top_score={:.3} margin={:.3}",
            response.diagnostics.intent,
            response.diagnostics.confidence,
            response.diagnostics.top_score,
            response.diagnostics.margin
        );
    }
    for item in &limited_snippets {
        println!(
            "- {}:{}-{} score={:.4} reasons={} channels={}",
            item.path,
            item.start_line,
            item.end_line,
            item.score,
            format_snippet_reasons(item),
            format_snippet_channels(item)
        );
    }
    Ok(())
}

fn cmd_bench(repo_root: Option<PathBuf>, prompt: &str, iterations: usize) -> Result<()> {
    if iterations == 0 {
        anyhow::bail!("--iterations must be at least 1");
    }
    let repo_root = resolve_repo_root(repo_root)?;
    let config = config::load_or_default(&repo_root)?;
    ensure_daemon_running(&repo_root, &config)?;
    let query_url = format!("{}/query", config.daemon_base_url());
    let client = daemon_client_with_timeout(Duration::from_secs(BENCH_QUERY_TIMEOUT_SECS));

    let mut latencies_ms = Vec::with_capacity(iterations);
    let mut context_chars = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        let response = send_query_request(
            &client,
            &query_url,
            &repo_root,
            prompt,
            Some(&repo_root),
            None,
            None,
        )
        .context("Failed benchmark query iteration")?;
        latencies_ms.push(start.elapsed().as_secs_f64() * 1000.0);
        context_chars.push(response.context.len() as f64);
    }

    let avg_context_chars = mean(&context_chars);
    let report = BenchReport {
        repo_root: repo_root.display().to_string(),
        iterations,
        latency_ms_p50: percentile(&latencies_ms, 0.50),
        latency_ms_p95: percentile(&latencies_ms, 0.95),
        avg_context_chars,
        avg_context_tokens_estimate: avg_context_chars / 4.0,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn cmd_eval_retrieval(
    repo_root: Option<PathBuf>,
    fixtures: Option<PathBuf>,
    limit: usize,
    mode: RetrievalModeArg,
    options: EvalRetrievalOptions,
) -> Result<()> {
    let EvalRetrievalOptions {
        json_output,
        out_dir,
        baseline,
        fail_on_regression,
        max_regression,
    } = options;
    if max_regression < 0.0 {
        anyhow::bail!("--max-regression must be non-negative");
    }
    let repo_root = resolve_repo_root(repo_root)?;
    let config = config::load_or_default(&repo_root)?;
    ensure_daemon_running(&repo_root, &config)?;
    let fixtures_path =
        fixtures.unwrap_or_else(|| repo_root.join(".budi").join("eval").join("retrieval.json"));
    let report = run_retrieval_eval(
        &repo_root,
        &fixtures_path,
        mode.as_rpc_mode(),
        limit,
        |sanitized_query| {
            query_daemon_with_timeout_mode(
                &repo_root,
                &config,
                sanitized_query,
                Some(&repo_root),
                EVAL_QUERY_TIMEOUT_SECS,
                Some(mode.as_rpc_mode()),
            )
        },
    )?;
    let artifact_path = persist_retrieval_eval_report(&repo_root, out_dir.as_deref(), &report)?;
    let baseline_path =
        resolve_retrieval_eval_baseline(baseline.as_deref(), &artifact_path, mode.as_rpc_mode())?;
    let regression = if let Some(path) = baseline_path.as_deref() {
        let baseline_report = load_retrieval_eval_report(path)?;
        Some(build_retrieval_eval_regression(
            &artifact_path,
            &report,
            path,
            &baseline_report,
            max_regression,
        ))
    } else {
        None
    };
    let regression_failed = if fail_on_regression {
        match regression.as_ref() {
            Some(summary) => !summary.passed,
            None => true,
        }
    } else {
        false
    };

    if json_output {
        let payload = json!({
            "artifact_path": artifact_path.display().to_string(),
            "baseline_artifact": baseline_path.as_ref().map(|path| path.display().to_string()),
            "regression": regression,
            "report": report,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        if regression_failed {
            anyhow::bail!("retrieval regression gate failed");
        }
        return Ok(());
    }

    println!("repo: {}", report.repo_root);
    println!("fixtures: {}", report.fixtures_path);
    println!("retrieval_mode: {}", report.retrieval_mode);
    println!(
        "cases: total={} scored={} errors={}",
        report.total_cases, report.scored_cases, report.cases_with_errors
    );
    println!(
        "metrics: hit@1={:.3} hit@3={:.3} hit@5={:.3} mrr={:.3}",
        report.metrics.hit_at_1,
        report.metrics.hit_at_3,
        report.metrics.hit_at_5,
        report.metrics.mrr
    );
    println!(
        "metrics: precision@1={:.3} precision@3={:.3} precision@5={:.3}",
        report.metrics.precision_at_1, report.metrics.precision_at_3, report.metrics.precision_at_5
    );
    println!(
        "metrics: recall@1={:.3} recall@3={:.3} recall@5={:.3}",
        report.metrics.recall_at_1, report.metrics.recall_at_3, report.metrics.recall_at_5
    );
    println!(
        "metrics: f1@1={:.3} f1@3={:.3} f1@5={:.3}",
        report.metrics.f1_at_1, report.metrics.f1_at_3, report.metrics.f1_at_5
    );
    if !report.per_intent_metrics.is_empty() {
        let mut intent_rows = report
            .per_intent_metrics
            .iter()
            .map(|(intent, metrics)| (intent.as_str(), metrics))
            .collect::<Vec<_>>();
        intent_rows.sort_by(|left, right| left.0.cmp(right.0));
        println!("per-intent metrics:");
        for (intent, metrics) in intent_rows {
            println!(
                "- {} cases={} hit@1={:.3} hit@3={:.3} hit@5={:.3} mrr={:.3} f1@3={:.3}",
                intent,
                metrics.cases,
                metrics.hit_at_1,
                metrics.hit_at_3,
                metrics.hit_at_5,
                metrics.mrr,
                metrics.f1_at_3
            );
        }
    }
    for case in &report.results {
        let rank_display = case.rank.map_or("-".to_string(), |r| r.to_string());
        let expected = if case.expected_paths.is_empty() {
            "-".to_string()
        } else {
            case.expected_paths.join(",")
        };
        let top = if case.top_paths.is_empty() {
            "-".to_string()
        } else {
            case.top_paths.join(",")
        };
        println!(
            "- rank={} matched@1/3/5={}/{}/{} intent={} confidence={:.3} query=\"{}\" expected={} top={}",
            rank_display,
            case.matched_at_1,
            case.matched_at_3,
            case.matched_at_5,
            case.intent,
            case.confidence,
            case.query,
            expected,
            top
        );
        if let Some(err) = &case.error {
            println!("  error={err}");
        }
    }
    if let Some(summary) = &regression {
        println!(
            "regression: baseline={} current={} max_drop={:.3} comparable={} passed={}",
            summary.baseline_artifact,
            summary.current_artifact,
            summary.max_regression,
            summary.comparable,
            summary.passed
        );
        if !summary.scope_mismatches.is_empty() {
            println!(
                "regression scope mismatches: {}",
                summary.scope_mismatches.join("; ")
            );
        } else {
            for check in &summary.checks {
                println!(
                    "- {} baseline={:.3} current={:.3} delta={:+.3} allowed_drop={:.3} passed={}",
                    check.metric,
                    check.baseline,
                    check.current,
                    check.delta,
                    check.max_drop,
                    check.passed
                );
            }
        }
    } else {
        println!("regression: baseline=none (no prior artifact found)");
    }
    println!("artifact: {}", artifact_path.display());
    if regression_failed {
        anyhow::bail!("retrieval regression gate failed");
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
struct RetrievalEvalRegressionCheck {
    metric: String,
    baseline: f64,
    current: f64,
    delta: f64,
    max_drop: f64,
    passed: bool,
}

#[derive(Debug, Clone, Serialize)]
struct RetrievalEvalRegressionSummary {
    baseline_artifact: String,
    current_artifact: String,
    max_regression: f64,
    comparable: bool,
    scope_mismatches: Vec<String>,
    checks: Vec<RetrievalEvalRegressionCheck>,
    passed: bool,
}

fn resolve_retrieval_eval_baseline(
    explicit_baseline: Option<&Path>,
    current_artifact: &Path,
    retrieval_mode: &str,
) -> Result<Option<PathBuf>> {
    if let Some(path) = explicit_baseline {
        return Ok(Some(path.to_path_buf()));
    }
    let Some(parent) = current_artifact.parent() else {
        return Ok(None);
    };
    find_previous_retrieval_eval_artifact(parent, retrieval_mode, current_artifact)
}

fn find_previous_retrieval_eval_artifact(
    output_dir: &Path,
    retrieval_mode: &str,
    current_artifact: &Path,
) -> Result<Option<PathBuf>> {
    if !output_dir.exists() {
        return Ok(None);
    }
    let mode = retrieval_mode.replace('-', "_");
    let expected_prefix = format!("retrieval-{mode}-");
    let mut candidates = Vec::new();
    for entry in fs::read_dir(output_dir)
        .with_context(|| format!("Failed reading eval artifact dir {}", output_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path == current_artifact || !path.is_file() {
            continue;
        }
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if file_name.starts_with(&expected_prefix) && file_name.ends_with(".json") {
            candidates.push(path);
        }
    }
    candidates.sort_by(|left, right| {
        right
            .file_name()
            .and_then(|name| name.to_str())
            .cmp(&left.file_name().and_then(|name| name.to_str()))
    });
    Ok(candidates.into_iter().next())
}

fn build_retrieval_eval_regression(
    current_artifact: &Path,
    current: &RetrievalEvalReport,
    baseline_artifact: &Path,
    baseline: &RetrievalEvalReport,
    max_regression: f64,
) -> RetrievalEvalRegressionSummary {
    let mut scope_mismatches = Vec::new();
    if baseline.fixtures_path != current.fixtures_path {
        scope_mismatches.push(format!(
            "fixtures_path differs (baseline={} current={})",
            baseline.fixtures_path, current.fixtures_path
        ));
    }
    if baseline.retrieval_mode != current.retrieval_mode {
        scope_mismatches.push(format!(
            "retrieval_mode differs (baseline={} current={})",
            baseline.retrieval_mode, current.retrieval_mode
        ));
    }
    if baseline.limit != current.limit {
        scope_mismatches.push(format!(
            "limit differs (baseline={} current={})",
            baseline.limit, current.limit
        ));
    }
    let comparable = scope_mismatches.is_empty();
    let checks = if comparable {
        vec![
            build_regression_check(
                "hit_at_3",
                baseline.metrics.hit_at_3,
                current.metrics.hit_at_3,
                max_regression,
            ),
            build_regression_check(
                "mrr",
                baseline.metrics.mrr,
                current.metrics.mrr,
                max_regression,
            ),
            build_regression_check(
                "f1_at_3",
                baseline.metrics.f1_at_3,
                current.metrics.f1_at_3,
                max_regression,
            ),
        ]
    } else {
        Vec::new()
    };
    let passed = comparable && checks.iter().all(|check| check.passed);
    RetrievalEvalRegressionSummary {
        baseline_artifact: baseline_artifact.display().to_string(),
        current_artifact: current_artifact.display().to_string(),
        max_regression,
        comparable,
        scope_mismatches,
        checks,
        passed,
    }
}

fn build_regression_check(
    metric: &str,
    baseline: f64,
    current: f64,
    max_regression: f64,
) -> RetrievalEvalRegressionCheck {
    let delta = current - baseline;
    RetrievalEvalRegressionCheck {
        metric: metric.to_string(),
        baseline,
        current,
        delta,
        max_drop: max_regression,
        passed: delta >= -max_regression,
    }
}

fn format_snippet_reasons(item: &QueryResultItem) -> String {
    if item.reasons.is_empty() {
        "semantic+lexical".to_string()
    } else {
        item.reasons.join(",")
    }
}

fn format_snippet_channels(item: &QueryResultItem) -> String {
    format!(
        "lexical={:.3},vector={:.3},symbol={:.3},path={:.3},graph={:.3},rerank={:.3}",
        item.channel_scores.lexical,
        item.channel_scores.vector,
        item.channel_scores.symbol,
        item.channel_scores.path,
        item.channel_scores.graph,
        item.channel_scores.rerank
    )
}

fn runtime_guard_is_non_prod_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.starts_with("test/")
        || lower.starts_with("tests/")
        || lower.contains("/test/")
        || lower.contains("/tests/")
        || lower.starts_with("examples/")
        || lower.contains("/examples/")
        || lower.starts_with("example/")
        || lower.contains("/example/")
        || lower.contains("/fixtures/")
        || lower.contains("/fixture/")
}

fn build_runtime_guard_context(snippets: &[QueryResultItem]) -> String {
    let mut selected = Vec::new();
    let mut seen = HashSet::new();
    for snippet in snippets {
        let path = snippet.path.trim();
        if path.is_empty() {
            continue;
        }
        if runtime_guard_is_non_prod_path(path) {
            continue;
        }
        // Only include snippets with explicit runtime-config signals.
        let has_runtime_signal = snippet.reasons.iter().any(|r| {
            r.starts_with("runtime-")
                || r.starts_with("symbol-hit")
                || r.starts_with("path-hit")
                || r.starts_with("graph-hit")
                || r.starts_with("lexical-hit")
        });
        if !has_runtime_signal {
            continue;
        }
        if !seen.insert(path.to_string()) {
            continue;
        }
        selected.push(path.to_string());
        if selected.len() >= 5 {
            break;
        }
    }
    if selected.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    out.push_str("[budi runtime guard]\n");
    out.push_str("rules:\n");
    out.push_str("- Use only the file paths listed below.\n");
    out.push_str(
        "- Prefer core source files; do not include tests/examples unless explicitly asked.\n",
    );
    out.push_str("- If unsure about function names, return file paths only.\n");
    out.push_str("verified_runtime_paths:\n");
    for path in selected {
        out.push_str("- ");
        out.push_str(&path);
        out.push('\n');
    }
    out
}

fn persist_retrieval_eval_report(
    repo_root: &Path,
    out_dir: Option<&Path>,
    report: &RetrievalEvalReport,
) -> Result<PathBuf> {
    let output_dir = out_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| repo_root.join(".budi").join("eval").join("runs"));
    fs::create_dir_all(&output_dir)
        .with_context(|| format!("Failed creating eval artifact dir {}", output_dir.display()))?;
    let mode = report.retrieval_mode.replace('-', "_");
    let path = output_dir.join(format!("retrieval-{mode}-{}.json", now_unix_ms()));
    let payload = serde_json::to_string_pretty(report)?;
    fs::write(&path, payload)
        .with_context(|| format!("Failed writing eval artifact {}", path.display()))?;
    Ok(path)
}

fn embedding_integrity_counts(chunks: &[index::ChunkRecord]) -> (usize, usize, usize, usize) {
    let mut dims_to_counts: HashMap<usize, usize> = HashMap::new();
    for chunk in chunks {
        if chunk.embedding.is_empty() || chunk.embedding.iter().any(|value| !value.is_finite()) {
            continue;
        }
        *dims_to_counts.entry(chunk.embedding.len()).or_insert(0) += 1;
    }
    let expected_dims = dims_to_counts
        .into_iter()
        .max_by(|(left_dims, left_count), (right_dims, right_count)| {
            left_count
                .cmp(right_count)
                .then_with(|| left_dims.cmp(right_dims))
        })
        .map(|(dims, _)| dims)
        .unwrap_or(0);

    let mut embedded = 0usize;
    let mut missing = 0usize;
    let mut invalid = 0usize;
    for chunk in chunks {
        if chunk.embedding.is_empty() {
            missing = missing.saturating_add(1);
            continue;
        }
        let has_non_finite = chunk.embedding.iter().any(|value| !value.is_finite());
        let dims_mismatch = expected_dims > 0 && chunk.embedding.len() != expected_dims;
        if has_non_finite || dims_mismatch {
            invalid = invalid.saturating_add(1);
        } else {
            embedded = embedded.saturating_add(1);
        }
    }
    (embedded, missing, invalid, expected_dims)
}

fn cmd_stats(repo_root: Option<PathBuf>, json_output: bool) -> Result<()> {
    let repo_root = resolve_repo_root(repo_root)?;
    let config = config::load_or_default(&repo_root)?;
    let index_db_path = config::index_db_path(&repo_root)?;
    let tantivy_path = config::tantivy_path(&repo_root)?;
    let state = index::load_state(&repo_root)?;
    let daemon_healthy = daemon_health(&config);
    let hooks_detected = repo_root.join(CLAUDE_LOCAL_SETTINGS).exists();

    let indexed_files = state.as_ref().map_or(0usize, |s| s.files.len());
    let indexed_chunks = state.as_ref().map_or(0usize, |s| s.chunks.len());
    let (embedded_chunks, missing_embeddings, invalid_embeddings, expected_embedding_dims) = state
        .as_ref()
        .map(|s| embedding_integrity_counts(&s.chunks))
        .unwrap_or((0, 0, 0, 0));
    let catalog_updated_at_ts = state.as_ref().map_or(0i64, |s| s.updated_at_ts);
    let chunks_per_file = if indexed_files == 0 {
        0.0
    } else {
        indexed_chunks as f64 / indexed_files as f64
    };
    let index_db_bytes = fs::metadata(&index_db_path).map(|m| m.len()).unwrap_or(0);

    if json_output {
        let payload = json!({
            "repo_root": repo_root.display().to_string(),
            "daemon_healthy": daemon_healthy,
            "hooks_detected": hooks_detected,
            "indexed_files": indexed_files,
            "indexed_chunks": indexed_chunks,
            "embedded_chunks": embedded_chunks,
            "missing_embeddings": missing_embeddings,
            "invalid_embeddings": invalid_embeddings,
            "expected_embedding_dims": expected_embedding_dims,
            "chunks_per_file": chunks_per_file,
            "catalog_updated_at_ts": catalog_updated_at_ts,
            "index_db_file": index_db_path.display().to_string(),
            "index_db_file_bytes": index_db_bytes,
            "index_db_exists": index_db_path.exists(),
            "tantivy_dir": tantivy_path.display().to_string(),
            "tantivy_exists": tantivy_path.exists(),
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    println!("repo: {}", repo_root.display());
    println!("daemon healthy: {}", daemon_healthy);
    println!("hooks detected: {}", hooks_detected);
    println!("indexed files: {}", indexed_files);
    println!("indexed chunks: {}", indexed_chunks);
    println!("embedded chunks: {}", embedded_chunks);
    println!("missing embeddings: {}", missing_embeddings);
    println!("invalid embeddings: {}", invalid_embeddings);
    println!("expected embedding dims: {}", expected_embedding_dims);
    println!("chunks/file: {:.2}", chunks_per_file);
    println!("catalog updated_at_ts: {}", catalog_updated_at_ts);
    println!(
        "index db: {} ({} bytes, exists: {})",
        index_db_path.display(),
        index_db_bytes,
        index_db_path.exists()
    );
    println!(
        "tantivy dir: {} (exists: {})",
        tantivy_path.display(),
        tantivy_path.exists()
    );
    Ok(())
}

fn run_deep_doctor_checks(repo_root: &Path, config: &BudiConfig) -> Result<()> {
    println!("\n-- deep checks --");
    let repo_root_str = repo_root.display().to_string();
    let index_db_path = config::index_db_path(repo_root)?;
    let tantivy_path = config::tantivy_path(repo_root)?;
    let embedding_cache_path = config::embedding_cache_path()?;
    let state = index::load_state(repo_root)?;

    let index_db_bytes = fs::metadata(&index_db_path).map(|m| m.len()).unwrap_or(0);
    println!(
        "index db: exists={} bytes={}",
        index_db_path.exists(),
        index_db_bytes
    );

    let mut sqlite_files_count = 0usize;
    let mut sqlite_chunks_count = 0usize;
    let mut sqlite_index_progress_rows = 0usize;
    if index_db_path.exists()
        && let Ok(conn) = Connection::open(&index_db_path)
    {
        if let Ok(count) =
            conn.query_row("SELECT COUNT(*) FROM files", [], |row| row.get::<_, i64>(0))
        {
            sqlite_files_count = count.max(0) as usize;
        }
        if let Ok(count) = conn.query_row("SELECT COUNT(*) FROM chunks", [], |row| {
            row.get::<_, i64>(0)
        }) {
            sqlite_chunks_count = count.max(0) as usize;
        }
        if let Ok(count) = conn.query_row("SELECT COUNT(*) FROM index_progress", [], |row| {
            row.get::<_, i64>(0)
        }) {
            sqlite_index_progress_rows = count.max(0) as usize;
        }
    }

    let mut duplicate_file_paths = 0usize;
    let mut duplicate_chunk_ids = 0usize;
    let mut orphan_chunks = 0usize;
    let mut state_embedded_chunks = 0usize;
    let mut state_missing_embeddings = 0usize;
    let mut state_invalid_embeddings = 0usize;
    let mut state_expected_embedding_dims = 0usize;
    if let Some(index_state) = &state {
        let mut seen_paths: HashSet<&str> = HashSet::new();
        for file in &index_state.files {
            if !seen_paths.insert(file.path.as_str()) {
                duplicate_file_paths = duplicate_file_paths.saturating_add(1);
            }
        }
        let file_paths: HashSet<&str> = index_state
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect();
        let mut seen_chunk_ids: HashSet<u64> = HashSet::new();
        for chunk in &index_state.chunks {
            if !seen_chunk_ids.insert(chunk.id) {
                duplicate_chunk_ids = duplicate_chunk_ids.saturating_add(1);
            }
            if !file_paths.contains(chunk.path.as_str()) {
                orphan_chunks = orphan_chunks.saturating_add(1);
            }
        }
        (
            state_embedded_chunks,
            state_missing_embeddings,
            state_invalid_embeddings,
            state_expected_embedding_dims,
        ) = embedding_integrity_counts(&index_state.chunks);
    }
    println!(
        "catalog consistency: duplicate_file_paths={} duplicate_chunk_ids={} orphan_chunks={}",
        duplicate_file_paths, duplicate_chunk_ids, orphan_chunks
    );
    println!(
        "embedding coverage: embedded_chunks={} missing_embeddings={} invalid_embeddings={} expected_dims={}",
        state_embedded_chunks,
        state_missing_embeddings,
        state_invalid_embeddings,
        state_expected_embedding_dims
    );
    println!(
        "embedding integrity: {}",
        if state_invalid_embeddings == 0 {
            "ok"
        } else {
            "degraded"
        }
    );

    let tantivy_entries = fs::read_dir(&tantivy_path)
        .map(|entries| entries.filter_map(std::result::Result::ok).count())
        .unwrap_or(0);
    println!(
        "tantivy dir: exists={} entries={}",
        tantivy_path.exists(),
        tantivy_entries
    );

    let mut embedding_cache_valid = false;
    let mut embedding_cache_entries = 0usize;
    if embedding_cache_path.exists()
        && let Ok(conn) = Connection::open(&embedding_cache_path)
        && let Ok(count) = conn.query_row("SELECT COUNT(*) FROM embeddings", [], |row| {
            row.get::<_, i64>(0)
        })
    {
        embedding_cache_valid = true;
        embedding_cache_entries = count.max(0) as usize;
    }
    println!(
        "embedding cache: exists={} valid_db={} entries={}",
        embedding_cache_path.exists(),
        embedding_cache_valid,
        embedding_cache_entries
    );

    let mut semantic_backend_ready = false;
    let mut semantic_embedding_dims = 0usize;
    let mut semantic_probe_error = String::new();
    match index::embed_query(repo_root, "doctor semantic backend probe") {
        Ok(Some(embedding)) if !embedding.is_empty() => {
            semantic_backend_ready = true;
            semantic_embedding_dims = embedding.len();
        }
        Ok(_) => {}
        Err(err) => {
            semantic_probe_error = err.to_string();
        }
    }
    println!(
        "semantic backend: ready={} dims={} error={}",
        semantic_backend_ready,
        semantic_embedding_dims,
        if semantic_probe_error.is_empty() {
            "-"
        } else {
            semantic_probe_error.as_str()
        }
    );

    let client = daemon_client_with_timeout(Duration::from_secs(HEALTH_TIMEOUT_SECS));
    let health_url = format!("{}/health", config.daemon_base_url());
    let health_route = client.get(health_url).send();
    let mut watcher_restarts_total = 0u64;
    match health_route {
        Ok(resp) => {
            let status = resp.status();
            match resp.json::<Value>() {
                Ok(body) => {
                    watcher_restarts_total = body
                        .get("watcher_restarts_total")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    println!(
                        "route /health: {} (watcher_restarts_total={})",
                        status, watcher_restarts_total
                    );
                }
                Err(_) => println!("route /health: {}", status),
            }
        }
        Err(err) => println!("route /health: error ({err})"),
    }

    let mut daemon_status: Option<StatusResponse> = None;
    match fetch_status_snapshot(&config.daemon_base_url(), &repo_root_str) {
        Ok(status) => {
            println!(
                "route /status: ok (tracked_files={} indexed_chunks={} embedded_chunks={} missing_embeddings={} invalid_embeddings={} update_retries={} update_failures={} updates_noop={} updates_applied={} watch_events_seen={} watch_events_accepted={} watch_events_dropped={} index_state={} index_job_state={} index_terminal_outcome={})",
                status.tracked_files,
                status.indexed_chunks,
                status.embedded_chunks,
                status.missing_embeddings,
                status.invalid_embeddings,
                status.update_retries,
                status.update_failures,
                status.updates_noop,
                status.updates_applied,
                status.watch_events_seen,
                status.watch_events_accepted,
                status.watch_events_dropped,
                if status.index_state.is_empty() {
                    "-"
                } else {
                    status.index_state.as_str()
                },
                if status.index_job_state.is_empty() {
                    "-"
                } else {
                    status.index_job_state.as_str()
                },
                status.index_terminal_outcome.as_deref().unwrap_or("-")
            );
            daemon_status = Some(status);
        }
        Err(err) => println!("route /status: error ({err:#})"),
    }

    match fetch_index_progress(&config.daemon_base_url(), &repo_root_str) {
        Ok(progress) => {
            let mut progress_issues = Vec::new();
            if progress.processed_files > progress.total_files {
                progress_issues.push("processed_gt_total");
            }
            if progress.active && progress.state == "ready" {
                progress_issues.push("active_ready_conflict");
            }
            if !progress.active && progress.state == "indexing" {
                progress_issues.push("inactive_indexing_conflict");
            }
            if progress.state == "failed" && progress.last_error.is_none() {
                progress_issues.push("failed_without_error");
            }
            if matches!(progress.job_state.as_str(), "queued" | "running") && !progress.active {
                progress_issues.push("job_active_conflict");
            }
            if progress.terminal_outcome.is_some() && progress.active {
                progress_issues.push("terminal_while_active");
            }
            if progress.job_state == "failed" && progress.last_error.is_none() {
                progress_issues.push("job_failed_without_error");
            }
            println!(
                "route /progress: ok (state={} phase={} active={} job_state={} terminal_outcome={} total={} processed={} sanity={})",
                if progress.state.is_empty() {
                    "-"
                } else {
                    progress.state.as_str()
                },
                if progress.phase.is_empty() {
                    "-"
                } else {
                    progress.phase.as_str()
                },
                progress.active,
                if progress.job_state.is_empty() {
                    "-"
                } else {
                    progress.job_state.as_str()
                },
                progress.terminal_outcome.as_deref().unwrap_or("-"),
                progress.total_files,
                progress.processed_files,
                if progress_issues.is_empty() {
                    HOOK_REASON_OK.to_string()
                } else {
                    progress_issues.join(",")
                }
            );
        }
        Err(err) => println!("route /progress: error ({err:#})"),
    }

    let mut drift_notes = Vec::new();
    if let Some(index_state) = &state {
        if sqlite_files_count != index_state.files.len() {
            drift_notes.push(format!(
                "sqlite_files={} state_files={}",
                sqlite_files_count,
                index_state.files.len()
            ));
        }
        if sqlite_chunks_count != index_state.chunks.len() {
            drift_notes.push(format!(
                "sqlite_chunks={} state_chunks={}",
                sqlite_chunks_count,
                index_state.chunks.len()
            ));
        }
    }
    if let Some(status) = &daemon_status {
        if status.tracked_files != sqlite_files_count {
            drift_notes.push(format!(
                "status_tracked_files={} sqlite_files={}",
                status.tracked_files, sqlite_files_count
            ));
        }
        if status.embedded_chunks != state_embedded_chunks {
            drift_notes.push(format!(
                "status_embedded_chunks={} state_embedded_chunks={}",
                status.embedded_chunks, state_embedded_chunks
            ));
        }
        if status.invalid_embeddings != state_invalid_embeddings {
            drift_notes.push(format!(
                "status_invalid_embeddings={} state_invalid_embeddings={}",
                status.invalid_embeddings, state_invalid_embeddings
            ));
        }
        if status.update_retries > 0 {
            drift_notes.push(format!("status_update_retries={}", status.update_retries));
        }
        if status.update_failures > 0 {
            drift_notes.push(format!("status_update_failures={}", status.update_failures));
        }
        if status.updates_noop > 0 {
            drift_notes.push(format!("status_updates_noop={}", status.updates_noop));
        }
        if status.updates_applied > 0 {
            drift_notes.push(format!("status_updates_applied={}", status.updates_applied));
        }
        if status
            .watch_events_accepted
            .saturating_add(status.watch_events_dropped)
            > status.watch_events_seen
        {
            drift_notes.push(format!(
                "status_watch_events_conflict=seen:{} accepted:{} dropped:{}",
                status.watch_events_seen, status.watch_events_accepted, status.watch_events_dropped
            ));
        }
        if status.watch_events_dropped > 0 {
            drift_notes.push(format!(
                "status_watch_events_dropped={}",
                status.watch_events_dropped
            ));
        }
        if matches!(status.index_job_state.as_str(), "failed" | "interrupted") {
            drift_notes.push(format!(
                "status_index_job_state={} outcome={}",
                status.index_job_state,
                status.index_terminal_outcome.as_deref().unwrap_or("-")
            ));
        }
    }
    println!(
        "index drift: sqlite_files={} sqlite_chunks={} index_progress_rows={} watcher_restarts_total={} notes={}",
        sqlite_files_count,
        sqlite_chunks_count,
        sqlite_index_progress_rows,
        watcher_restarts_total,
        if drift_notes.is_empty() {
            "none".to_string()
        } else {
            drift_notes.join("; ")
        }
    );

    if sqlite_index_progress_rows > 1 {
        println!("index progress table: unexpected row count (>1)");
    }

    if state
        .as_ref()
        .map_or(0usize, |index_state| index_state.chunks.len())
        > 0
    {
        match query_daemon_with_timeout(
            repo_root,
            config,
            "doctor deep health check retrieval",
            Some(repo_root),
            DOCTOR_QUERY_TIMEOUT_SECS,
        ) {
            Ok(response) => println!(
                "query smoke: snippets={} confidence={:.3} intent={}",
                response.snippets.len(),
                response.diagnostics.confidence,
                response.diagnostics.intent
            ),
            Err(err) => println!("query smoke: error ({err:#})"),
        }
    } else {
        println!("query smoke: skipped (index has zero chunks)");
    }
    Ok(())
}

fn percentile(values: &[f64], fraction: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let last_idx = sorted.len().saturating_sub(1);
    let idx = ((last_idx as f64) * fraction.clamp(0.0, 1.0)).round() as usize;
    sorted[idx.min(last_idx)]
}

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.iter().sum::<f64>() / values.len() as f64
}

fn cmd_hook_user_prompt_submit() -> Result<()> {
    let hook_started = Instant::now();
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let parsed: UserPromptSubmitInput = match serde_json::from_str(&input) {
        Ok(v) => v,
        Err(_) => {
            emit_hook_response(UserPromptSubmitOutput::allow_with_context(String::new()))?;
            return Ok(());
        }
    };

    let cwd = PathBuf::from(&parsed.common.cwd);
    let session_id = parsed.common.session_id.clone();
    let repo_root = match config::find_repo_root(&cwd) {
        Ok(path) => path,
        Err(_) => {
            emit_hook_response(UserPromptSubmitOutput::allow_with_context(String::new()))?;
            return Ok(());
        }
    };
    let config = config::load_or_default(&repo_root)?;
    let directives = parse_prompt_directives(&parsed.prompt);
    let sanitized_prompt = sanitize_prompt_for_query(&parsed.prompt);
    log_hook_event(&repo_root, &config, || {
        json!({
            "event":"UserPromptSubmit",
            "phase":"input",
            "ts_unix_ms": now_unix_ms(),
            "session_id": session_id.clone(),
            "cwd": parsed.common.cwd,
            "permission_mode": parsed.common.permission_mode,
            "prompt_chars": parsed.prompt.len(),
            "prompt_excerpt": excerpt(&parsed.prompt, &config),
            "sanitized_prompt_chars": sanitized_prompt.len(),
            "force_skip": directives.force_skip,
            "force_inject": directives.force_inject,
        })
    });

    if directives.force_skip {
        let diagnostics = QueryDiagnostics {
            intent: "forced".to_string(),
            confidence: 1.0,
            top_score: 0.0,
            margin: 0.0,
            signals: vec!["@nobudi".to_string()],
            recommended_injection: false,
            skip_reason: Some(SKIP_REASON_FORCED_SKIP.to_string()),
        };
        log_hook_event(&repo_root, &config, || {
            json!({
                "event":"UserPromptSubmit",
                "phase":"output",
                "ts_unix_ms": now_unix_ms(),
                "session_id": session_id.clone(),
                "latency_ms": hook_started.elapsed().as_millis(),
                "success": true,
                "reason": SKIP_REASON_FORCED_SKIP,
                "context_chars": 0,
                "context_excerpt": "",
                "retrieval_intent": diagnostics.intent,
                "retrieval_confidence": diagnostics.confidence,
                "recommended_injection": diagnostics.recommended_injection,
                "skip_reason": diagnostics.skip_reason,
            })
        });
        emit_hook_response(UserPromptSubmitOutput::allow_with_context(String::new()))?;
        return Ok(());
    }

    if ensure_daemon_running(&repo_root, &config).is_err() {
        log_hook_event(&repo_root, &config, || {
            json!({
                "event":"UserPromptSubmit",
                "phase":"output",
                "ts_unix_ms": now_unix_ms(),
                "session_id": session_id.clone(),
                "latency_ms": hook_started.elapsed().as_millis(),
                "success": false,
                "reason": HOOK_REASON_DAEMON_UNAVAILABLE,
                "context_chars": 0,
                "context_excerpt": "",
            })
        });
        emit_hook_response(UserPromptSubmitOutput::allow_with_context(String::new()))?;
        return Ok(());
    }

    let mut diagnostics = QueryDiagnostics::default();
    let mut total_candidates = 0usize;
    let mut snippets_count = 0usize;
    let mut query_timing: Option<HashMap<String, u64>> = None;
    let mut snippet_refs: Vec<budi_core::rpc::SnippetRef> = Vec::new();
    let (context, success, reason, error_detail) =
        match query_daemon_for_hook_context(&repo_root, &config, &sanitized_prompt, Some(&cwd), Some(&session_id)) {
            Ok(response) => {
                total_candidates = response.total_candidates;
                snippets_count = response.snippets.len();
                query_timing = response.timing_ms;
                snippet_refs = response.snippet_refs;
                diagnostics = response.diagnostics;
                let skip_reason = evaluate_context_skip(&config, &directives, &diagnostics);
                if let Some(skip_reason) = skip_reason {
                    let runtime_guard_context = if diagnostics.intent == "runtime-config" {
                        build_runtime_guard_context(&response.snippets)
                    } else {
                        String::new()
                    };
                    if runtime_guard_context.is_empty() {
                        (
                            String::new(),
                            true,
                            format_skip_hook_reason(&skip_reason),
                            String::new(),
                        )
                    } else {
                        (
                            runtime_guard_context,
                            true,
                            HOOK_REASON_OK.to_string(),
                            String::new(),
                        )
                    }
                } else {
                    (
                        response.context,
                        true,
                        HOOK_REASON_OK.to_string(),
                        String::new(),
                    )
                }
            }
            Err(err) => {
                let reason = classify_query_error(&err).as_str().to_string();
                (String::new(), false, reason, err.to_string())
            }
        };
    log_hook_event(&repo_root, &config, || {
        let mut payload = json!({
            "event":"UserPromptSubmit",
            "phase":"output",
            "ts_unix_ms": now_unix_ms(),
            "session_id": session_id.clone(),
            "latency_ms": hook_started.elapsed().as_millis(),
            "success": success,
            "reason": reason,
            "context_chars": context.len(),
            "context_excerpt": excerpt(&context, &config),
            "error_detail": excerpt(&error_detail, &config),
        });
        if success && let Some(obj) = payload.as_object_mut() {
            obj.insert("total_candidates".to_string(), json!(total_candidates));
            obj.insert("snippets_count".to_string(), json!(snippets_count));
            obj.insert(
                "retrieval_intent".to_string(),
                json!(diagnostics.intent.clone()),
            );
            obj.insert(
                "retrieval_confidence".to_string(),
                json!(diagnostics.confidence),
            );
            obj.insert(
                "retrieval_top_score".to_string(),
                json!(diagnostics.top_score),
            );
            obj.insert("retrieval_margin".to_string(), json!(diagnostics.margin));
            obj.insert(
                "retrieval_signals_count".to_string(),
                json!(diagnostics.signals.len()),
            );
            obj.insert(
                "recommended_injection".to_string(),
                json!(diagnostics.recommended_injection),
            );
            obj.insert(
                "skip_reason".to_string(),
                json!(diagnostics.skip_reason.clone()),
            );
            if let Some(ref timing) = query_timing {
                obj.insert(
                    "timing".to_string(),
                    serde_json::to_value(timing).unwrap_or_default(),
                );
            }
            if !snippet_refs.is_empty() {
                obj.insert(
                    "snippet_refs".to_string(),
                    serde_json::to_value(&snippet_refs).unwrap_or_default(),
                );
            }
        }
        payload
    });
    emit_hook_response(UserPromptSubmitOutput::allow_with_context(context))
}

fn query_daemon_for_hook_context(
    repo_root: &Path,
    config: &BudiConfig,
    prompt: &str,
    cwd: Option<&Path>,
    session_id: Option<&str>,
) -> Result<QueryResponse> {
    let url = format!("{}/query", config.daemon_base_url());
    let client = daemon_client_with_timeout(Duration::from_secs(HOOK_QUERY_TIMEOUT_SECS));
    match send_query_request(&client, &url, repo_root, prompt, cwd, None, session_id) {
        Ok(response) => Ok(response),
        Err(initial_err) => {
            let reason = classify_query_error(&initial_err);
            if !should_retry_hook_query(reason) {
                return Err(initial_err);
            }
            let _ = ensure_daemon_running(repo_root, config);
            let retry_client =
                daemon_client_with_timeout(Duration::from_secs(HOOK_QUERY_RETRY_TIMEOUT_SECS));
            send_query_request(&retry_client, &url, repo_root, prompt, cwd, None, session_id)
                .with_context(|| format!("hook-retry-after-{}", reason.as_str()))
        }
    }
}

fn cmd_hook_post_tool_use() -> Result<()> {
    let hook_started = Instant::now();
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let parsed: PostToolUseInput = match serde_json::from_str(&input) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    let tool_name = parsed.tool_name.clone();
    let cwd_str = parsed.common.cwd.clone();
    let session_id = parsed.common.session_id.clone();

    let is_write_edit = tool_name == "Write" || tool_name == "Edit";
    let is_read = tool_name == "Read" || tool_name == "Glob";
    if !is_write_edit && !is_read {
        return Ok(());
    }

    let file_path = parsed
        .tool_input
        .get("file_path")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if file_path.is_empty() {
        return Ok(());
    }

    let cwd = PathBuf::from(&cwd_str);
    let Ok(repo_root) = config::find_repo_root(&cwd) else {
        return Ok(());
    };
    let Ok(config) = config::load_or_default(&repo_root) else {
        return Ok(());
    };
    log_hook_event(&repo_root, &config, || {
        json!({
            "event":"PostToolUse",
            "phase":"input",
            "ts_unix_ms": now_unix_ms(),
            "session_id": session_id.clone(),
            "tool_name": tool_name,
            "file_path": file_path.clone(),
            "cwd": cwd_str,
        })
    });
    if ensure_daemon_running(&repo_root, &config).is_err() {
        log_hook_event(&repo_root, &config, || {
            json!({
                "event":"PostToolUse",
                "phase":"output",
                "ts_unix_ms": now_unix_ms(),
                "session_id": session_id.clone(),
                "latency_ms": hook_started.elapsed().as_millis(),
                "success": false,
                "reason": HOOK_REASON_DAEMON_UNAVAILABLE,
            })
        });
        return Ok(());
    }

    if is_read {
        // PostToolUse/Read: prefetch graph neighbors for the file Claude just read.
        let client = daemon_client_with_timeout(Duration::from_secs(HOOK_QUERY_TIMEOUT_SECS));
        let url = format!("{}/prefetch-neighbors", config.daemon_base_url());
        if let Ok(resp) = client
            .post(&url)
            .json(&serde_json::json!({
                "repo_root": repo_root.display().to_string(),
                "file_path": file_path,
                "session_id": session_id,
                "limit": 5,
            }))
            .send()
        {
            if let Ok(prefetch) = resp.json::<budi_core::rpc::PrefetchResponse>() {
                if !prefetch.context.is_empty() {
                    println!(
                        "{}",
                        serde_json::to_string(&AsyncSystemMessageOutput {
                            system_message: prefetch.context,
                        })?
                    );
                }
            }
        }
        return Ok(());
    }

    // Write/Edit: trigger incremental index update.
    let client = daemon_client_with_timeout(Duration::from_secs(UPDATE_TIMEOUT_SECS));
    let url = format!("{}/update", config.daemon_base_url());
    let update_result = client
        .post(url)
        .json(&UpdateRequest {
            repo_root: repo_root.display().to_string(),
            changed_files: vec![file_path.clone()],
        })
        .send();
    let mut update_success = false;
    let mut indexed_chunks = 0usize;
    let mut changed_files = 0usize;
    let (update_reason, update_error_detail) = match update_result {
        Ok(response) => match response.error_for_status() {
            Ok(ok_resp) => match ok_resp.json::<IndexResponse>() {
                Ok(parsed_resp) => {
                    update_success = true;
                    indexed_chunks = parsed_resp.indexed_chunks;
                    changed_files = parsed_resp.changed_files;
                    let msg = format!(
                        "budi indexed {} changed file(s), total chunks={}",
                        parsed_resp.changed_files, parsed_resp.indexed_chunks
                    );
                    println!(
                        "{}",
                        serde_json::to_string(&AsyncSystemMessageOutput {
                            system_message: msg
                        })?
                    );
                    (HOOK_REASON_OK.to_string(), String::new())
                }
                Err(err) => (
                    HOOK_REASON_RESPONSE_PARSE_ERROR.to_string(),
                    err.to_string(),
                ),
            },
            Err(err) => (
                classify_update_error(&err).as_str().to_string(),
                err.to_string(),
            ),
        },
        Err(err) => (
            classify_update_error(&err).as_str().to_string(),
            err.to_string(),
        ),
    };
    log_hook_event(&repo_root, &config, || {
        json!({
            "event":"PostToolUse",
            "phase":"output",
            "ts_unix_ms": now_unix_ms(),
            "session_id": session_id.clone(),
            "latency_ms": hook_started.elapsed().as_millis(),
            "success": update_success,
            "reason": update_reason,
            "error_detail": excerpt(&update_error_detail, &config),
            "indexed_chunks": indexed_chunks,
            "changed_files": changed_files,
        })
    });
    Ok(())
}

fn cmd_hook_session_start() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let Ok(repo_root) = config::find_repo_root(&cwd) else {
        return Ok(());
    };
    let mut message = String::new();
    if let Some(map) = budi_core::project_map::read_project_map(&repo_root) {
        // Cap the map at ~3000 chars to stay within Claude's context budget.
        let truncated: String = map.chars().take(3000).collect();
        message.push_str(&truncated);
    }
    // Phase J+M2: append recently-relevant files with anchor lines from prior sessions.
    let affinity_files = read_session_affinity(&repo_root, 5);
    if !affinity_files.is_empty() {
        message.push_str("\n\n## Recently Relevant Files\n(files active in prior sessions, for reference)\n");
        for (path, anchors) in &affinity_files {
            if anchors.is_empty() {
                message.push_str(&format!("- {}\n", path));
            } else {
                message.push_str(&format!("- {} — {}\n", path, anchors.join("; ")));
            }
        }
    }
    if !message.is_empty() {
        println!(
            "{}",
            serde_json::to_string(&AsyncSystemMessageOutput {
                system_message: message,
            })?
        );
    }
    Ok(())
}

fn cmd_hook_session_end() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let Ok(repo_root) = config::find_repo_root(&cwd) else {
        return Ok(());
    };
    let Ok(config) = config::load_or_default(&repo_root) else {
        return Ok(());
    };
    if !config.debug_io {
        return Ok(());
    }

    // Read session_id from environment (Claude Code sets CLAUDE_SESSION_ID for Stop hooks).
    let session_id = std::env::var("CLAUDE_SESSION_ID").ok();

    let Ok(log_path) = config::hook_log_path(&repo_root) else {
        return Ok(());
    };
    let Ok(raw) = std::fs::read_to_string(&log_path) else {
        return Ok(());
    };

    // Collect output events for this session.
    let mut total_injected = 0u32;
    let mut total_prompts = 0u32;
    let mut first_ts: Option<u64> = None;
    let mut last_ts: Option<u64> = None;
    let mut file_counts: HashMap<String, u32> = HashMap::new();

    for line in raw.lines() {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        // Filter by session_id if available.
        if let Some(ref sid) = session_id {
            if val.get("session_id").and_then(Value::as_str) != Some(sid.as_str()) {
                continue;
            }
        }
        if val.get("phase").and_then(Value::as_str) != Some("output") {
            continue;
        }
        if val.get("event").and_then(Value::as_str) != Some("UserPromptSubmit") {
            continue;
        }
        total_prompts += 1;
        if let Some(ts) = val.get("ts_unix_ms").and_then(Value::as_u64) {
            if first_ts.is_none() || ts < first_ts.unwrap() {
                first_ts = Some(ts);
            }
            if last_ts.is_none() || ts > last_ts.unwrap() {
                last_ts = Some(ts);
            }
        }
        if val.get("recommended_injection").and_then(Value::as_bool).unwrap_or(false) {
            total_injected += 1;
        }
        if let Some(refs) = val.get("snippet_refs").and_then(Value::as_array) {
            for r in refs {
                if let Some(path) = r.get("path").and_then(Value::as_str) {
                    *file_counts.entry(path.to_string()).or_insert(0) += 1;
                }
            }
        }
    }

    if total_prompts == 0 {
        return Ok(());
    }

    let duration_secs = match (first_ts, last_ts) {
        (Some(a), Some(b)) => (b.saturating_sub(a) / 1000) as u32,
        _ => 0,
    };

    let mut top_files: Vec<(String, u32)> = file_counts.into_iter().collect();
    top_files.sort_by(|a, b| b.1.cmp(&a.1));
    top_files.truncate(5);

    log_hook_event(&repo_root, &config, || {
        json!({
            "event": "SessionEnd",
            "ts_unix_ms": now_unix_ms(),
            "session_id": session_id,
            "duration_secs": duration_secs,
            "total_prompts": total_prompts,
            "total_injected": total_injected,
            "injection_rate": if total_prompts > 0 { total_injected as f32 / total_prompts as f32 } else { 0.0 },
            "top_files": top_files.iter().map(|(p, n)| json!({"path": p, "count": n})).collect::<Vec<_>>(),
        })
    });
    Ok(())
}

/// Phase J+M2: Read session-affinity.json, return top N entries (path, anchors) sorted by recency.
/// Supports both new format (AffinityEntry with ts+anchors) and old flat format (ts only).
fn read_session_affinity(repo_root: &std::path::Path, top_n: usize) -> Vec<(String, Vec<String>)> {
    let Ok(paths) = budi_core::config::repo_paths(repo_root) else {
        return Vec::new();
    };
    let affinity_path = paths.data_dir.join("session-affinity.json");
    let Ok(raw) = std::fs::read_to_string(&affinity_path) else {
        return Vec::new();
    };
    #[derive(serde::Deserialize, Default)]
    struct Entry {
        ts: u64,
        #[serde(default)]
        anchors: Vec<String>,
    }
    // Try new format first; fall back to old flat HashMap<String, u64>.
    let map: std::collections::HashMap<String, Entry> = serde_json::from_str(&raw)
        .or_else(|_| {
            let old: std::collections::HashMap<String, u64> = serde_json::from_str(&raw)?;
            Ok::<_, serde_json::Error>(
                old.into_iter()
                    .map(|(k, ts)| (k, Entry { ts, anchors: vec![] }))
                    .collect(),
            )
        })
        .unwrap_or_default();
    let mut entries: Vec<(String, Entry)> = map.into_iter().collect();
    entries.sort_by(|a, b| b.1.ts.cmp(&a.1.ts));
    entries.truncate(top_n);
    entries.into_iter().map(|(path, e)| (path, e.anchors)).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueryErrorReason {
    Timeout,
    TransportError,
    HttpError,
    Error,
}

impl QueryErrorReason {
    const fn as_str(self) -> &'static str {
        match self {
            QueryErrorReason::Timeout => HOOK_REASON_QUERY_TIMEOUT,
            QueryErrorReason::TransportError => HOOK_REASON_QUERY_TRANSPORT_ERROR,
            QueryErrorReason::HttpError => HOOK_REASON_QUERY_HTTP_ERROR,
            QueryErrorReason::Error => HOOK_REASON_QUERY_ERROR,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdateErrorReason {
    Timeout,
    ConnectError,
    HttpError,
    Failed,
}

impl UpdateErrorReason {
    const fn as_str(self) -> &'static str {
        match self {
            UpdateErrorReason::Timeout => HOOK_REASON_UPDATE_TIMEOUT,
            UpdateErrorReason::ConnectError => HOOK_REASON_UPDATE_CONNECT_ERROR,
            UpdateErrorReason::HttpError => HOOK_REASON_UPDATE_HTTP_ERROR,
            UpdateErrorReason::Failed => HOOK_REASON_UPDATE_FAILED,
        }
    }
}

fn classify_query_error(err: &anyhow::Error) -> QueryErrorReason {
    for cause in err.chain() {
        if let Some(reqwest_err) = cause.downcast_ref::<reqwest::Error>() {
            return match classify_update_error(reqwest_err) {
                UpdateErrorReason::Timeout => QueryErrorReason::Timeout,
                UpdateErrorReason::ConnectError => QueryErrorReason::TransportError,
                UpdateErrorReason::HttpError => QueryErrorReason::HttpError,
                UpdateErrorReason::Failed => QueryErrorReason::Error,
            };
        }
    }
    let message = err.to_string().to_ascii_lowercase();
    if message.contains("timed out") || message.contains("timeout") {
        return QueryErrorReason::Timeout;
    }
    if message.contains("failed to send query request")
        || message.contains("connection")
        || message.contains("connect")
    {
        return QueryErrorReason::TransportError;
    }
    if message.contains("query endpoint returned error") {
        return QueryErrorReason::HttpError;
    }
    QueryErrorReason::Error
}

const fn should_retry_hook_query(reason: QueryErrorReason) -> bool {
    matches!(
        reason,
        QueryErrorReason::Timeout | QueryErrorReason::TransportError
    )
}

fn classify_update_error(err: &reqwest::Error) -> UpdateErrorReason {
    if err.is_timeout() {
        return UpdateErrorReason::Timeout;
    }
    if err.is_connect() {
        return UpdateErrorReason::ConnectError;
    }
    if err.status().is_some() {
        return UpdateErrorReason::HttpError;
    }
    UpdateErrorReason::Failed
}

fn emit_hook_response(output: UserPromptSubmitOutput) -> Result<()> {
    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}

fn install_hooks(repo_root: &Path) -> Result<()> {
    let settings_path = repo_root.join(CLAUDE_LOCAL_SETTINGS);
    if let Some(parent) = settings_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed creating {}", parent.display()))?;
    }

    let mut settings = if settings_path.exists() {
        let raw = fs::read_to_string(&settings_path)
            .with_context(|| format!("Failed reading {}", settings_path.display()))?;
        serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    };
    if !settings.is_object() {
        settings = json!({});
    }
    if !settings.get("hooks").map(Value::is_object).unwrap_or(false) {
        settings["hooks"] = json!({});
    }

    settings["hooks"]["SessionStart"] = json!([
      {
        "hooks": [
          {
            "type": "command",
            "command": "budi hook session-start"
          }
        ]
      }
    ]);

    settings["hooks"]["UserPromptSubmit"] = json!([
      {
        "hooks": [
          {
            "type": "command",
            "command": "budi hook user-prompt-submit"
          }
        ]
      }
    ]);

    settings["hooks"]["PostToolUse"] = json!([
      {
        "matcher": "Write|Edit|Read|Glob",
        "hooks": [
          {
            "type": "command",
            "command": "budi hook post-tool-use",
            "async": true,
            "timeout": 30
          }
        ]
      }
    ]);

    settings["hooks"]["Stop"] = json!([
      {
        "hooks": [
          {
            "type": "command",
            "command": "budi hook session-end"
          }
        ]
      }
    ]);

    let raw = serde_json::to_string_pretty(&settings)?;
    fs::write(&settings_path, raw)
        .with_context(|| format!("Failed writing {}", settings_path.display()))?;
    Ok(())
}

fn resolve_repo_root(candidate: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = candidate {
        return Ok(path);
    }
    let cwd = std::env::current_dir()?;
    config::find_repo_root(&cwd)
}

fn daemon_client_with_timeout(timeout: Duration) -> Client {
    Client::builder()
        .timeout(timeout)
        .build()
        .expect("Failed to construct HTTP client")
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[derive(Debug)]
struct HookLogLockGuard {
    lock_path: PathBuf,
    _lock_file: fs::File,
}

impl Drop for HookLogLockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.lock_path);
    }
}

fn hook_log_lock_path(log_path: &Path) -> PathBuf {
    let lock_name = log_path
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| format!("{name}.lock"))
        .unwrap_or_else(|| "hook-io.jsonl.lock".to_string());
    log_path.with_file_name(lock_name)
}

fn clear_stale_hook_log_lock(lock_path: &Path) {
    let Ok(metadata) = fs::metadata(lock_path) else {
        return;
    };
    let Ok(modified) = metadata.modified() else {
        return;
    };
    let Ok(age) = SystemTime::now().duration_since(modified) else {
        return;
    };
    if age > Duration::from_secs(HOOK_LOG_LOCK_STALE_SECS) {
        let _ = fs::remove_file(lock_path);
    }
}

fn acquire_hook_log_lock(log_path: &Path) -> Option<HookLogLockGuard> {
    let lock_path = hook_log_lock_path(log_path);
    let started = Instant::now();
    loop {
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&lock_path)
        {
            Ok(lock_file) => {
                return Some(HookLogLockGuard {
                    lock_path,
                    _lock_file: lock_file,
                });
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                clear_stale_hook_log_lock(&lock_path);
                if started.elapsed() >= Duration::from_millis(HOOK_LOG_LOCK_TIMEOUT_MS) {
                    return None;
                }
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return None,
        }
    }
}

fn log_hook_event<F>(repo_root: &Path, config: &BudiConfig, build_value: F)
where
    F: FnOnce() -> Value,
{
    if !config.debug_io {
        return;
    }
    let Ok(log_path) = config::hook_log_path(repo_root) else {
        return;
    };
    if let Some(parent) = log_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let Some(_lock_guard) = acquire_hook_log_lock(&log_path) else {
        return;
    };
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&log_path) {
        let mut line = build_value();
        if let Some(obj) = line.as_object_mut() {
            obj.insert(
                "repo_root".to_string(),
                json!(repo_root.display().to_string()),
            );
        }
        if let Ok(mut serialized) = serde_json::to_vec(&line) {
            serialized.push(b'\n');
            let _ = file.write_all(&serialized);
        }
    }
}

fn daemon_health(config: &BudiConfig) -> bool {
    daemon_health_with_timeout(config, Duration::from_secs(HEALTH_TIMEOUT_SECS))
}

fn daemon_health_with_timeout(config: &BudiConfig, timeout: Duration) -> bool {
    let client = daemon_client_with_timeout(timeout);
    let url = format!("{}/health", config.daemon_base_url());
    client
        .get(url)
        .send()
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

fn ensure_daemon_running(repo_root: &Path, config: &BudiConfig) -> Result<()> {
    if daemon_health(config) {
        return Ok(());
    }

    // Avoid duplicate daemon spawns when an existing process is still booting
    // or temporarily busy and not answering /health yet.
    if daemon_port_is_listening(config) {
        if wait_for_daemon_health(
            config,
            24,
            Duration::from_millis(250),
            Duration::from_millis(250),
        ) {
            return Ok(());
        }
        if restart_unhealthy_daemon_listener(repo_root, config)? {
            return Ok(());
        }
        anyhow::bail!(
            "Daemon port is occupied but health endpoint is unavailable at {}. \
If another process is restarting budi-daemon, retry in a few seconds. \
Otherwise run `budi init` to restart the daemon.",
            config.daemon_base_url(),
        );
    }

    spawn_daemon_process(repo_root, config)?;
    if wait_for_daemon_health(
        config,
        80,
        Duration::from_millis(500),
        Duration::from_millis(150),
    ) {
        return Ok(());
    }
    anyhow::bail!(
        "Daemon failed to become healthy at {}",
        config.daemon_base_url()
    );
}

fn wait_for_daemon_health(
    config: &BudiConfig,
    retries: usize,
    request_timeout: Duration,
    sleep_interval: Duration,
) -> bool {
    for attempt in 0..retries {
        if daemon_health_with_timeout(config, request_timeout) {
            return true;
        }
        if attempt + 1 < retries {
            thread::sleep(sleep_interval);
        }
    }
    false
}

fn restart_unhealthy_daemon_listener(repo_root: &Path, config: &BudiConfig) -> Result<bool> {
    let listener_pids = daemon_listener_pids(config.daemon_port)?;
    if listener_pids.is_empty() {
        return Ok(false);
    }

    let mut killed_any = false;
    for pid in listener_pids {
        let Some(command_line) = daemon_process_command(pid) else {
            continue;
        };
        if !is_budi_daemon_command_for_port(&command_line, config.daemon_port) {
            continue;
        }
        if kill_process(pid, "-TERM")? {
            killed_any = true;
        }
    }
    if !killed_any {
        return Ok(false);
    }

    if !wait_for_port_release(config, 30, Duration::from_millis(120)) {
        for pid in daemon_listener_pids(config.daemon_port)? {
            let Some(command_line) = daemon_process_command(pid) else {
                continue;
            };
            if is_budi_daemon_command_for_port(&command_line, config.daemon_port) {
                let _ = kill_process(pid, "-KILL");
            }
        }
    }
    if daemon_port_is_listening(config) {
        return Ok(false);
    }

    spawn_daemon_process(repo_root, config)?;
    Ok(wait_for_daemon_health(
        config,
        80,
        Duration::from_millis(500),
        Duration::from_millis(150),
    ))
}

fn wait_for_port_release(config: &BudiConfig, retries: usize, sleep_interval: Duration) -> bool {
    for attempt in 0..retries {
        if !daemon_port_is_listening(config) {
            return true;
        }
        if attempt + 1 < retries {
            thread::sleep(sleep_interval);
        }
    }
    !daemon_port_is_listening(config)
}

fn daemon_listener_pids(port: u16) -> Result<Vec<u32>> {
    let output = match Command::new("lsof")
        .arg("-nP")
        .arg(format!("-tiTCP:{port}"))
        .arg("-sTCP:LISTEN")
        .output()
    {
        Ok(output) => output,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).context("Failed to inspect listener pids via lsof"),
    };
    if !output.status.success() {
        return Ok(Vec::new());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .collect())
}

fn daemon_process_command(pid: u32) -> Option<String> {
    let output = match Command::new("ps")
        .arg("-p")
        .arg(pid.to_string())
        .arg("-o")
        .arg("command=")
        .output()
    {
        Ok(output) => output,
        Err(_) => return None,
    };
    if !output.status.success() {
        return None;
    }
    let command = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if command.is_empty() {
        None
    } else {
        Some(command)
    }
}

fn is_budi_daemon_command_for_port(command: &str, port: u16) -> bool {
    let spaced_port_flag = format!("--port {port}");
    let inline_port_flag = format!("--port={port}");
    command.contains("budi-daemon")
        && command.contains("serve")
        && (command.contains(&spaced_port_flag) || command.contains(&inline_port_flag))
}

fn kill_process(pid: u32, signal: &str) -> Result<bool> {
    let status = match Command::new("kill")
        .arg(signal)
        .arg(pid.to_string())
        .status()
    {
        Ok(status) => status,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("Failed to send {signal} to pid {pid} via kill"));
        }
    };
    Ok(status.success())
}

fn daemon_port_is_listening(config: &BudiConfig) -> bool {
    let endpoint = format!("{}:{}", config.daemon_host, config.daemon_port);
    let Ok(addrs) = endpoint.to_socket_addrs() else {
        return false;
    };
    for addr in addrs {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(250)).is_ok() {
            return true;
        }
    }
    false
}

fn spawn_daemon_process(repo_root: &Path, config: &BudiConfig) -> Result<()> {
    let daemon_bin = resolve_daemon_binary()?;
    let log_path = config::daemon_log_path(repo_root)?;
    let fastembed_cache_dir = config::fastembed_cache_dir()?;
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::create_dir_all(&fastembed_cache_dir)?;
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("Failed opening {}", log_path.display()))?;
    let stderr = stdout.try_clone()?;

    Command::new(daemon_bin)
        .arg("serve")
        .arg("--host")
        .arg(&config.daemon_host)
        .arg("--port")
        .arg(config.daemon_port.to_string())
        .env("FASTEMBED_CACHE_DIR", &fastembed_cache_dir)
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .stdin(Stdio::null())
        .spawn()
        .with_context(|| "Failed to spawn budi-daemon process".to_string())?;
    Ok(())
}

fn resolve_daemon_binary() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("BUDI_DAEMON_BIN") {
        return Ok(PathBuf::from(path));
    }
    let current = std::env::current_exe().context("Failed to resolve current executable")?;
    if let Some(parent) = current.parent() {
        let sibling = parent.join("budi-daemon");
        if sibling.exists() {
            return Ok(sibling);
        }
    }
    Ok(PathBuf::from("budi-daemon"))
}

fn query_daemon_with_timeout(
    repo_root: &Path,
    config: &BudiConfig,
    prompt: &str,
    cwd: Option<&Path>,
    timeout_secs: u64,
) -> Result<QueryResponse> {
    query_daemon_with_timeout_mode(repo_root, config, prompt, cwd, timeout_secs, None)
}

fn query_daemon_with_timeout_mode(
    repo_root: &Path,
    config: &BudiConfig,
    prompt: &str,
    cwd: Option<&Path>,
    timeout_secs: u64,
    retrieval_mode: Option<&str>,
) -> Result<QueryResponse> {
    let url = format!("{}/query", config.daemon_base_url());
    let client = daemon_client_with_timeout(Duration::from_secs(timeout_secs));
    send_query_request(&client, &url, repo_root, prompt, cwd, retrieval_mode, None)
}

fn send_query_request(
    client: &Client,
    url: &str,
    repo_root: &Path,
    prompt: &str,
    cwd: Option<&Path>,
    retrieval_mode: Option<&str>,
    session_id: Option<&str>,
) -> Result<QueryResponse> {
    let response: QueryResponse = client
        .post(url)
        .json(&QueryRequest {
            repo_root: repo_root.display().to_string(),
            prompt: prompt.to_string(),
            cwd: cwd.map(|p| p.display().to_string()),
            retrieval_mode: retrieval_mode.map(str::to_string),
            session_id: session_id.map(str::to_string),
        })
        .send()
        .context("Failed to send query request")?
        .error_for_status()
        .context("Query endpoint returned error")?
        .json()
        .context("Invalid query response JSON")?;
    Ok(response)
}

fn run_index_with_progress(
    repo_root: &Path,
    config: &BudiConfig,
    hard: bool,
    ignore_patterns: &[String],
    include_extensions: &[String],
) -> Result<IndexResponse> {
    let base_url = config.daemon_base_url();
    let repo_root_str = repo_root.display().to_string();
    send_index_request(
        &base_url,
        &repo_root_str,
        hard,
        ignore_patterns,
        include_extensions,
    )?;

    let started = Instant::now();
    let mut had_progress_line = false;
    let mut warned_missing_progress = false;
    let mut previous_line_len = 0usize;
    loop {
        if started.elapsed() > Duration::from_secs(INDEX_TIMEOUT_SECS) {
            if had_progress_line {
                eprintln!();
            }
            anyhow::bail!("Timed out while waiting for async index job to finish");
        }
        let elapsed = started.elapsed().as_secs_f32();
        let snapshot = match fetch_index_progress(&base_url, &repo_root_str) {
            Ok(snapshot) => snapshot,
            Err(err) => {
                if !warned_missing_progress {
                    eprintln!();
                    eprintln!(
                        "warning: live progress endpoint unavailable ({err}). \
restart daemon (`budi init`) to enable per-file progress."
                    );
                    warned_missing_progress = true;
                }
                let line = format!("Indexing... preparing ({elapsed:.1}s elapsed)");
                render_progress_to_stderr(&line, &mut previous_line_len);
                had_progress_line = true;
                std::thread::sleep(Duration::from_millis(220));
                continue;
            }
        };
        let line = render_progress_line(&snapshot, elapsed);
        render_progress_to_stderr(&line, &mut previous_line_len);
        had_progress_line = true;
        if is_terminal_job_progress(&snapshot) {
            if had_progress_line {
                eprintln!();
            }
            return build_index_response_from_progress(&base_url, &repo_root_str, snapshot);
        }
        std::thread::sleep(Duration::from_millis(220));
    }
}

fn is_terminal_job_progress(progress: &IndexProgressResponse) -> bool {
    if progress.terminal_outcome.is_some() {
        return true;
    }
    if matches!(
        progress.job_state.as_str(),
        "succeeded" | "failed" | "interrupted"
    ) {
        return true;
    }
    !progress.active && matches!(progress.state.as_str(), "ready" | "failed" | "interrupted")
}

fn build_index_response_from_progress(
    base_url: &str,
    repo_root: &str,
    progress: IndexProgressResponse,
) -> Result<IndexResponse> {
    if matches!(progress.job_state.as_str(), "failed" | "interrupted")
        || matches!(progress.state.as_str(), "failed" | "interrupted")
    {
        let message = progress
            .last_error
            .unwrap_or_else(|| "index job failed".to_string());
        anyhow::bail!("{message}");
    }
    let status = fetch_status_snapshot(base_url, repo_root)
        .context("Failed to fetch final status after index job completion")?;
    let index_status = progress
        .terminal_outcome
        .clone()
        .unwrap_or_else(|| "completed".to_string());
    Ok(IndexResponse {
        indexed_files: status.tracked_files,
        indexed_chunks: status.indexed_chunks,
        embedded_chunks: status.embedded_chunks,
        missing_embeddings: status.missing_embeddings,
        repaired_embeddings: 0,
        invalid_embeddings: status.invalid_embeddings,
        changed_files: progress.changed_files,
        index_status,
        job_id: progress.job_id,
        job_state: progress.job_state,
        terminal_outcome: progress.terminal_outcome,
    })
}

fn render_progress_to_stderr(line: &str, previous_line_len: &mut usize) {
    let line_len = line.chars().count();
    if *previous_line_len > line_len {
        let clear_tail = " ".repeat(*previous_line_len - line_len);
        eprint!("\r{line}{clear_tail}");
    } else {
        eprint!("\r{line}");
    }
    let _ = io::stderr().flush();
    *previous_line_len = line_len;
}

fn send_index_request(
    base_url: &str,
    repo_root: &str,
    hard: bool,
    ignore_patterns: &[String],
    include_extensions: &[String],
) -> Result<IndexResponse> {
    let client = daemon_client_with_timeout(Duration::from_secs(INDEX_TIMEOUT_SECS));
    let url = format!("{base_url}/index");
    let response: IndexResponse = client
        .post(url)
        .json(&IndexRequest {
            repo_root: repo_root.to_string(),
            hard,
            include_extensions: include_extensions.to_vec(),
            ignore_patterns: ignore_patterns.to_vec(),
        })
        .send()
        .context("Failed sending index request")?
        .error_for_status()
        .context("Index request failed")?
        .json()
        .context("Invalid index response JSON")?;
    Ok(response)
}

fn fetch_status_snapshot(base_url: &str, repo_root: &str) -> Result<StatusResponse> {
    let client = daemon_client_with_timeout(Duration::from_secs(STATUS_TIMEOUT_SECS));
    let url = format!("{base_url}/status");
    let response: StatusResponse = client
        .post(url)
        .json(&StatusRequest {
            repo_root: repo_root.to_string(),
        })
        .send()
        .context("Failed requesting daemon status")?
        .error_for_status()
        .context("Status endpoint returned error")?
        .json()
        .context("Invalid status response JSON")?;
    Ok(response)
}

fn fetch_index_progress(base_url: &str, repo_root: &str) -> Result<IndexProgressResponse> {
    let client = daemon_client_with_timeout(Duration::from_secs(HEALTH_TIMEOUT_SECS));
    let url = format!("{base_url}/progress");
    let response: IndexProgressResponse = client
        .post(url)
        .json(&IndexProgressRequest {
            repo_root: repo_root.to_string(),
        })
        .send()
        .context("Failed requesting index progress")?
        .error_for_status()
        .context("Progress endpoint returned error")?
        .json()
        .context("Invalid progress response JSON")?;
    Ok(response)
}

fn render_progress_line(progress: &IndexProgressResponse, elapsed_secs: f32) -> String {
    if let Some(error) = &progress.last_error {
        return format!("Indexing failed ({elapsed_secs:.1}s): {error}");
    }
    if let Some(outcome) = &progress.terminal_outcome {
        return format!("Index {outcome} ({elapsed_secs:.1}s elapsed)");
    }
    let phase = if progress.phase.is_empty() {
        if progress.state.is_empty() {
            "working"
        } else {
            progress.state.as_str()
        }
    } else {
        progress.phase.as_str()
    };
    if progress.job_state == "queued" {
        return format!("Indexing... queued ({elapsed_secs:.1}s elapsed)");
    }
    if progress.total_files == 0 {
        if progress.active || matches!(progress.job_state.as_str(), "running") {
            return format!("Indexing... {phase} ({elapsed_secs:.1}s elapsed)");
        }
        if progress.state == "ready" {
            return format!("Index ready ({elapsed_secs:.1}s elapsed)");
        }
        return format!("Indexing... waiting to start ({elapsed_secs:.1}s elapsed)");
    }
    let current = progress.current_file.as_deref().unwrap_or("-");
    let processed = progress.processed_files.min(progress.total_files);
    format!(
        "Indexing... {processed}/{total} files (changed {changed}) phase: {phase} current: {current} [{elapsed_secs:.1}s]",
        total = progress.total_files,
        changed = progress.changed_files
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_eval_report(fixtures_path: &str, retrieval_mode: &str) -> RetrievalEvalReport {
        RetrievalEvalReport {
            repo_root: "/tmp/repo".to_string(),
            fixtures_path: fixtures_path.to_string(),
            retrieval_mode: retrieval_mode.to_string(),
            limit: 8,
            total_cases: 1,
            scored_cases: 1,
            cases_with_errors: 0,
            metrics: retrieval_eval::RetrievalEvalMetrics {
                cases: 1,
                hit_at_1: 0.0,
                hit_at_3: 0.0,
                hit_at_5: 0.0,
                mrr: 0.0,
                precision_at_1: 0.0,
                precision_at_3: 0.0,
                precision_at_5: 0.0,
                recall_at_1: 0.0,
                recall_at_3: 0.0,
                recall_at_5: 0.0,
                f1_at_1: 0.0,
                f1_at_3: 0.0,
                f1_at_5: 0.0,
            },
            per_intent_metrics: HashMap::new(),
            results: Vec::new(),
        }
    }

    #[test]
    fn parses_prompt_directives() {
        let d = parse_prompt_directives("please @nobudi for this prompt");
        assert!(d.force_skip);
        assert!(!d.force_inject);

        let d = parse_prompt_directives("please @forcebudi thanks");
        assert!(!d.force_skip);
        assert!(d.force_inject);

        let d = parse_prompt_directives("@nobudi @forcebudi");
        assert!(!d.force_skip);
        assert!(d.force_inject);
    }

    #[test]
    fn skips_non_code_when_enabled() {
        let config = BudiConfig {
            smart_skip_enabled: true,
            skip_non_code_prompts: true,
            ..BudiConfig::default()
        };
        let diagnostics = QueryDiagnostics {
            intent: "non-code".to_string(),
            confidence: 0.92,
            top_score: 0.7,
            margin: 0.3,
            signals: vec!["semantic-hit".to_string()],
            recommended_injection: true,
            skip_reason: None,
        };
        let directives = PromptDirectives::default();
        let skip = evaluate_context_skip(&config, &directives, &diagnostics);
        assert_eq!(
            skip.as_deref(),
            Some(budi_core::reason_codes::SKIP_REASON_NON_CODE_INTENT)
        );
    }

    #[test]
    fn force_inject_bypasses_skip() {
        let config = BudiConfig {
            smart_skip_enabled: true,
            skip_non_code_prompts: true,
            ..BudiConfig::default()
        };
        let diagnostics = QueryDiagnostics {
            intent: "non-code".to_string(),
            confidence: 0.1,
            top_score: 0.1,
            margin: 0.0,
            signals: vec![],
            recommended_injection: false,
            skip_reason: Some(budi_core::reason_codes::SKIP_REASON_LOW_CONFIDENCE.to_string()),
        };
        let directives = PromptDirectives {
            force_skip: false,
            force_inject: true,
        };
        let skip = evaluate_context_skip(&config, &directives, &diagnostics);
        assert!(skip.is_none());
    }

    #[test]
    fn force_skip_wins_in_evaluate_context_skip() {
        let config = BudiConfig {
            smart_skip_enabled: true,
            skip_non_code_prompts: false,
            ..BudiConfig::default()
        };
        let diagnostics = QueryDiagnostics {
            intent: "code-navigation".to_string(),
            confidence: 0.95,
            top_score: 0.8,
            margin: 0.2,
            signals: vec!["path-hit".to_string()],
            recommended_injection: true,
            skip_reason: None,
        };
        let directives = PromptDirectives {
            force_skip: true,
            force_inject: false,
        };
        let skip = evaluate_context_skip(&config, &directives, &diagnostics);
        assert_eq!(
            skip.as_deref(),
            Some(budi_core::reason_codes::SKIP_REASON_FORCED_SKIP)
        );
    }

    #[test]
    fn sanitize_removes_directive_tokens() {
        let prompt = "Please @forcebudi map auth flow and @nobudi ignore this token";
        let sanitized = sanitize_prompt_for_query(prompt);
        assert!(!sanitized.contains("@forcebudi"));
        assert!(!sanitized.contains("@nobudi"));
        assert!(sanitized.contains("map auth flow"));
    }

    #[test]
    fn runtime_guard_context_filters_non_prod_and_weak_signals() {
        let snippets = vec![
            QueryResultItem {
                path: "src/flask/config.py".to_string(),
                start_line: 1,
                end_line: 20,
                score: 1.0,
                reasons: vec!["runtime-env-api-hit".to_string()],
                channel_scores: budi_core::rpc::QueryChannelScores::default(),
                text: "from_envvar".to_string(),
                slm_relevance_note: None,
            },
            QueryResultItem {
                path: "tests/test_config.py".to_string(),
                start_line: 1,
                end_line: 20,
                score: 0.9,
                reasons: vec!["runtime-config-support-hit".to_string()],
                channel_scores: budi_core::rpc::QueryChannelScores::default(),
                text: "test".to_string(),
                slm_relevance_note: None,
            },
            QueryResultItem {
                path: "examples/tutorial/flaskr/__init__.py".to_string(),
                start_line: 1,
                end_line: 20,
                score: 0.8,
                reasons: vec!["runtime-config-path-hit".to_string()],
                channel_scores: budi_core::rpc::QueryChannelScores::default(),
                text: "example".to_string(),
                slm_relevance_note: None,
            },
            QueryResultItem {
                path: "src/flask/cli.py".to_string(),
                start_line: 1,
                end_line: 20,
                score: 0.7,
                reasons: vec!["runtime-config-support-hit".to_string()],
                channel_scores: budi_core::rpc::QueryChannelScores::default(),
                text: "load_dotenv".to_string(),
                slm_relevance_note: None,
            },
            QueryResultItem {
                path: "src/flask/app.py".to_string(),
                start_line: 1,
                end_line: 20,
                score: 0.6,
                reasons: vec!["semantic-hit".to_string()],
                channel_scores: budi_core::rpc::QueryChannelScores::default(),
                text: "app".to_string(),
                slm_relevance_note: None,
            },
        ];

        let context = build_runtime_guard_context(&snippets);
        assert!(context.contains("[budi runtime guard]"));
        assert!(context.contains("- src/flask/config.py"));
        assert!(context.contains("- src/flask/cli.py"));
        assert!(!context.contains("tests/test_config.py"));
        assert!(!context.contains("examples/tutorial/flaskr/__init__.py"));
        assert!(!context.contains("- src/flask/app.py"));
    }

    #[test]
    fn runtime_guard_context_empty_without_runtime_signals() {
        let snippets = vec![QueryResultItem {
            path: "src/flask/app.py".to_string(),
            start_line: 1,
            end_line: 20,
            score: 1.0,
            reasons: vec!["semantic-hit".to_string()],
            channel_scores: budi_core::rpc::QueryChannelScores::default(),
            text: "app".to_string(),
            slm_relevance_note: None,
        }];
        let context = build_runtime_guard_context(&snippets);
        assert!(context.is_empty());
    }

    #[test]
    fn excerpt_omits_text_when_max_chars_is_zero() {
        let config = BudiConfig {
            debug_io: true,
            debug_io_full_text: false,
            debug_io_max_chars: 0,
            ..BudiConfig::default()
        };
        assert_eq!(excerpt("sensitive prompt text", &config), "");
    }

    #[test]
    fn eval_regression_gate_detects_metric_drop() {
        let mut baseline = make_eval_report("/tmp/fixtures.json", "hybrid");
        baseline.metrics.hit_at_3 = 0.80;
        baseline.metrics.mrr = 0.70;
        baseline.metrics.f1_at_3 = 0.65;

        let mut current = make_eval_report("/tmp/fixtures.json", "hybrid");
        current.metrics.hit_at_3 = 0.79;
        current.metrics.mrr = 0.69;
        current.metrics.f1_at_3 = 0.50;

        let summary = build_retrieval_eval_regression(
            Path::new("/tmp/current.json"),
            &current,
            Path::new("/tmp/baseline.json"),
            &baseline,
            0.02,
        );
        assert!(summary.comparable);
        assert!(!summary.passed);
        assert_eq!(summary.checks.len(), 3);
        let failed_checks = summary.checks.iter().filter(|check| !check.passed).count();
        assert_eq!(failed_checks, 1);
    }

    #[test]
    fn eval_regression_requires_matching_scope() {
        let baseline = make_eval_report("/tmp/fixtures_a.json", "hybrid");
        let current = make_eval_report("/tmp/fixtures_b.json", "vector");
        let summary = build_retrieval_eval_regression(
            Path::new("/tmp/current.json"),
            &current,
            Path::new("/tmp/baseline.json"),
            &baseline,
            0.01,
        );
        assert!(!summary.comparable);
        assert!(!summary.passed);
        assert!(!summary.scope_mismatches.is_empty());
        assert!(summary.checks.is_empty());
    }

    #[test]
    fn daemon_command_match_is_port_scoped() {
        let cmd = "/usr/local/bin/budi-daemon serve --host 127.0.0.1 --port 7878";
        assert!(is_budi_daemon_command_for_port(cmd, 7878));
        assert!(!is_budi_daemon_command_for_port(cmd, 9999));
        assert!(!is_budi_daemon_command_for_port(
            "python3 -m http.server 7878",
            7878
        ));
    }

    #[test]
    fn hook_query_retry_policy_only_retries_transient_failures() {
        assert!(should_retry_hook_query(QueryErrorReason::Timeout));
        assert!(should_retry_hook_query(QueryErrorReason::TransportError));
        assert!(!should_retry_hook_query(QueryErrorReason::HttpError));
        assert!(!should_retry_hook_query(QueryErrorReason::Error));
    }
}
