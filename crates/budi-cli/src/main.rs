use std::collections::HashMap;
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
use clap::{ArgAction, Parser, Subcommand};
use reqwest::blocking::Client;
use serde_json::{Value, json};
use tracing_subscriber::EnvFilter;

const HEALTH_TIMEOUT_SECS: u64 = 3;
const PREVIEW_QUERY_TIMEOUT_SECS: u64 = 180;
const HOOK_QUERY_TIMEOUT_SECS: u64 = 12;
const STATUS_TIMEOUT_SECS: u64 = 120;
const UPDATE_TIMEOUT_SECS: u64 = 180;
const INDEX_TIMEOUT_SECS: u64 = 21_600;

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
    Status {
        #[arg(long)]
        repo_root: Option<PathBuf>,
    },
    Ignore {
        pattern: String,
        #[arg(long)]
        repo_root: Option<PathBuf>,
    },
    Doctor {
        #[arg(long)]
        repo_root: Option<PathBuf>,
    },
    Preview {
        prompt: String,
        #[arg(long)]
        repo_root: Option<PathBuf>,
    },
    Observe {
        #[command(subcommand)]
        command: ObserveCommands,
    },
    Hook {
        #[command(subcommand)]
        command: HookCommands,
    },
}

#[derive(Debug, Subcommand)]
enum HookCommands {
    UserPromptSubmit,
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
        Commands::Status { repo_root } => cmd_status(repo_root),
        Commands::Ignore { pattern, repo_root } => cmd_ignore(repo_root, &pattern),
        Commands::Doctor { repo_root } => cmd_doctor(repo_root),
        Commands::Preview { prompt, repo_root } => cmd_preview(repo_root, &prompt),
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
        "Index complete: files={}, chunks={}, changed_files={}",
        response.indexed_files, response.indexed_chunks, response.changed_files
    );
    Ok(())
}

fn cmd_status(repo_root: Option<PathBuf>) -> Result<()> {
    let repo_root = resolve_repo_root(repo_root)?;
    let config = config::load_or_default(&repo_root)?;
    let client = daemon_client_with_timeout(Duration::from_secs(STATUS_TIMEOUT_SECS));
    let url = format!("{}/status", config.daemon_base_url());
    let response: StatusResponse = client
        .post(url)
        .json(&StatusRequest {
            repo_root: repo_root.display().to_string(),
        })
        .send()
        .context("Failed to request daemon status")?
        .error_for_status()
        .context("Status endpoint returned error")?
        .json()
        .context("Invalid status response JSON")?;

    println!("budi daemon {}", response.daemon_version);
    println!("repo: {}", response.repo_root);
    println!("branch: {}", response.branch);
    println!("head: {}", response.head);
    println!("tracked files: {}", response.tracked_files);
    println!("embedded chunks: {}", response.embedded_chunks);
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

fn cmd_doctor(repo_root: Option<PathBuf>) -> Result<()> {
    let repo_root = resolve_repo_root(repo_root)?;
    let config = config::load_or_default(&repo_root)?;
    let paths = config::repo_paths(&repo_root)?;
    println!("repo root: {}", repo_root.display());
    println!(".git: {}", repo_root.join(".git").exists());
    println!("local data dir: {}", paths.data_dir.display());
    println!("config: {}", paths.config_file.exists());
    println!("budi ignore: {}", paths.ignore_file.exists());
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

    println!("branch={} head={}", response.branch, response.head);
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

        if let Some(confidence) = obj.get("retrieval_confidence").and_then(Value::as_f64) {
            self.retrieval_confidence_total += confidence;
            self.retrieval_confidence_count = self.retrieval_confidence_count.saturating_add(1);
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
    if reason.starts_with("query_error:") {
        return "query_error".to_string();
    }
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

fn percentage(part: usize, total: usize) -> f64 {
    if total == 0 {
        0.0
    } else {
        (part as f64) * 100.0 / (total as f64)
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
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        summary.lines_parsed = summary.lines_parsed.saturating_add(1);
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
        "prompt_injection_rate_success_pct": percentage(summary.prompt_injected, summary.prompt_outputs_success),
        "prompt_avg_latency_ms": prompt_avg_latency,
        "prompt_p50_latency_ms": percentile(&summary.prompt_latency_ms, 0.50),
        "prompt_p95_latency_ms": percentile(&summary.prompt_latency_ms, 0.95),
        "avg_retrieval_confidence": avg_confidence,
        "avg_injected_context_chars": avg_injected_context_chars,
        "post_tool_outputs_total": summary.post_tool_outputs_total,
        "post_tool_outputs_success": summary.post_tool_outputs_success,
        "post_tool_outputs_failed": summary.post_tool_outputs_failed,
        "post_tool_success_rate_pct": percentage(summary.post_tool_outputs_success, summary.post_tool_outputs_total),
        "post_tool_avg_latency_ms": post_tool_avg_latency,
        "post_tool_p50_latency_ms": percentile(&summary.post_tool_latency_ms, 0.50),
        "post_tool_p95_latency_ms": percentile(&summary.post_tool_latency_ms, 0.95),
        "post_tool_changed_files_total": summary.post_tool_changed_files_total,
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
            summary.prompt_injected,
            percentage(summary.prompt_injected, summary.prompt_outputs_success)
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
            "- latency ms (avg/p50/p95): {:.1} / {:.1} / {:.1}\n",
            prompt_avg_latency,
            percentile(&summary.prompt_latency_ms, 0.50),
            percentile(&summary.prompt_latency_ms, 0.95)
        ));
        if summary.retrieval_confidence_count > 0 {
            output.push_str(&format!(
                "- avg retrieval confidence: {:.3}\n",
                avg_confidence
            ));
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
                percentage(
                    summary.post_tool_outputs_success,
                    summary.post_tool_outputs_total
                )
            ));
            output.push_str(&format!(
                "- latency ms (avg/p50/p95): {:.1} / {:.1} / {:.1}\n",
                post_tool_avg_latency,
                percentile(&summary.post_tool_latency_ms, 0.50),
                percentile(&summary.post_tool_latency_ms, 0.95)
            ));
            output.push_str(&format!(
                "- changed files sent to daemon: {}\n",
                summary.post_tool_changed_files_total
            ));
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
            "session_id": parsed.common.session_id,
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
    let (context, success, reason) = match query_daemon_with_timeout(
        &repo_root,
        &config,
        &sanitized_prompt,
        Some(&cwd),
        HOOK_QUERY_TIMEOUT_SECS,
    ) {
        Ok(response) => {
            diagnostics = response.diagnostics;
            let skip_reason = evaluate_context_skip(&config, &directives, &diagnostics);
            if let Some(skip_reason) = skip_reason {
                (String::new(), true, format!("skip:{skip_reason}"))
            } else {
                (response.context, true, "ok".to_string())
            }
        }
        Err(err) => (String::new(), false, format!("query_error:{err}")),
    };
    log_hook_event(&repo_root, &config, || {
        json!({
            "event":"UserPromptSubmit",
            "phase":"output",
            "ts_unix_ms": now_unix_ms(),
            "latency_ms": hook_started.elapsed().as_millis(),
            "success": success,
            "reason": reason,
            "context_chars": context.len(),
            "context_excerpt": excerpt(&context, &config),
            "retrieval_intent": diagnostics.intent,
            "retrieval_confidence": diagnostics.confidence,
            "recommended_injection": diagnostics.recommended_injection,
            "skip_reason": diagnostics.skip_reason,
        })
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

fn excerpt(text: &str, config: &BudiConfig) -> String {
    if config.debug_io_full_text {
        return text.to_string();
    }
    if config.debug_io_max_chars == 0 {
        return String::new();
    }
    let max = config.debug_io_max_chars.max(64);
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max).collect::<String>()
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
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&log_path) {
        let mut line = build_value();
        if let Some(obj) = line.as_object_mut() {
            obj.insert(
                "repo_root".to_string(),
                json!(repo_root.display().to_string()),
            );
        }
        if let Ok(serialized) = serde_json::to_string(&line) {
            let _ = writeln!(file, "{serialized}");
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
        "working"
    } else {
        progress.phase.as_str()
    };
    if progress.total_files == 0 {
        if progress.active {
            return format!("Indexing... {phase} ({elapsed_secs:.1}s elapsed)");
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

#[derive(Debug, Clone, Copy, Default)]
struct PromptDirectives {
    force_skip: bool,
    force_inject: bool,
}

fn parse_prompt_directives(prompt: &str) -> PromptDirectives {
    let mut directives = PromptDirectives::default();
    for raw in prompt.split_whitespace() {
        let normalized = raw.trim_matches(|c: char| {
            matches!(
                c,
                ',' | '.' | ';' | ':' | '!' | '?' | '"' | '\'' | '`' | '(' | ')' | '[' | ']'
            )
        });
        if normalized.eq_ignore_ascii_case("@nobudi") {
            directives.force_skip = true;
        } else if normalized.eq_ignore_ascii_case("@forcebudi") {
            directives.force_inject = true;
        }
    }
    if directives.force_inject {
        directives.force_skip = false;
    }
    directives
}

fn sanitize_prompt_for_query(prompt: &str) -> String {
    let mut cleaned = Vec::new();
    for raw in prompt.split_whitespace() {
        let normalized = raw.trim_matches(|c: char| {
            matches!(
                c,
                ',' | '.' | ';' | ':' | '!' | '?' | '"' | '\'' | '`' | '(' | ')' | '[' | ']'
            )
        });
        if normalized.eq_ignore_ascii_case("@nobudi")
            || normalized.eq_ignore_ascii_case("@forcebudi")
        {
            continue;
        }
        cleaned.push(raw);
    }
    let sanitized = cleaned.join(" ").trim().to_string();
    if sanitized.is_empty() {
        prompt.to_string()
    } else {
        sanitized
    }
}

fn evaluate_context_skip(
    config: &BudiConfig,
    directives: &PromptDirectives,
    diagnostics: &QueryDiagnostics,
) -> Option<String> {
    if directives.force_skip {
        return Some("forced_skip".to_string());
    }
    if directives.force_inject {
        return None;
    }
    if !config.smart_skip_enabled {
        return None;
    }
    if !diagnostics_available(diagnostics) {
        return None;
    }
    if config.skip_non_code_prompts && diagnostics.intent == "non-code" {
        return Some("non-code-intent".to_string());
    }
    if !diagnostics.recommended_injection {
        if let Some(reason) = &diagnostics.skip_reason {
            return Some(reason.clone());
        }
        return Some("low-confidence".to_string());
    }
    None
}

fn diagnostics_available(diagnostics: &QueryDiagnostics) -> bool {
    !diagnostics.intent.is_empty()
        || diagnostics.top_score > 0.0
        || diagnostics.margin > 0.0
        || !diagnostics.signals.is_empty()
        || diagnostics.skip_reason.is_some()
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
}
