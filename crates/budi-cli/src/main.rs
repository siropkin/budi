use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use budi_core::config::{self, BudiConfig, CLAUDE_LOCAL_SETTINGS};
use budi_core::hooks::{
    AsyncSystemMessageOutput, PostToolUseInput, UserPromptSubmitInput, UserPromptSubmitOutput,
};
use budi_core::rpc::{
    IndexProgressRequest, IndexProgressResponse, IndexRequest, IndexResponse, QueryDiagnostics,
    QueryRequest, QueryResponse, StatusRequest, StatusResponse, UpdateRequest,
};
use budi_core::{git, index};
use clap::{ArgAction, Parser, Subcommand};
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
use retrieval_eval::run_retrieval_eval;

const HEALTH_TIMEOUT_SECS: u64 = 3;
const PREVIEW_QUERY_TIMEOUT_SECS: u64 = 180;
const SEARCH_QUERY_TIMEOUT_SECS: u64 = 30;
const BENCH_QUERY_TIMEOUT_SECS: u64 = 30;
const EVAL_QUERY_TIMEOUT_SECS: u64 = 45;
const DOCTOR_QUERY_TIMEOUT_SECS: u64 = 8;
const HOOK_QUERY_TIMEOUT_SECS: u64 = 12;
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
    },
    Ignore {
        pattern: String,
        #[arg(long)]
        repo_root: Option<PathBuf>,
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
    Observe {
        #[command(subcommand)]
        command: ObserveCommands,
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
}

#[derive(Debug, Subcommand)]
enum ObserveCommands {
    Enable {
        #[arg(long)]
        repo_root: Option<PathBuf>,
    },
    Disable {
        #[arg(long)]
        repo_root: Option<PathBuf>,
    },
    Report {
        #[arg(long)]
        repo_root: Option<PathBuf>,
        #[arg(
            long,
            value_parser = clap::value_parser!(u32).range(1..=3650),
            conflicts_with = "all"
        )]
        days: Option<u32>,
        #[arg(long, default_value_t = false)]
        all: bool,
        #[arg(long, default_value_t = false)]
        json: bool,
        #[arg(long)]
        out: Option<PathBuf>,
    },
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
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum RepoCommands {
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
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    Preview {
        prompt: String,
        #[arg(long)]
        repo_root: Option<PathBuf>,
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
        } => cmd_index(repo_root, hard, progress),
        Commands::Ignore { pattern, repo_root } => cmd_ignore(repo_root, &pattern),
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
                json,
            } => cmd_eval_retrieval(repo_root, fixtures, limit, json),
        },
        Commands::Repo { command } => match command {
            RepoCommands::Status { repo_root } => cmd_status(repo_root),
            RepoCommands::Stats { repo_root, json } => cmd_stats(repo_root, json),
            RepoCommands::Search {
                query,
                repo_root,
                limit,
                json,
            } => cmd_search(repo_root, &query, limit, json),
            RepoCommands::Preview { prompt, repo_root } => cmd_preview(repo_root, &prompt),
        },
        Commands::Observe { command } => match command {
            ObserveCommands::Enable { repo_root } => cmd_observe_enable(repo_root),
            ObserveCommands::Disable { repo_root } => cmd_observe_disable(repo_root),
            ObserveCommands::Report {
                repo_root,
                days,
                all,
                json,
                out,
            } => cmd_observe_report(repo_root, days, all, json, out),
        },
        Commands::Hook { command } => match command {
            HookCommands::UserPromptSubmit => cmd_hook_user_prompt_submit(),
            HookCommands::PostToolUse => cmd_hook_post_tool_use(),
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

fn cmd_index(repo_root: Option<PathBuf>, hard: bool, progress: bool) -> Result<()> {
    let repo_root = resolve_repo_root(repo_root)?;
    let config = config::load_or_default(&repo_root)?;
    ensure_daemon_running(&repo_root, &config)?;
    let response = if progress {
        run_index_with_progress(&repo_root, &config, hard)?
    } else {
        send_index_request(
            &config.daemon_base_url(),
            &repo_root.display().to_string(),
            hard,
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
    Ok(())
}

fn cmd_status(repo_root: Option<PathBuf>) -> Result<()> {
    let repo_root = resolve_repo_root(repo_root)?;
    let config = config::load_or_default(&repo_root)?;
    let response =
        fetch_status_snapshot(&config.daemon_base_url(), &repo_root.display().to_string())
            .context("Status endpoint returned error")?;

    println!("budi daemon {}", response.daemon_version);
    println!("repo: {}", response.repo_root);
    println!("branch: {}", response.branch);
    println!("head: {}", response.head);
    println!("tracked files: {}", response.tracked_files);
    println!("embedded chunks: {}", response.embedded_chunks);
    println!("invalid embeddings: {}", response.invalid_embeddings);
    println!("dirty files: {}", response.dirty_files);
    println!("hooks detected: {}", response.hooks_detected);
    Ok(())
}

fn cmd_ignore(repo_root: Option<PathBuf>, pattern: &str) -> Result<()> {
    let repo_root = resolve_repo_root(repo_root)?;
    let ignore_file = config::ignore_path(&repo_root)?;
    let mut existing = String::new();
    if ignore_file.exists() {
        existing = fs::read_to_string(&ignore_file)
            .with_context(|| format!("Failed reading {}", ignore_file.display()))?;
    }
    if existing.lines().any(|line| line.trim() == pattern) {
        println!("Pattern already exists in {}", ignore_file.display());
        return Ok(());
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&ignore_file)
        .with_context(|| format!("Failed opening {}", ignore_file.display()))?;
    writeln!(file, "{pattern}")?;
    println!("Added `{pattern}` to {}", ignore_file.display());
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
    println!("budi ignore: {}", config::ignore_path(&repo_root)?.exists());
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

fn cmd_preview(repo_root: Option<PathBuf>, prompt: &str) -> Result<()> {
    let repo_root = resolve_repo_root(repo_root)?;
    let config = config::load_or_default(&repo_root)?;
    ensure_daemon_running(&repo_root, &config)?;
    let directives = parse_prompt_directives(prompt);
    let sanitized_prompt = sanitize_prompt_for_query(prompt);
    let response = query_daemon_with_timeout(
        &repo_root,
        &config,
        &sanitized_prompt,
        Some(&repo_root),
        PREVIEW_QUERY_TIMEOUT_SECS,
    )?;
    let effective_skip_reason = evaluate_context_skip(&config, &directives, &response.diagnostics);
    let forced_inject = directives.force_inject && !directives.force_skip;
    let recommended_injection = if forced_inject {
        true
    } else {
        effective_skip_reason.is_none() && response.diagnostics.recommended_injection
    };
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
            "- {}:{}-{} score={:.4} reason={}",
            item.path, item.start_line, item.end_line, item.score, item.reason
        );
    }
    println!("\n--- injected context preview ---\n{}", context_preview);
    Ok(())
}

fn cmd_search(
    repo_root: Option<PathBuf>,
    query: &str,
    limit: usize,
    json_output: bool,
) -> Result<()> {
    if limit == 0 {
        anyhow::bail!("--limit must be at least 1");
    }
    let repo_root = resolve_repo_root(repo_root)?;
    let config = config::load_or_default(&repo_root)?;
    ensure_daemon_running(&repo_root, &config)?;
    let sanitized_query = sanitize_prompt_for_query(query);
    let response = query_daemon_with_timeout(
        &repo_root,
        &config,
        &sanitized_query,
        Some(&repo_root),
        SEARCH_QUERY_TIMEOUT_SECS,
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
            "total_candidates": response.total_candidates,
            "returned": limited_snippets.len(),
            "diagnostics": response.diagnostics,
            "snippets": limited_snippets,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    println!("query: {}", query);
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
            "- {}:{}-{} score={:.4} reason={}",
            item.path, item.start_line, item.end_line, item.score, item.reason
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
        let response =
            send_query_request(&client, &query_url, &repo_root, prompt, Some(&repo_root))
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
    json_output: bool,
) -> Result<()> {
    let repo_root = resolve_repo_root(repo_root)?;
    let config = config::load_or_default(&repo_root)?;
    ensure_daemon_running(&repo_root, &config)?;
    let fixtures_path =
        fixtures.unwrap_or_else(|| repo_root.join(".budi").join("eval").join("retrieval.json"));
    let report = run_retrieval_eval(&repo_root, &fixtures_path, limit, |sanitized_query| {
        query_daemon_with_timeout(
            &repo_root,
            &config,
            sanitized_query,
            Some(&repo_root),
            EVAL_QUERY_TIMEOUT_SECS,
        )
    })?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    println!("repo: {}", report.repo_root);
    println!("fixtures: {}", report.fixtures_path);
    println!(
        "cases: total={} scored={} errors={}",
        report.total_cases, report.scored_cases, report.cases_with_errors
    );
    println!(
        "metrics: hit@1={:.3} hit@3={:.3} hit@5={:.3} mrr={:.3}",
        report.hit_at_1, report.hit_at_3, report.hit_at_5, report.mrr
    );
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
            "- rank={} intent={} confidence={:.3} query=\"{}\" expected={} top={}",
            rank_display, case.intent, case.confidence, case.query, expected, top
        );
        if let Some(err) = &case.error {
            println!("  error={err}");
        }
    }
    Ok(())
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
    let git_snapshot = git::snapshot(&repo_root).ok();

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
    let (branch, head, dirty_files) = if let Some(snapshot) = git_snapshot {
        (snapshot.branch, snapshot.head, snapshot.dirty_files.len())
    } else {
        ("unknown".to_string(), "unknown".to_string(), 0usize)
    };

    if json_output {
        let payload = json!({
            "repo_root": repo_root.display().to_string(),
            "branch": branch,
            "head": head,
            "dirty_files": dirty_files,
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
    println!("branch: {}", branch);
    println!("head: {}", head);
    println!("dirty files: {}", dirty_files);
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
                "route /status: ok (tracked_files={} embedded_chunks={} invalid_embeddings={} dirty_files={})",
                status.tracked_files,
                status.embedded_chunks,
                status.invalid_embeddings,
                status.dirty_files
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
            println!(
                "route /progress: ok (state={} phase={} active={} total={} processed={} sanity={})",
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
                progress.total_files,
                progress.processed_files,
                if progress_issues.is_empty() {
                    "ok".to_string()
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

#[derive(Debug, Default)]
struct ObserveSummary {
    lines_total: usize,
    lines_parsed: usize,
    lines_in_window: usize,
    first_ts_unix_ms: Option<u128>,
    last_ts_unix_ms: Option<u128>,
    prompt_outputs_total: usize,
    prompt_outputs_success: usize,
    prompt_outputs_failed: usize,
    prompt_injected: usize,
    prompt_skipped: usize,
    prompt_recommended_injection: usize,
    context_chars_total: usize,
    context_chars_injected_total: usize,
    prompt_latency_ms: Vec<f64>,
    retrieval_confidence_total: f64,
    retrieval_confidence_count: usize,
    retrieval_top_score_total: f64,
    retrieval_top_score_count: usize,
    retrieval_margin_total: f64,
    retrieval_margin_count: usize,
    snippets_count_total: usize,
    snippets_count_count: usize,
    total_candidates_total: usize,
    total_candidates_count: usize,
    reason_counts: HashMap<String, usize>,
    intent_counts: HashMap<String, usize>,
    skip_reason_counts: HashMap<String, usize>,
    post_tool_outputs_total: usize,
    post_tool_outputs_success: usize,
    post_tool_outputs_failed: usize,
    post_tool_latency_ms: Vec<f64>,
    post_tool_changed_files_total: usize,
}

impl ObserveSummary {
    fn update_time_bounds(&mut self, ts_unix_ms: u128) {
        if ts_unix_ms == 0 {
            return;
        }
        self.first_ts_unix_ms = Some(
            self.first_ts_unix_ms
                .map_or(ts_unix_ms, |current| current.min(ts_unix_ms)),
        );
        self.last_ts_unix_ms = Some(
            self.last_ts_unix_ms
                .map_or(ts_unix_ms, |current| current.max(ts_unix_ms)),
        );
    }

    fn record_user_prompt_output(&mut self, obj: &serde_json::Map<String, Value>) {
        self.prompt_outputs_total = self.prompt_outputs_total.saturating_add(1);

        let success = obj.get("success").and_then(Value::as_bool).unwrap_or(false);
        if success {
            self.prompt_outputs_success = self.prompt_outputs_success.saturating_add(1);
        } else {
            self.prompt_outputs_failed = self.prompt_outputs_failed.saturating_add(1);
        }

        let context_chars = obj
            .get("context_chars")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        self.context_chars_total = self.context_chars_total.saturating_add(context_chars);
        if success && context_chars > 0 {
            self.prompt_injected = self.prompt_injected.saturating_add(1);
            self.context_chars_injected_total = self
                .context_chars_injected_total
                .saturating_add(context_chars);
        } else if success {
            self.prompt_skipped = self.prompt_skipped.saturating_add(1);
        }

        if obj
            .get("recommended_injection")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            self.prompt_recommended_injection = self.prompt_recommended_injection.saturating_add(1);
        }

        if let Some(latency_ms) = obj.get("latency_ms").and_then(Value::as_f64) {
            self.prompt_latency_ms.push(latency_ms);
        }

        if success {
            if let Some(confidence) = obj.get("retrieval_confidence").and_then(Value::as_f64) {
                self.retrieval_confidence_total += confidence;
                self.retrieval_confidence_count = self.retrieval_confidence_count.saturating_add(1);
            }
            if let Some(top_score) = obj.get("retrieval_top_score").and_then(Value::as_f64) {
                self.retrieval_top_score_total += top_score;
                self.retrieval_top_score_count = self.retrieval_top_score_count.saturating_add(1);
            }
            if let Some(margin) = obj.get("retrieval_margin").and_then(Value::as_f64) {
                self.retrieval_margin_total += margin;
                self.retrieval_margin_count = self.retrieval_margin_count.saturating_add(1);
            }
            if let Some(snippets_count) = obj.get("snippets_count").and_then(Value::as_u64) {
                self.snippets_count_total = self
                    .snippets_count_total
                    .saturating_add(snippets_count as usize);
                self.snippets_count_count = self.snippets_count_count.saturating_add(1);
            }
            if let Some(total_candidates) = obj.get("total_candidates").and_then(Value::as_u64) {
                self.total_candidates_total = self
                    .total_candidates_total
                    .saturating_add(total_candidates as usize);
                self.total_candidates_count = self.total_candidates_count.saturating_add(1);
            }
        }

        if let Some(reason) = obj.get("reason").and_then(Value::as_str) {
            increment_counter(&mut self.reason_counts, &normalize_reason(reason));
        }
        if let Some(intent) = obj.get("retrieval_intent").and_then(Value::as_str)
            && !intent.is_empty()
        {
            increment_counter(&mut self.intent_counts, intent);
        }
        if let Some(skip_reason) = obj.get("skip_reason").and_then(Value::as_str)
            && !skip_reason.is_empty()
        {
            increment_counter(
                &mut self.skip_reason_counts,
                &normalize_skip_reason(skip_reason),
            );
        }
    }

    fn record_post_tool_output(&mut self, obj: &serde_json::Map<String, Value>) {
        self.post_tool_outputs_total = self.post_tool_outputs_total.saturating_add(1);
        let success = obj.get("success").and_then(Value::as_bool).unwrap_or(false);
        if success {
            self.post_tool_outputs_success = self.post_tool_outputs_success.saturating_add(1);
        } else {
            self.post_tool_outputs_failed = self.post_tool_outputs_failed.saturating_add(1);
        }
        if let Some(latency_ms) = obj.get("latency_ms").and_then(Value::as_f64) {
            self.post_tool_latency_ms.push(latency_ms);
        }
        let changed_files = obj
            .get("changed_files")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        self.post_tool_changed_files_total = self
            .post_tool_changed_files_total
            .saturating_add(changed_files);
    }
}

fn normalize_reason(reason: &str) -> String {
    if let Some(rest) = reason.strip_prefix("skip:") {
        if rest.starts_with("low-confidence") {
            return "skip:low-confidence".to_string();
        }
        return format!("skip:{rest}");
    }
    reason.to_string()
}

fn normalize_skip_reason(skip_reason: &str) -> String {
    if skip_reason.starts_with("low-confidence") {
        "low-confidence".to_string()
    } else {
        skip_reason.to_string()
    }
}

fn increment_counter(counter: &mut HashMap<String, usize>, key: &str) {
    let entry = counter.entry(key.to_string()).or_default();
    *entry = entry.saturating_add(1);
}

fn top_counts(counter: &HashMap<String, usize>, limit: usize) -> Vec<(String, usize)> {
    let mut rows: Vec<(String, usize)> = counter.iter().map(|(k, v)| (k.clone(), *v)).collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    rows.truncate(limit);
    rows
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

fn percentage(part: usize, total: usize) -> f64 {
    if total == 0 {
        0.0
    } else {
        (part as f64) * 100.0 / (total as f64)
    }
}

#[derive(Debug, Clone)]
struct ObserveAssessment {
    score: i32,
    grade: &'static str,
    likely_helping: bool,
    findings: Vec<String>,
    hypotheses: Vec<String>,
}

fn reason_count(summary: &ObserveSummary, key: &str) -> usize {
    summary.reason_counts.get(key).copied().unwrap_or(0)
}

fn reason_prefix_count(summary: &ObserveSummary, prefix: &str) -> usize {
    summary
        .reason_counts
        .iter()
        .filter(|(name, _)| name.starts_with(prefix))
        .map(|(_, count)| *count)
        .sum()
}

fn build_observe_assessment(
    summary: &ObserveSummary,
    prompt_p95_latency_ms: f64,
    avg_confidence: f64,
    prompt_injection_rate_success_pct: f64,
    post_tool_success_rate_pct: f64,
) -> ObserveAssessment {
    let prompt_success_rate_pct =
        percentage(summary.prompt_outputs_success, summary.prompt_outputs_total);
    let query_like_failures =
        reason_prefix_count(summary, "query_") + reason_count(summary, "daemon_unavailable");
    let query_failure_rate_pct = percentage(query_like_failures, summary.prompt_outputs_total);

    let mut score = 100i32;
    let mut findings = Vec::new();
    let mut hypotheses = Vec::new();

    if prompt_success_rate_pct < 95.0 {
        score -= 30;
    } else if prompt_success_rate_pct < 97.0 {
        score -= 15;
    } else if prompt_success_rate_pct < 99.0 {
        score -= 8;
    }

    if query_failure_rate_pct > 2.0 {
        score -= 20;
    } else if query_failure_rate_pct > 1.0 {
        score -= 10;
    } else if query_failure_rate_pct > 0.2 {
        score -= 5;
    }

    if prompt_p95_latency_ms > 15_000.0 {
        score -= 15;
    } else if prompt_p95_latency_ms > 10_000.0 {
        score -= 8;
    } else if prompt_p95_latency_ms > 5_000.0 {
        score -= 3;
    }

    if avg_confidence < 0.55 {
        score -= 10;
    } else if avg_confidence < 0.70 {
        score -= 4;
    } else {
        score += 3;
    }

    if summary.post_tool_outputs_total > 0 {
        if post_tool_success_rate_pct < 70.0 {
            score -= 20;
        } else if post_tool_success_rate_pct < 85.0 {
            score -= 8;
        } else if post_tool_success_rate_pct >= 95.0 {
            score += 4;
        }
    }

    if !(20.0..=98.0).contains(&prompt_injection_rate_success_pct) {
        score -= 5;
    } else if (40.0..=95.0).contains(&prompt_injection_rate_success_pct) {
        score += 3;
    }

    score = score.clamp(0, 100);
    let grade = if score >= 85 {
        "A"
    } else if score >= 70 {
        "B"
    } else if score >= 55 {
        "C"
    } else {
        "D"
    };
    let likely_helping = score >= 70 && prompt_success_rate_pct >= 95.0 && avg_confidence >= 0.60;

    findings.push(format!(
        "Prompt success rate: {:.1}% ({} / {})",
        prompt_success_rate_pct, summary.prompt_outputs_success, summary.prompt_outputs_total
    ));
    if summary.post_tool_outputs_total > 0 {
        findings.push(format!(
            "PostToolUse success rate: {:.1}% ({} / {})",
            post_tool_success_rate_pct,
            summary.post_tool_outputs_success,
            summary.post_tool_outputs_total
        ));
    }
    findings.push(format!(
        "Prompt p95 latency: {:.0} ms, avg confidence: {:.3}",
        prompt_p95_latency_ms, avg_confidence
    ));

    if post_tool_success_rate_pct < 85.0 && summary.post_tool_outputs_total > 0 {
        hypotheses.push(
            "Incremental update path is unstable under edit bursts; prioritize queueing/coalescing and daemon backpressure handling.".to_string(),
        );
    }
    if query_failure_rate_pct > 1.0 {
        hypotheses.push(
            "Prompt-time query path has transient transport or daemon errors; investigate daemon lifecycle and /query timeout behavior."
                .to_string(),
        );
    }
    if prompt_p95_latency_ms > 10_000.0 {
        hypotheses.push(
            "Long-tail prompt latency suggests cold-load/index churn or heavy retrieval/ranking work; consider warmup and lighter tail-path logic."
                .to_string(),
        );
    }
    if avg_confidence < 0.60 {
        hypotheses.push(
            "Retrieval confidence is low for many prompts; adjust ranking/intent signals or prompt skip threshold."
                .to_string(),
        );
    }
    if prompt_injection_rate_success_pct > 98.0 {
        hypotheses.push(
            "Context is injected almost always; possible over-injection. Consider raising confidence threshold for smarter skip."
                .to_string(),
        );
    }
    if prompt_injection_rate_success_pct < 20.0 {
        hypotheses.push(
            "Context is rarely injected; skip thresholds may be too strict for this repo's prompt patterns."
                .to_string(),
        );
    }
    if hypotheses.is_empty() {
        hypotheses.push(
            "Current telemetry looks healthy; keep observing trend deltas weekly and run periodic judged A/B checks for quality confirmation."
                .to_string(),
        );
    }

    ObserveAssessment {
        score,
        grade,
        likely_helping,
        findings,
        hypotheses,
    }
}

#[derive(Debug, Clone, Copy)]
enum ObserveWindow {
    All,
    RollingDays(u32),
}

impl ObserveWindow {
    fn description(self) -> String {
        match self {
            ObserveWindow::All => "all available history".to_string(),
            ObserveWindow::RollingDays(days) => {
                format!(
                    "last {} day{} (rolling window from now)",
                    days,
                    if days == 1 { "" } else { "s" }
                )
            }
        }
    }

    fn mode(self) -> &'static str {
        match self {
            ObserveWindow::All => "all",
            ObserveWindow::RollingDays(_) => "rolling_days",
        }
    }

    fn days(self) -> Option<u32> {
        match self {
            ObserveWindow::All => None,
            ObserveWindow::RollingDays(days) => Some(days),
        }
    }

    fn since_unix_ms(self, now_unix_ms: u128) -> Option<u128> {
        match self {
            ObserveWindow::All => None,
            ObserveWindow::RollingDays(days) => {
                let window_ms = u128::from(days)
                    .saturating_mul(24)
                    .saturating_mul(60)
                    .saturating_mul(60)
                    .saturating_mul(1000);
                Some(now_unix_ms.saturating_sub(window_ms))
            }
        }
    }
}

fn resolve_observe_window(days: Option<u32>, all: bool) -> ObserveWindow {
    if all {
        return ObserveWindow::All;
    }
    if let Some(days) = days {
        ObserveWindow::RollingDays(days)
    } else {
        ObserveWindow::All
    }
}

fn parse_json_values_from_line(line: &str) -> Vec<Value> {
    serde_json::Deserializer::from_str(line)
        .into_iter::<Value>()
        .filter_map(std::result::Result::ok)
        .collect()
}

fn cmd_observe_enable(repo_root: Option<PathBuf>) -> Result<()> {
    let repo_root = resolve_repo_root(repo_root)?;
    config::ensure_repo_layout(&repo_root)?;
    let mut cfg = config::load_or_default(&repo_root)?;
    cfg.debug_io = true;
    cfg.debug_io_full_text = false;
    cfg.debug_io_max_chars = 0;
    config::save(&repo_root, &cfg)?;
    println!("Observe logging enabled for {}", repo_root.display());
    println!("Log file: {}", config::hook_log_path(&repo_root)?.display());
    println!("Mode: metadata-only (prompt/context text omitted). Use `budi observe report` later.");
    Ok(())
}

fn cmd_observe_disable(repo_root: Option<PathBuf>) -> Result<()> {
    let repo_root = resolve_repo_root(repo_root)?;
    config::ensure_repo_layout(&repo_root)?;
    let mut cfg = config::load_or_default(&repo_root)?;
    cfg.debug_io = false;
    config::save(&repo_root, &cfg)?;
    println!("Observe logging disabled for {}", repo_root.display());
    Ok(())
}

fn cmd_observe_report(
    repo_root: Option<PathBuf>,
    days: Option<u32>,
    all: bool,
    as_json: bool,
    out_path: Option<PathBuf>,
) -> Result<()> {
    let repo_root = resolve_repo_root(repo_root)?;
    let log_path = config::hook_log_path(&repo_root)?;
    if !log_path.exists() {
        println!("No observe log found at {}", log_path.display());
        println!(
            "Enable it with: budi observe enable --repo-root {}",
            repo_root.display()
        );
        return Ok(());
    }

    let window = resolve_observe_window(days, all);
    let since_ms = window.since_unix_ms(now_unix_ms());
    let file = fs::File::open(&log_path)
        .with_context(|| format!("Failed reading observe log at {}", log_path.display()))?;
    let reader = BufReader::new(file);
    let mut summary = ObserveSummary::default();

    for maybe_line in reader.lines() {
        summary.lines_total = summary.lines_total.saturating_add(1);
        let Ok(line) = maybe_line else {
            continue;
        };
        let values = parse_json_values_from_line(&line);
        if values.is_empty() {
            continue;
        }
        summary.lines_parsed = summary.lines_parsed.saturating_add(1);
        for value in values {
            let Some(obj) = value.as_object() else {
                continue;
            };
            let ts_unix_ms = obj
                .get("ts_unix_ms")
                .and_then(Value::as_u64)
                .map(u128::from);
            if let Some(ts) = ts_unix_ms {
                if let Some(since) = since_ms
                    && ts < since
                {
                    continue;
                }
                summary.update_time_bounds(ts);
            }
            summary.lines_in_window = summary.lines_in_window.saturating_add(1);

            let event = obj.get("event").and_then(Value::as_str).unwrap_or_default();
            let phase = obj.get("phase").and_then(Value::as_str).unwrap_or_default();
            match (event, phase) {
                ("UserPromptSubmit", "output") => summary.record_user_prompt_output(obj),
                ("PostToolUse", "output") => summary.record_post_tool_output(obj),
                _ => {}
            }
        }
    }

    let prompt_avg_latency = if summary.prompt_latency_ms.is_empty() {
        0.0
    } else {
        summary.prompt_latency_ms.iter().sum::<f64>() / (summary.prompt_latency_ms.len() as f64)
    };
    let post_tool_avg_latency = if summary.post_tool_latency_ms.is_empty() {
        0.0
    } else {
        summary.post_tool_latency_ms.iter().sum::<f64>()
            / (summary.post_tool_latency_ms.len() as f64)
    };
    let avg_confidence = if summary.retrieval_confidence_count == 0 {
        0.0
    } else {
        summary.retrieval_confidence_total / (summary.retrieval_confidence_count as f64)
    };
    let avg_injected_context_chars = if summary.prompt_injected == 0 {
        0.0
    } else {
        (summary.context_chars_injected_total as f64) / (summary.prompt_injected as f64)
    };
    let avg_top_score = if summary.retrieval_top_score_count == 0 {
        None
    } else {
        Some(summary.retrieval_top_score_total / (summary.retrieval_top_score_count as f64))
    };
    let avg_margin = if summary.retrieval_margin_count == 0 {
        None
    } else {
        Some(summary.retrieval_margin_total / (summary.retrieval_margin_count as f64))
    };
    let avg_snippets = if summary.snippets_count_count == 0 {
        None
    } else {
        Some((summary.snippets_count_total as f64) / (summary.snippets_count_count as f64))
    };
    let avg_total_candidates = if summary.total_candidates_count == 0 {
        None
    } else {
        Some((summary.total_candidates_total as f64) / (summary.total_candidates_count as f64))
    };
    let prompt_p50_latency = percentile(&summary.prompt_latency_ms, 0.50);
    let prompt_p95_latency = percentile(&summary.prompt_latency_ms, 0.95);
    let post_tool_p50_latency = percentile(&summary.post_tool_latency_ms, 0.50);
    let post_tool_p95_latency = percentile(&summary.post_tool_latency_ms, 0.95);
    let prompt_injection_rate_success_pct =
        percentage(summary.prompt_injected, summary.prompt_outputs_success);
    let prompt_query_error_count =
        reason_prefix_count(&summary, "query_") + reason_count(&summary, "daemon_unavailable");
    let prompt_query_error_rate_pct =
        percentage(prompt_query_error_count, summary.prompt_outputs_total);
    let post_tool_success_rate_pct = percentage(
        summary.post_tool_outputs_success,
        summary.post_tool_outputs_total,
    );
    let assessment = build_observe_assessment(
        &summary,
        prompt_p95_latency,
        avg_confidence,
        prompt_injection_rate_success_pct,
        post_tool_success_rate_pct,
    );

    let top_reasons = top_counts(&summary.reason_counts, 8);
    let top_intents = top_counts(&summary.intent_counts, 8);
    let top_skip_reasons = top_counts(&summary.skip_reason_counts, 8);

    let json_payload = json!({
        "repo_root": repo_root.display().to_string(),
        "log_path": log_path.display().to_string(),
        "window": {
            "mode": window.mode(),
            "days": window.days(),
            "description": window.description(),
        },
        "first_ts_unix_ms": summary.first_ts_unix_ms,
        "last_ts_unix_ms": summary.last_ts_unix_ms,
        "lines_total": summary.lines_total,
        "lines_parsed": summary.lines_parsed,
        "lines_in_window": summary.lines_in_window,
        "prompt_outputs_total": summary.prompt_outputs_total,
        "prompt_outputs_success": summary.prompt_outputs_success,
        "prompt_outputs_failed": summary.prompt_outputs_failed,
        "prompt_injected": summary.prompt_injected,
        "prompt_skipped": summary.prompt_skipped,
        "prompt_recommended_injection": summary.prompt_recommended_injection,
        "prompt_injection_rate_success_pct": prompt_injection_rate_success_pct,
        "prompt_query_error_count": prompt_query_error_count,
        "prompt_query_error_rate_pct": prompt_query_error_rate_pct,
        "prompt_avg_latency_ms": prompt_avg_latency,
        "prompt_p50_latency_ms": prompt_p50_latency,
        "prompt_p95_latency_ms": prompt_p95_latency,
        "avg_retrieval_confidence": avg_confidence,
        "avg_retrieval_top_score": avg_top_score,
        "avg_retrieval_margin": avg_margin,
        "avg_snippets_per_prompt": avg_snippets,
        "avg_total_candidates_per_prompt": avg_total_candidates,
        "avg_injected_context_chars": avg_injected_context_chars,
        "post_tool_outputs_total": summary.post_tool_outputs_total,
        "post_tool_outputs_success": summary.post_tool_outputs_success,
        "post_tool_outputs_failed": summary.post_tool_outputs_failed,
        "post_tool_success_rate_pct": post_tool_success_rate_pct,
        "post_tool_avg_latency_ms": post_tool_avg_latency,
        "post_tool_p50_latency_ms": post_tool_p50_latency,
        "post_tool_p95_latency_ms": post_tool_p95_latency,
        "post_tool_changed_files_total": summary.post_tool_changed_files_total,
        "assessment": {
            "score": assessment.score,
            "grade": assessment.grade,
            "likely_helping": assessment.likely_helping,
            "findings": assessment.findings.clone(),
            "hypotheses": assessment.hypotheses.clone(),
        },
        "top_reasons": top_reasons.iter().map(|(name, count)| json!({"name": name, "count": count})).collect::<Vec<_>>(),
        "top_intents": top_intents.iter().map(|(name, count)| json!({"name": name, "count": count})).collect::<Vec<_>>(),
        "top_skip_reasons": top_skip_reasons.iter().map(|(name, count)| json!({"name": name, "count": count})).collect::<Vec<_>>(),
    });

    let rendered = if as_json {
        serde_json::to_string_pretty(&json_payload)?
    } else {
        let mut output = String::new();
        output.push_str(&format!("budi observe report ({})\n", window.description()));
        output.push_str(&format!("repo: {}\n", repo_root.display()));
        output.push_str(&format!("log: {}\n", log_path.display()));
        output.push_str(&format!(
            "log lines: total={} parsed={} in_window={}\n",
            summary.lines_total, summary.lines_parsed, summary.lines_in_window
        ));
        if let (Some(first), Some(last)) = (summary.first_ts_unix_ms, summary.last_ts_unix_ms) {
            output.push_str(&format!("window data ts_unix_ms: {} .. {}\n", first, last));
        }
        output.push('\n');
        output.push_str("Health verdict:\n");
        output.push_str(&format!(
            "- score: {}/100 (grade {})\n",
            assessment.score, assessment.grade
        ));
        output.push_str(&format!(
            "- likely helping right now: {}\n",
            if assessment.likely_helping {
                "yes"
            } else {
                "unclear / needs tuning"
            }
        ));
        for finding in &assessment.findings {
            output.push_str(&format!("- {}\n", finding));
        }
        output.push('\n');
        output.push_str("Prompt hook outcomes:\n");
        output.push_str(&format!(
            "- total outputs: {}\n",
            summary.prompt_outputs_total
        ));
        output.push_str(&format!(
            "- success/fail: {} / {}\n",
            summary.prompt_outputs_success, summary.prompt_outputs_failed
        ));
        output.push_str(&format!(
            "- injected contexts: {} ({:.1}% of successful prompts)\n",
            summary.prompt_injected, prompt_injection_rate_success_pct
        ));
        output.push_str(&format!(
            "- skipped (no context): {} ({:.1}% of successful prompts)\n",
            summary.prompt_skipped,
            percentage(summary.prompt_skipped, summary.prompt_outputs_success)
        ));
        output.push_str(&format!(
            "- recommended injection: {} ({:.1}% of prompt outputs)\n",
            summary.prompt_recommended_injection,
            percentage(
                summary.prompt_recommended_injection,
                summary.prompt_outputs_total
            )
        ));
        output.push_str(&format!(
            "- query-like failures: {} ({:.1}% of prompt outputs)\n",
            prompt_query_error_count, prompt_query_error_rate_pct
        ));
        output.push_str(&format!(
            "- latency ms (avg/p50/p95): {:.1} / {:.1} / {:.1}\n",
            prompt_avg_latency, prompt_p50_latency, prompt_p95_latency
        ));
        if summary.retrieval_confidence_count > 0 {
            output.push_str(&format!(
                "- avg retrieval confidence: {:.3}\n",
                avg_confidence
            ));
            if let (Some(top_score), Some(margin)) = (avg_top_score, avg_margin) {
                output.push_str(&format!(
                    "- avg retrieval top_score/margin: {:.3} / {:.3}\n",
                    top_score, margin
                ));
            }
            if let (Some(snippets), Some(candidates)) = (avg_snippets, avg_total_candidates) {
                output.push_str(&format!(
                    "- avg snippets / candidates: {:.1} / {:.1}\n",
                    snippets, candidates
                ));
            }
        }
        if summary.prompt_injected > 0 {
            output.push_str(&format!(
                "- avg injected context size (chars): {:.0}\n",
                avg_injected_context_chars
            ));
        }

        if !top_intents.is_empty() {
            output.push('\n');
            output.push_str("Top retrieval intents:\n");
            for (intent, count) in top_intents {
                output.push_str(&format!("- {}: {}\n", intent, count));
            }
        }
        if !top_skip_reasons.is_empty() {
            output.push('\n');
            output.push_str("Top skip reasons:\n");
            for (reason, count) in top_skip_reasons {
                output.push_str(&format!("- {}: {}\n", reason, count));
            }
        }
        if !top_reasons.is_empty() {
            output.push('\n');
            output.push_str("Top output reasons:\n");
            for (reason, count) in top_reasons {
                output.push_str(&format!("- {}: {}\n", reason, count));
            }
        }

        if summary.post_tool_outputs_total > 0 {
            output.push('\n');
            output.push_str("PostToolUse (index refresh after edits):\n");
            output.push_str(&format!(
                "- success/fail: {} / {} ({:.1}% success)\n",
                summary.post_tool_outputs_success,
                summary.post_tool_outputs_failed,
                post_tool_success_rate_pct
            ));
            output.push_str(&format!(
                "- latency ms (avg/p50/p95): {:.1} / {:.1} / {:.1}\n",
                post_tool_avg_latency, post_tool_p50_latency, post_tool_p95_latency
            ));
            output.push_str(&format!(
                "- changed files sent to daemon: {}\n",
                summary.post_tool_changed_files_total
            ));
        }

        output.push('\n');
        output.push_str("Hypotheses / next checks:\n");
        for item in &assessment.hypotheses {
            output.push_str(&format!("- {}\n", item));
        }
        output.push('\n');
        output.push_str(&format!(
            "Tips:\n- All history: budi observe report --repo-root {}\n- Rolling 7 days from now: budi observe report --days 7 --repo-root {}\n- JSON export: budi observe report --all --json --out budi-observe.json --repo-root {}\n",
            repo_root.display(),
            repo_root.display(),
            repo_root.display()
        ));
        output
    };

    let mut saved_to: Option<PathBuf> = None;
    if let Some(raw_out) = out_path {
        let target = if raw_out.is_absolute() {
            raw_out
        } else {
            std::env::current_dir()?.join(raw_out)
        };
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("Failed creating report directory {}", parent.display())
            })?;
        }
        fs::write(&target, rendered.as_bytes())
            .with_context(|| format!("Failed writing observe report to {}", target.display()))?;
        saved_to = Some(target);
    }

    if !(as_json && saved_to.is_some()) {
        println!("{rendered}");
    }
    if let Some(saved) = saved_to {
        println!("Saved observe report: {}", saved.display());
    }
    Ok(())
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
            skip_reason: Some("forced_skip".to_string()),
        };
        log_hook_event(&repo_root, &config, || {
            json!({
                "event":"UserPromptSubmit",
                "phase":"output",
                "ts_unix_ms": now_unix_ms(),
                "session_id": session_id.clone(),
                "latency_ms": hook_started.elapsed().as_millis(),
                "success": true,
                "reason": "forced_skip",
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
                "reason": "daemon_unavailable",
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
    let (context, success, reason, error_detail) = match query_daemon_with_timeout(
        &repo_root,
        &config,
        &sanitized_prompt,
        Some(&cwd),
        HOOK_QUERY_TIMEOUT_SECS,
    ) {
        Ok(response) => {
            total_candidates = response.total_candidates;
            snippets_count = response.snippets.len();
            diagnostics = response.diagnostics;
            let skip_reason = evaluate_context_skip(&config, &directives, &diagnostics);
            if let Some(skip_reason) = skip_reason {
                (
                    String::new(),
                    true,
                    format!("skip:{skip_reason}"),
                    String::new(),
                )
            } else {
                (response.context, true, "ok".to_string(), String::new())
            }
        }
        Err(err) => {
            let reason = classify_query_error(&err);
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
        }
        payload
    });
    emit_hook_response(UserPromptSubmitOutput::allow_with_context(context))
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
    if parsed.tool_name != "Write" && parsed.tool_name != "Edit" {
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
                "reason": "daemon_unavailable",
            })
        });
        return Ok(());
    }

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
                    ("ok".to_string(), String::new())
                }
                Err(err) => ("response_parse_error".to_string(), err.to_string()),
            },
            Err(err) => (classify_update_error(&err), err.to_string()),
        },
        Err(err) => (classify_update_error(&err), err.to_string()),
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

fn classify_query_error(err: &anyhow::Error) -> String {
    for cause in err.chain() {
        if let Some(reqwest_err) = cause.downcast_ref::<reqwest::Error>() {
            return format!("query_{}", classify_update_error(reqwest_err));
        }
    }
    let message = err.to_string().to_ascii_lowercase();
    if message.contains("timed out") || message.contains("timeout") {
        return "query_timeout".to_string();
    }
    if message.contains("failed to send query request")
        || message.contains("connection")
        || message.contains("connect")
    {
        return "query_transport_error".to_string();
    }
    if message.contains("query endpoint returned error") {
        return "query_http_error".to_string();
    }
    "query_error".to_string()
}

fn classify_update_error(err: &reqwest::Error) -> String {
    if err.is_timeout() {
        return "request_timeout".to_string();
    }
    if err.is_connect() {
        return "request_connect_error".to_string();
    }
    if let Some(status) = err.status() {
        return format!("http_{}", status.as_u16());
    }
    "request_failed".to_string()
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
        "matcher": "Write|Edit",
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
        for _ in 0..16 {
            if daemon_health_with_timeout(config, Duration::from_millis(250)) {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(250));
        }
        anyhow::bail!(
            "Daemon port is occupied but health endpoint is unavailable at {}",
            config.daemon_base_url()
        );
    }

    spawn_daemon_process(repo_root, config)?;
    for _ in 0..80 {
        if daemon_health_with_timeout(config, Duration::from_millis(500)) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(150));
    }
    anyhow::bail!(
        "Daemon failed to become healthy at {}",
        config.daemon_base_url()
    );
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
    let url = format!("{}/query", config.daemon_base_url());
    let client = daemon_client_with_timeout(Duration::from_secs(timeout_secs));
    send_query_request(&client, &url, repo_root, prompt, cwd)
}

fn send_query_request(
    client: &Client,
    url: &str,
    repo_root: &Path,
    prompt: &str,
    cwd: Option<&Path>,
) -> Result<QueryResponse> {
    let response: QueryResponse = client
        .post(url)
        .json(&QueryRequest {
            repo_root: repo_root.display().to_string(),
            prompt: prompt.to_string(),
            cwd: cwd.map(|p| p.display().to_string()),
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
) -> Result<IndexResponse> {
    let base_url = config.daemon_base_url();
    let repo_root_str = repo_root.display().to_string();
    let (tx, rx) = mpsc::channel::<Result<IndexResponse>>();
    thread::spawn({
        let base_url = base_url.clone();
        let repo_root_str = repo_root_str.clone();
        move || {
            let result = send_index_request(&base_url, &repo_root_str, hard);
            let _ = tx.send(result);
        }
    });

    let started = Instant::now();
    let mut had_progress_line = false;
    let mut warned_missing_progress = false;
    let mut previous_line_len = 0usize;
    loop {
        match rx.try_recv() {
            Ok(result) => {
                if had_progress_line {
                    eprintln!();
                }
                return result;
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                anyhow::bail!("Index worker terminated unexpectedly");
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }

        let elapsed = started.elapsed().as_secs_f32();
        let line = match fetch_index_progress(&base_url, &repo_root_str) {
            Ok(snapshot) => render_progress_line(&snapshot, elapsed),
            Err(err) => {
                if !warned_missing_progress {
                    eprintln!();
                    eprintln!(
                        "warning: live progress endpoint unavailable ({err}). \
restart daemon (`budi init`) to enable per-file progress."
                    );
                    warned_missing_progress = true;
                }
                format!("Indexing... preparing ({elapsed:.1}s elapsed)")
            }
        };
        render_progress_to_stderr(&line, &mut previous_line_len);
        had_progress_line = true;
        thread::sleep(Duration::from_millis(220));
    }
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

fn send_index_request(base_url: &str, repo_root: &str, hard: bool) -> Result<IndexResponse> {
    let client = daemon_client_with_timeout(Duration::from_secs(INDEX_TIMEOUT_SECS));
    let url = format!("{base_url}/index");
    let response: IndexResponse = client
        .post(url)
        .json(&IndexRequest {
            repo_root: repo_root.to_string(),
            hard,
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
    let phase = if progress.phase.is_empty() {
        if progress.state.is_empty() {
            "working"
        } else {
            progress.state.as_str()
        }
    } else {
        progress.phase.as_str()
    };
    if progress.total_files == 0 {
        if progress.active || progress.state == "indexing" {
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
        assert_eq!(skip.as_deref(), Some("non-code-intent"));
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
            skip_reason: Some("low-confidence".to_string()),
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
        assert_eq!(skip.as_deref(), Some("forced_skip"));
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
    fn observe_window_defaults_to_all_history() {
        let window = resolve_observe_window(None, false);
        assert!(matches!(window, ObserveWindow::All));
    }

    #[test]
    fn observe_window_uses_days_when_provided() {
        let window = resolve_observe_window(Some(7), false);
        assert!(matches!(window, ObserveWindow::RollingDays(7)));
    }

    #[test]
    fn parse_json_values_supports_concatenated_objects() {
        let values = parse_json_values_from_line(r#"{"event":"a"}{"event":"b"}"#);
        assert_eq!(values.len(), 2);
        assert_eq!(values[0].get("event").and_then(Value::as_str), Some("a"));
        assert_eq!(values[1].get("event").and_then(Value::as_str), Some("b"));
    }

    #[test]
    fn observe_summary_ignores_failed_prompt_diagnostics() {
        let mut summary = ObserveSummary::default();

        let failed = json!({
            "success": false,
            "context_chars": 0,
            "retrieval_confidence": 0.0,
            "retrieval_top_score": 0.0,
            "retrieval_margin": 0.0,
            "snippets_count": 0,
            "total_candidates": 0,
            "reason": "query_request_timeout"
        });
        summary.record_user_prompt_output(failed.as_object().expect("object"));
        assert_eq!(summary.retrieval_confidence_count, 0);
        assert_eq!(summary.retrieval_top_score_count, 0);
        assert_eq!(summary.retrieval_margin_count, 0);
        assert_eq!(summary.snippets_count_count, 0);
        assert_eq!(summary.total_candidates_count, 0);

        let ok = json!({
            "success": true,
            "context_chars": 10,
            "retrieval_confidence": 0.8,
            "retrieval_top_score": 0.6,
            "retrieval_margin": 0.2,
            "snippets_count": 5,
            "total_candidates": 17,
            "reason": "ok"
        });
        summary.record_user_prompt_output(ok.as_object().expect("object"));
        assert_eq!(summary.retrieval_confidence_count, 1);
        assert_eq!(summary.retrieval_top_score_count, 1);
        assert_eq!(summary.retrieval_margin_count, 1);
        assert_eq!(summary.snippets_count_count, 1);
        assert_eq!(summary.total_candidates_count, 1);
    }
}
