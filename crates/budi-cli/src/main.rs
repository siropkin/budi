use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use budi_core::analytics;
use budi_core::claude_data;
use budi_core::config::{self, BudiConfig, CLAUDE_LOCAL_SETTINGS};
use budi_core::cost;
use budi_core::hooks::{PostToolUseInput, UserPromptSubmitInput, UserPromptSubmitOutput};
use budi_core::insights;
use budi_core::rpc::{StatusRequest, StatusResponse};
use chrono::{Datelike, Local, NaiveDate};
use clap::{Parser, Subcommand, ValueEnum};
use reqwest::blocking::Client;
use serde_json::{Value, json};
use tracing_subscriber::EnvFilter;

const HEALTH_TIMEOUT_SECS: u64 = 3;
const STATUS_TIMEOUT_SECS: u64 = 120;
const HOOK_LOG_LOCK_TIMEOUT_MS: u64 = 800;
const HOOK_LOG_LOCK_STALE_SECS: u64 = 30;

#[derive(Debug, Parser)]
#[command(name = "budi")]
#[command(about = "budi — AI code analytics. See where your tokens go.")]
#[command(version)]
#[command(after_help = "Get started:\n  budi init --global")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Set up budi (use --global for all repos, or run in a repo for local setup)
    Init {
        #[arg(long, hide = true)]
        repo_root: Option<PathBuf>,
        #[arg(long, hide = true)]
        no_daemon: bool,
        /// Install hooks globally in ~/.claude/settings.json (works for all repos)
        #[arg(long, default_value_t = false)]
        global: bool,
    },
    /// Check repo health: config, hooks, daemon
    Doctor {
        #[arg(long, hide = true)]
        repo_root: Option<PathBuf>,
    },
    /// Manage repos
    Repo {
        #[command(subcommand)]
        command: RepoCommands,
    },
    #[command(hide = true)]
    Hook {
        #[command(subcommand)]
        command: HookCommands,
    },
    /// Show usage analytics
    Stats {
        /// Time period to show (default: today)
        #[arg(long, short, value_enum, default_value_t = StatsPeriod::Today)]
        period: StatsPeriod,
        /// Show details for a specific session (ID or prefix)
        #[arg(long)]
        session: Option<String>,
        /// Show most-used working directories
        #[arg(long, default_value_t = false)]
        files: bool,
        /// Filter by provider (e.g. claude_code, cursor)
        #[arg(long)]
        provider: Option<String>,
        /// Output as JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Show actionable insights and recommendations
    Insights {
        /// Time period to analyze (default: all)
        #[arg(long, short, value_enum, default_value_t = StatsPeriod::All)]
        period: StatsPeriod,
        /// Output as JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Sync transcripts into the analytics database
    Sync,
    /// Show model usage breakdown
    Models {
        /// Time period (default: all)
        #[arg(long, short, value_enum, default_value_t = StatsPeriod::All)]
        period: StatsPeriod,
        /// Output as JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// List sessions with stats
    Sessions {
        /// Time period (default: today)
        #[arg(long, short, value_enum, default_value_t = StatsPeriod::Today)]
        period: StatsPeriod,
        /// Output as JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Show installed plugins
    Plugins {
        /// Output as JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Show repositories ranked by usage
    Projects {
        /// Time period (default: all)
        #[arg(long, short, value_enum, default_value_t = StatsPeriod::All)]
        period: StatsPeriod,
        /// Output as JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Open the budi dashboard in the browser
    Dashboard,
    /// Update budi to the latest version
    Update,
    /// Print version information
    Version,
    /// Print a compact status line (reads stdin, outputs one line)
    Statusline {
        /// Install the status line in ~/.claude/settings.json
        #[arg(long, default_value_t = false)]
        install: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum StatsPeriod {
    Today,
    Week,
    Month,
    All,
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
    #[command(hide = true)]
    SubagentStart,
}

#[derive(Debug, Subcommand)]
enum RepoCommands {
    #[command(hide = true)]
    List {
        #[arg(long, default_value_t = false)]
        stale_only: bool,
    },
    #[command(hide = true)]
    Remove {
        #[arg(long)]
        repo_root: PathBuf,
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
    #[command(hide = true)]
    Wipe {
        #[arg(long, default_value_t = false)]
        confirm: bool,
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
    /// Show daemon status for the current repo
    Status {
        #[arg(long, hide = true)]
        repo_root: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();

    match cli.command {
        Commands::Init {
            repo_root,
            no_daemon,
            global,
        } => cmd_init(repo_root, no_daemon, global),
        Commands::Doctor { repo_root } => cmd_doctor(repo_root),
        Commands::Repo { command } => match command {
            RepoCommands::List { stale_only } => cmd_repo_list(stale_only),
            RepoCommands::Remove { repo_root, dry_run } => cmd_repo_remove(repo_root, dry_run),
            RepoCommands::Wipe { confirm, dry_run } => cmd_repo_wipe(confirm, dry_run),
            RepoCommands::Status { repo_root } => cmd_status(repo_root),
        },
        Commands::Hook { command } => match command {
            HookCommands::UserPromptSubmit => cmd_hook_user_prompt_submit(),
            HookCommands::PostToolUse => cmd_hook_post_tool_use(),
            HookCommands::SessionStart => cmd_hook_session_start(),
            HookCommands::SessionEnd => cmd_hook_session_end(),
            HookCommands::SubagentStart => cmd_hook_subagent_start(),
        },
        Commands::Stats {
            period,
            session,
            files,
            provider,
            json,
        } => cmd_stats(period, session, files, provider, json),
        Commands::Insights { period, json } => cmd_insights(period, json),
        // Cost command removed — merged into `budi stats`
        Commands::Models { period, json } => cmd_models(period, json),
        Commands::Sessions { period, json } => cmd_sessions(period, json),
        Commands::Plugins { json } => cmd_plugins(json),
        Commands::Projects { period, json } => cmd_projects(period, json),
        Commands::Sync => cmd_sync(),
        Commands::Dashboard => cmd_dashboard(),
        Commands::Update => cmd_update(),
        Commands::Version => {
            println!("budi {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Commands::Statusline { install } => {
            if install {
                cmd_statusline_install()
            } else {
                cmd_statusline()
            }
        }
    }
}

// ─── Init ────────────────────────────────────────────────────────────────────

fn cmd_init(repo_root: Option<PathBuf>, no_daemon: bool, global: bool) -> Result<()> {
    let repo_root = resolve_repo_root(repo_root)?;
    let config = config::load_or_default(&repo_root)?;
    config::ensure_repo_layout(&repo_root)?;
    config::save(&repo_root, &config)?;

    let hooks_location = if global {
        install_hooks_global()?
    } else {
        install_hooks(&repo_root)?;
        repo_root.join(CLAUDE_LOCAL_SETTINGS)
    };

    install_statusline_if_missing();

    if !no_daemon {
        ensure_daemon_running(&repo_root, &config)?;
    }

    // Auto-sync existing transcripts on first run
    let sync_result = init_auto_sync();

    let dashboard_url = format!("{}/dashboard", config.daemon_base_url());

    println!();
    if global {
        println!("\x1b[1;36m  budi\x1b[0m initialized globally");
    } else {
        println!(
            "\x1b[1;36m  budi\x1b[0m initialized in {}",
            repo_root.display()
        );
    }
    println!();
    println!("  Hooks:     {}", hooks_location.display());
    println!(
        "  Data:      {}",
        config::repo_paths(&repo_root)?.data_dir.display()
    );
    println!("  Dashboard: {dashboard_url}");
    println!();
    match sync_result {
        Ok((files, msgs)) if files > 0 => {
            println!(
                "  Synced \x1b[1m{msgs}\x1b[0m messages from \x1b[1m{files}\x1b[0m transcript files."
            );
        }
        Ok(_) => {
            println!("  No existing transcripts found (will sync as you use Claude Code).");
        }
        Err(e) => {
            tracing::warn!("auto-sync failed: {e}");
            println!("  Auto-sync skipped (run `budi sync` manually).");
        }
    }
    println!();
    println!("  \x1b[1mNext steps:\x1b[0m");
    println!("    1. Restart Claude Code so hook settings take effect");
    println!("    2. Open the dashboard: \x1b[4m{dashboard_url}\x1b[0m");
    println!("    3. Run `budi doctor` to verify everything is working");
    println!();

    // Auto-open dashboard in browser (best-effort)
    open_url_in_browser(&dashboard_url);

    Ok(())
}

fn init_auto_sync() -> Result<(usize, usize)> {
    let db_path = analytics::db_path()?;
    let mut conn = analytics::open_db(&db_path)?;
    analytics::sync_all(&mut conn)
}

fn open_url_in_browser(url: &str) {
    let result = if cfg!(target_os = "macos") {
        Command::new("open")
            .arg(url)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
    } else if cfg!(target_os = "windows") {
        Command::new("cmd")
            .args(["/C", "start", "", url])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
    } else {
        Command::new("xdg-open")
            .arg(url)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
    };
    if let Err(e) = result {
        tracing::debug!("Could not open browser: {e}");
    }
}

// ─── Doctor ──────────────────────────────────────────────────────────────────

fn cmd_doctor(repo_root: Option<PathBuf>) -> Result<()> {
    let repo_root = resolve_repo_root(repo_root)?;
    let config = config::load_or_default(&repo_root)?;
    let paths = config::repo_paths(&repo_root)?;
    let mut issues: Vec<String> = Vec::new();

    println!("budi doctor — {}", repo_root.display());
    println!();

    let has_git = repo_root.join(".git").exists();
    doctor_check("git repo", has_git, None);
    if !has_git {
        issues.push("Not a git repository. Run `git init` first.".into());
    }

    let has_config = paths.config_file.exists();
    if has_config {
        doctor_check("config", true, Some(&paths.config_file));
    } else {
        println!("  [ok] config: using defaults");
    }

    let hooks_path = repo_root.join(CLAUDE_LOCAL_SETTINGS);
    let has_hooks = hooks_path.exists();
    doctor_check("hook settings", has_hooks, Some(&hooks_path));
    if !has_hooks {
        issues.push("No hook settings. Run `budi init` to install hooks.".into());
    }

    let health = daemon_health(&config);
    doctor_check("daemon", health, None);
    if !health {
        println!("  Attempting daemon start...");
        match ensure_daemon_running(&repo_root, &config) {
            Ok(()) => {
                let retry = daemon_health(&config);
                doctor_check("daemon (retry)", retry, None);
                if !retry {
                    let log_hint = config::daemon_log_path(&repo_root).map_or_else(
                        |_| "Check logs with `budi -vv doctor`.".to_string(),
                        |p| format!("Logs: {}", p.display()),
                    );
                    issues.push(format!("Daemon failed to start. {log_hint}"));
                }
            }
            Err(e) => {
                println!("  x daemon start failed: {e}");
                issues.push(format!("Daemon start error: {e}"));
            }
        }
    }

    // Activity summary
    if daemon_health(&config)
        && let Some(stats) = fetch_daemon_stats(&config)
    {
        let queries = stats.get("queries").and_then(|v| v.as_u64()).unwrap_or(0);
        if queries > 0 {
            let skips = stats.get("skips").and_then(|v| v.as_u64()).unwrap_or(0);
            println!();
            println!("  activity: {} queries, {} skipped", queries, skips);
        }
    }

    println!();
    if issues.is_empty() {
        println!("All checks passed.");
    } else {
        println!("Issues found:");
        for issue in &issues {
            println!("  - {issue}");
        }
    }

    if issues.is_empty()
        && let Some(stats) = daemon_health(&config)
            .then(|| fetch_daemon_stats(&config))
            .flatten()
    {
        let queries = stats.get("queries").and_then(|v| v.as_u64()).unwrap_or(0);
        if queries == 0 {
            println!();
            println!("No queries yet. Start a Claude Code session to see budi in action.");
        }
    }
    Ok(())
}

fn doctor_check(label: &str, ok: bool, path: Option<&Path>) {
    let mark = if ok { "ok" } else { "!!" };
    if let Some(p) = path {
        println!("  [{mark}] {label}: {}", p.display());
    } else {
        println!("  [{mark}] {label}");
    }
}

// ─── Status ──────────────────────────────────────────────────────────────────

fn cmd_status(repo_root: Option<PathBuf>) -> Result<()> {
    let repo_root = resolve_repo_root(repo_root)?;
    let config = config::load_or_default(&repo_root)?;
    ensure_daemon_running(&repo_root, &config)?;
    let response =
        fetch_status_snapshot(&config.daemon_base_url(), &repo_root.display().to_string())
            .context("Status endpoint returned error")?;

    println!("budi daemon {}", response.daemon_version);
    println!("repo: {}", response.repo_root);
    println!("hooks detected: {}", response.hooks_detected);
    Ok(())
}

// ─── Repo Management ─────────────────────────────────────────────────────────

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
        .filter(|e| e.kind == RepoStorageEntryKind::Active)
        .count();
    let stale = entries
        .iter()
        .filter(|e| e.kind == RepoStorageEntryKind::Stale)
        .count();
    let marker_missing = entries
        .iter()
        .filter(|e| e.kind == RepoStorageEntryKind::MarkerMissing)
        .count();

    println!("repo storage root: {}", repos_root.display());
    println!(
        "scanned={} active={} stale={} unknown_without_marker={}",
        scanned, active, stale, marker_missing
    );
    let filtered: Vec<_> = entries
        .iter()
        .filter(|e| !stale_only || e.kind == RepoStorageEntryKind::Stale)
        .collect();
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

// ─── Hook Commands ───────────────────────────────────────────────────────────

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

    log_hook_event(&repo_root, &config, || {
        json!({
            "event": "UserPromptSubmit",
            "phase": "input",
            "ts_unix_ms": now_unix_ms(),
            "session_id": session_id.clone(),
            "cwd": parsed.common.cwd,
            "prompt_chars": parsed.prompt.len(),
        })
    });

    // Record the prompt in daemon stats (via HTTP hook if daemon is running).
    // In v4 we no longer inject context — just track analytics.
    log_hook_event(&repo_root, &config, || {
        json!({
            "event": "UserPromptSubmit",
            "phase": "output",
            "ts_unix_ms": now_unix_ms(),
            "session_id": session_id.clone(),
            "latency_ms": hook_started.elapsed().as_millis(),
            "success": true,
            "context_chars": 0,
        })
    });

    emit_hook_response(UserPromptSubmitOutput::allow_with_context(String::new()))
}

fn cmd_hook_post_tool_use() -> Result<()> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let parsed: PostToolUseInput = match serde_json::from_str(&input) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };

    let cwd = PathBuf::from(&parsed.common.cwd);
    let Ok(repo_root) = config::find_repo_root(&cwd) else {
        return Ok(());
    };
    let Ok(config) = config::load_or_default(&repo_root) else {
        return Ok(());
    };

    log_hook_event(&repo_root, &config, || {
        json!({
            "event": "PostToolUse",
            "phase": "input",
            "ts_unix_ms": now_unix_ms(),
            "session_id": parsed.common.session_id.clone(),
            "tool_name": parsed.tool_name,
        })
    });
    Ok(())
}

fn cmd_hook_session_start() -> Result<()> {
    let mut input = String::new();
    let _ = io::stdin().read_to_string(&mut input);

    let cwd = std::env::current_dir()?;
    let Ok(repo_root) = config::find_repo_root(&cwd) else {
        return Ok(());
    };
    let config = config::load_or_default(&repo_root)?;
    let _ = ensure_daemon_running(&repo_root, &config);
    Ok(())
}

fn cmd_hook_subagent_start() -> Result<()> {
    let mut input = String::new();
    let _ = io::stdin().read_to_string(&mut input);
    // No project map injection in v4 — analytics only.
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

    let session_id = std::env::var("CLAUDE_SESSION_ID").ok();

    if let Some(ref sid) = session_id
        && let Some(stats) = fetch_session_stats(&config, sid)
    {
        let queries = stats.get("queries").and_then(|v| v.as_u64()).unwrap_or(0);
        if queries > 0 {
            let skips = stats.get("skips").and_then(|v| v.as_u64()).unwrap_or(0);
            eprintln!("budi: {} prompts tracked, {} skipped", queries, skips);
        }
    }
    Ok(())
}

fn emit_hook_response(output: UserPromptSubmitOutput) -> Result<()> {
    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}

// ─── Stats ────────────────────────────────────────────────────────────────────

fn period_label(period: StatsPeriod) -> &'static str {
    match period {
        StatsPeriod::Today => "Today",
        StatsPeriod::Week => "This week",
        StatsPeriod::Month => "This month",
        StatsPeriod::All => "All time",
    }
}

fn period_date_range(period: StatsPeriod) -> (Option<String>, Option<String>) {
    let today = Local::now().date_naive();
    match period {
        StatsPeriod::Today => {
            let since = today.format("%Y-%m-%dT00:00:00").to_string();
            (Some(since), None)
        }
        StatsPeriod::Week => {
            let weekday = today.weekday().num_days_from_monday();
            let monday = today - chrono::Duration::days(weekday as i64);
            let since = monday.format("%Y-%m-%dT00:00:00").to_string();
            (Some(since), None)
        }
        StatsPeriod::Month => {
            let first = NaiveDate::from_ymd_opt(today.year(), today.month(), 1).unwrap();
            let since = first.format("%Y-%m-%dT00:00:00").to_string();
            (Some(since), None)
        }
        StatsPeriod::All => (None, None),
    }
}

fn cmd_stats(
    period: StatsPeriod,
    session: Option<String>,
    files: bool,
    provider: Option<String>,
    json_output: bool,
) -> Result<()> {
    let db_path = analytics::db_path()?;
    if !db_path.exists() {
        println!("No analytics data yet. Run \x1b[1mbudi sync\x1b[0m to import transcripts.");
        return Ok(());
    }
    let conn = analytics::open_db(&db_path)?;

    if let Some(ref sid) = session {
        if json_output {
            let detail = analytics::session_detail(&conn, sid)?;
            println!("{}", serde_json::to_string_pretty(&detail)?);
            return Ok(());
        }
        return cmd_stats_session(&conn, sid);
    }

    if files {
        if json_output {
            let (since, until) = period_date_range(period);
            let data = analytics::repo_usage(&conn, since.as_deref(), until.as_deref(), 50)?;
            println!("{}", serde_json::to_string_pretty(&data)?);
            return Ok(());
        }
        return cmd_stats_files(&conn, period);
    }

    if json_output {
        let (since, until) = period_date_range(period);
        let summary = analytics::usage_summary_filtered(
            &conn,
            since.as_deref(),
            until.as_deref(),
            provider.as_deref(),
        )?;
        println!("{}", serde_json::to_string_pretty(&summary)?);
        return Ok(());
    }

    // When no provider filter and multiple agents detected, show breakdown
    if provider.is_none() && analytics::provider_count(&conn)? > 1 {
        let (since, until) = period_date_range(period);
        let providers = analytics::provider_stats(&conn, since.as_deref(), until.as_deref())?;
        if providers.len() > 1 {
            cmd_stats_multi_agent(&conn, period, &providers)?;
            return Ok(());
        }
    }

    cmd_stats_summary_filtered(&conn, period, provider.as_deref())
}

fn cmd_stats_summary_filtered(
    conn: &rusqlite::Connection,
    period: StatsPeriod,
    provider: Option<&str>,
) -> Result<()> {
    let (since, until) = period_date_range(period);
    let summary =
        analytics::usage_summary_filtered(conn, since.as_deref(), until.as_deref(), provider)?;

    let period_label = period_label(period);
    let provider_label = provider.unwrap_or("all");

    println!();
    if provider.is_some() {
        println!(
            "  \x1b[1;36m budi stats\x1b[0m — \x1b[1m{}\x1b[0m \x1b[90m({})\x1b[0m",
            period_label, provider_label
        );
    } else {
        println!(
            "  \x1b[1;36m budi stats\x1b[0m — \x1b[1m{}\x1b[0m",
            period_label
        );
    }
    println!("  \x1b[90m{}\x1b[0m", "─".repeat(40));

    if summary.total_messages == 0 {
        println!("  No data for this period.");
        println!();
        return Ok(());
    }

    println!(
        "  \x1b[1mMessages\x1b[0m     {} \x1b[90m({} user, {} assistant)\x1b[0m",
        summary.total_messages, summary.total_user_messages, summary.total_assistant_messages
    );
    println!("  \x1b[1mSessions\x1b[0m     {}", summary.total_sessions);
    println!();

    let total_input = summary.total_input_tokens
        + summary.total_cache_creation_tokens
        + summary.total_cache_read_tokens;
    println!(
        "  \x1b[1mInput tokens\x1b[0m  {}",
        format_tokens(total_input)
    );
    println!(
        "  \x1b[1mOutput tokens\x1b[0m {}",
        format_tokens(summary.total_output_tokens)
    );

    // Cost breakdown
    let est = cost::estimate_cost_filtered(conn, since.as_deref(), until.as_deref(), provider)?;
    println!();
    println!(
        "  \x1b[1mEst. cost\x1b[0m     \x1b[33m${:.2}\x1b[0m",
        est.total_cost
    );
    println!(
        "  \x1b[90m  input ${:.2}  output ${:.2}  cache write ${:.2}  cache read ${:.2}\x1b[0m",
        est.input_cost, est.output_cost, est.cache_write_cost, est.cache_read_cost
    );
    if est.cache_savings > 0.0 {
        println!("  \x1b[32m  cache savings ${:.2}\x1b[0m", est.cache_savings);
    }

    let tools = analytics::top_tools(conn, since.as_deref(), until.as_deref()).unwrap_or_default();
    if !tools.is_empty() {
        println!();
        println!("  \x1b[1mTop tools\x1b[0m");
        let max_count = tools.first().map(|(_, c)| *c).unwrap_or(1);
        for (name, count) in tools.iter().take(10) {
            let bar_len = ((*count as f64 / max_count as f64) * 20.0) as usize;
            let bar: String = "█".repeat(bar_len);
            println!(
                "    \x1b[36m{:<16}\x1b[0m {:>5}  \x1b[36m{}\x1b[0m",
                name, count, bar
            );
        }
    }

    println!();
    Ok(())
}

fn cmd_stats_multi_agent(
    conn: &rusqlite::Connection,
    period: StatsPeriod,
    providers: &[analytics::ProviderStats],
) -> Result<()> {
    let period_label = period_label(period);

    println!();
    println!(
        "  \x1b[1;36m budi stats\x1b[0m — \x1b[1m{}\x1b[0m",
        period_label
    );
    println!("  \x1b[90m{}\x1b[0m", "─".repeat(40));

    // Per-agent breakdown
    println!("  \x1b[1mAgents\x1b[0m");
    for ps in providers {
        let total_tokens =
            ps.input_tokens + ps.output_tokens + ps.cache_creation_tokens + ps.cache_read_tokens;
        // Use ground-truth cost_cents when available, fall back to estimated
        let cost = if ps.total_cost_cents > 0.0 {
            ps.total_cost_cents / 100.0
        } else {
            ps.estimated_cost
        };
        let lines_str = if ps.total_lines_added > 0 || ps.total_lines_removed > 0 {
            format!(
                "  +{}/\x1b[31m-{}\x1b[0m",
                ps.total_lines_added, ps.total_lines_removed
            )
        } else {
            String::new()
        };
        println!(
            "    \x1b[36m{:<14}\x1b[0m {:>3} sessions  {}  \x1b[33m${:.2}\x1b[0m{}",
            ps.display_name,
            ps.session_count,
            format_tokens(total_tokens),
            cost,
            lines_str,
        );
    }
    println!();

    // Show combined summary
    let (since, until) = period_date_range(period);
    let summary =
        analytics::usage_summary_filtered(conn, since.as_deref(), until.as_deref(), None)?;

    println!(
        "  \x1b[1mTotal\x1b[0m        {} messages, {} sessions",
        summary.total_messages, summary.total_sessions
    );

    let total_input = summary.total_input_tokens
        + summary.total_cache_creation_tokens
        + summary.total_cache_read_tokens;
    println!(
        "  \x1b[1mTokens\x1b[0m       {} in, {} out",
        format_tokens(total_input),
        format_tokens(summary.total_output_tokens),
    );

    println!();
    Ok(())
}

fn cmd_stats_session(conn: &rusqlite::Connection, session_id: &str) -> Result<()> {
    let detail = analytics::session_detail(conn, session_id)?;
    let Some(d) = detail else {
        println!("Session not found: {}", session_id);
        return Ok(());
    };

    println!();
    let title = d
        .session_title
        .as_deref()
        .unwrap_or(&d.session_id[..d.session_id.len().min(12)]);
    println!("  \x1b[1;36m Session\x1b[0m \x1b[90m{}\x1b[0m", title);
    println!("  \x1b[90m{}\x1b[0m", "─".repeat(40));

    // Provider and mode badges
    let mode_badge = d.interaction_mode.as_deref().unwrap_or("unknown");
    println!(
        "  \x1b[1mProvider\x1b[0m  {} \x1b[90m({})\x1b[0m",
        d.provider, mode_badge
    );

    if let Some(ref repo) = d.repo_id {
        println!("  \x1b[1mRepo\x1b[0m      {}", repo);
    } else if let Some(ref dir) = d.project_dir {
        println!("  \x1b[1mProject\x1b[0m   {}", dir);
    }
    if let Some(ref branch) = d.git_branch {
        println!("  \x1b[1mBranch\x1b[0m    {}", branch);
    }
    if let Some(ref ver) = d.version {
        println!("  \x1b[1mClaude\x1b[0m    v{}", ver);
    }
    if d.lines_added > 0 || d.lines_removed > 0 {
        println!(
            "  \x1b[1mLines\x1b[0m     \x1b[32m+{}\x1b[0m/\x1b[31m-{}\x1b[0m",
            d.lines_added, d.lines_removed
        );
    }
    if d.cost_cents > 0.0 {
        println!(
            "  \x1b[1mCost\x1b[0m      \x1b[33m${:.2}\x1b[0m",
            d.cost_cents / 100.0
        );
    }
    println!(
        "  \x1b[1mStarted\x1b[0m   {}",
        format_timestamp(&d.first_seen)
    );
    println!(
        "  \x1b[1mLast msg\x1b[0m  {}",
        format_timestamp(&d.last_seen)
    );
    println!();

    let total_msgs = d.user_messages + d.assistant_messages;
    println!(
        "  \x1b[1mMessages\x1b[0m  {} \x1b[90m({} user, {} assistant)\x1b[0m",
        total_msgs, d.user_messages, d.assistant_messages
    );

    let total_input = d.input_tokens + d.cache_creation_tokens + d.cache_read_tokens;
    println!(
        "  \x1b[1mInput\x1b[0m     {} \x1b[90m(direct: {}, cache w: {}, cache r: {})\x1b[0m",
        format_tokens(total_input),
        format_tokens(d.input_tokens),
        format_tokens(d.cache_creation_tokens),
        format_tokens(d.cache_read_tokens),
    );
    println!(
        "  \x1b[1mOutput\x1b[0m    {}",
        format_tokens(d.output_tokens)
    );

    if !d.top_tools.is_empty() {
        println!();
        println!("  \x1b[1mTools used\x1b[0m");
        for (name, count) in &d.top_tools {
            println!("    \x1b[36m{:<16}\x1b[0m {}", name, count);
        }
    }

    println!();
    Ok(())
}

fn cmd_stats_files(conn: &rusqlite::Connection, period: StatsPeriod) -> Result<()> {
    let (since, until) = period_date_range(period);
    let repos = analytics::repo_usage(conn, since.as_deref(), until.as_deref(), 15)?;

    let period_label = period_label(period);

    println!();
    println!(
        "  \x1b[1;36m📊 Repositories\x1b[0m — \x1b[1m{}\x1b[0m",
        period_label
    );
    println!("  \x1b[90m{}\x1b[0m", "─".repeat(40));

    if repos.is_empty() {
        println!("  No data for this period.");
        println!();
        return Ok(());
    }

    let max_msgs = repos.first().map(|f| f.message_count).unwrap_or(1);
    for r in &repos {
        let bar_len = ((r.message_count as f64 / max_msgs as f64) * 16.0) as usize;
        let bar: String = "█".repeat(bar_len);
        println!(
            "    \x1b[1m{:<30}\x1b[0m {:>5} msgs  {:>8} tok  \x1b[36m{}\x1b[0m",
            r.repo_id,
            r.message_count,
            format_tokens(r.input_tokens + r.output_tokens),
            bar
        );
    }

    println!();
    Ok(())
}

fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        format!("{}", n)
    }
}

fn format_timestamp(ts: &str) -> String {
    // Try to parse as RFC 3339, fall back to raw string.
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|dt| {
            dt.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| ts.to_string())
}

fn shorten_path(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() <= 2 {
        return path.to_string();
    }
    format!("…/{}", parts[parts.len() - 2..].join("/"))
}

// ─── Insights ─────────────────────────────────────────────────────────────────

fn cmd_insights(period: StatsPeriod, json_output: bool) -> Result<()> {
    let db_path = analytics::db_path()?;
    if !db_path.exists() {
        println!("No analytics data yet. Run \x1b[1mbudi sync\x1b[0m to import transcripts.");
        return Ok(());
    }
    let conn = analytics::open_db(&db_path)?;
    let (since, until) = period_date_range(period);
    let ins = insights::generate_insights(&conn, since.as_deref(), until.as_deref(), 0)?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&ins)?);
        return Ok(());
    }

    let period_label = match period {
        StatsPeriod::Today => "Today",
        StatsPeriod::Week => "This week",
        StatsPeriod::Month => "This month",
        StatsPeriod::All => "All time",
    };

    println!("\x1b[1m  Insights — {}\x1b[0m", period_label);
    println!();

    // Search efficiency
    let se = &ins.search_efficiency;
    println!(
        "  \x1b[1mSearch Efficiency\x1b[0m  {} search / {} total tool calls ({:.0}%)",
        se.search_calls,
        se.total_calls,
        se.ratio * 100.0
    );
    if let Some(ref rec) = se.recommendation {
        let color = if se.ratio > 0.40 { "33" } else { "32" }; // yellow or green
        println!("    \x1b[{}m{}\x1b[0m", color, rec);
    }
    println!();

    // MCP tools
    if !ins.mcp_tools.is_empty() {
        println!("  \x1b[1mMCP Tool Usage\x1b[0m");
        for mcp in &ins.mcp_tools {
            println!("    \x1b[36m{}\x1b[0m  {} calls", mcp.tool, mcp.call_count);
        }
        println!();
    }

    // CLAUDE.md files
    if !ins.claude_md_files.is_empty() {
        println!("  \x1b[1mCLAUDE.md Files\x1b[0m");
        for f in &ins.claude_md_files {
            let size_label = if f.est_tokens >= 1000 {
                format!("~{}K tokens", f.est_tokens / 1000)
            } else {
                format!("~{} tokens", f.est_tokens)
            };
            println!(
                "    \x1b[36m{}\x1b[0m  {}",
                shorten_path(&f.path),
                size_label
            );
            if let Some(ref rec) = f.recommendation {
                println!("    \x1b[33m{}\x1b[0m", rec);
            }
        }
        println!();
    }

    // Cache efficiency
    let ce = &ins.cache_efficiency;
    if ce.total_input_tokens > 0 {
        println!(
            "  \x1b[1mCache Efficiency\x1b[0m  {:.0}% hit rate ({} cache reads / {} total input)",
            ce.hit_rate * 100.0,
            format_tokens(ce.total_cache_read_tokens),
            format_tokens(ce.total_input_tokens)
        );
        if let Some(ref rec) = ce.recommendation {
            let color = if ce.hit_rate < 0.30 { "33" } else { "32" };
            println!("    \x1b[{}m{}\x1b[0m", color, rec);
        }
        println!();
    }

    // Token-heavy sessions
    if !ins.token_heavy_sessions.is_empty() {
        println!("  \x1b[1mToken-Heavy Sessions\x1b[0m  (input/output ratio > 5x)");
        for s in ins.token_heavy_sessions.iter().take(5) {
            let project = s
                .repo_id
                .as_deref()
                .unwrap_or_else(|| s.project_dir.as_deref().unwrap_or(""));
            println!(
                "    \x1b[36m{}…\x1b[0m  {} in / {} out ({:.0}x)  {}",
                &s.session_id[..s.session_id.len().min(8)],
                format_tokens(s.input_tokens),
                format_tokens(s.output_tokens),
                s.ratio,
                project
            );
        }
        if !ins.token_heavy_sessions.is_empty() {
            println!(
                "    \x1b[33mHigh input/output ratio suggests large context. \
                 Try splitting tasks into smaller sessions.\x1b[0m"
            );
        }
        println!();
    }

    Ok(())
}

// ─── Cost ─────────────────────────────────────────────────────────────────────

// ─── Models ───────────────────────────────────────────────────────────────────

fn cmd_models(period: StatsPeriod, json_output: bool) -> Result<()> {
    let db_path = analytics::db_path()?;
    if !db_path.exists() {
        println!("No analytics data yet. Run \x1b[1mbudi sync\x1b[0m to import transcripts.");
        return Ok(());
    }
    let conn = analytics::open_db(&db_path)?;
    let (since, until) = period_date_range(period);
    let models = analytics::model_usage(&conn, since.as_deref(), until.as_deref())?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&models)?);
        return Ok(());
    }

    let period_label = period_label(period);
    println!();
    println!(
        "  \x1b[1;36m🤖 Model usage\x1b[0m — \x1b[1m{}\x1b[0m",
        period_label
    );
    println!("  \x1b[90m{}\x1b[0m", "─".repeat(50));

    if models.is_empty() {
        println!("  No data for this period.");
        println!();
        return Ok(());
    }

    let max_msgs = models.first().map(|m| m.message_count).unwrap_or(1);
    for m in &models {
        let bar_len = ((m.message_count as f64 / max_msgs as f64) * 16.0) as usize;
        let bar: String = "█".repeat(bar_len);
        let total_tok =
            m.input_tokens + m.output_tokens + m.cache_read_tokens + m.cache_creation_tokens;
        println!(
            "    \x1b[1m{:<30}\x1b[0m {:>5} msgs  {:>8} tok  \x1b[36m{}\x1b[0m",
            m.model,
            m.message_count,
            format_tokens(total_tok),
            bar
        );
    }

    println!();
    Ok(())
}

// ─── Sessions ─────────────────────────────────────────────────────────────────

fn cmd_sessions(period: StatsPeriod, json_output: bool) -> Result<()> {
    let db_path = analytics::db_path()?;
    if !db_path.exists() {
        println!("No analytics data yet. Run \x1b[1mbudi sync\x1b[0m to import transcripts.");
        return Ok(());
    }
    let conn = analytics::open_db(&db_path)?;
    let (since, until) = period_date_range(period);
    let result = analytics::session_list(
        &conn,
        &analytics::SessionListParams {
            since: since.as_deref(),
            until: until.as_deref(),
            search: None,
            sort_by: None,
            sort_asc: false,
            limit: 100,
            offset: 0,
        },
    )?;
    let sessions = result.sessions;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&sessions)?);
        return Ok(());
    }

    let period_label = period_label(period);
    println!();
    println!(
        "  \x1b[1;36m📋 Sessions\x1b[0m — \x1b[1m{}\x1b[0m  ({} total)",
        period_label,
        sessions.len()
    );
    println!("  \x1b[90m{}\x1b[0m", "─".repeat(60));

    if sessions.is_empty() {
        println!("  No sessions for this period.");
        println!();
        return Ok(());
    }

    for s in sessions.iter().take(20) {
        let short_id = &s.session_id[..s.session_id.len().min(8)];
        let project = s
            .repo_id
            .as_deref()
            .unwrap_or_else(|| s.project_dir.as_deref().unwrap_or(""));
        let total_tok = s.input_tokens + s.output_tokens;
        let cost_str = if s.cost_cents > 0.0 {
            format!("${:.2}", s.cost_cents / 100.0)
        } else {
            "--".to_string()
        };
        println!(
            "    \x1b[36m{}…\x1b[0m  {:>4} msgs  {:>8} tok  {:>6}  {}  \x1b[90m{}\x1b[0m",
            short_id,
            s.message_count,
            format_tokens(total_tok),
            cost_str,
            format_timestamp(&s.first_seen),
            project
        );
    }

    if sessions.len() > 20 {
        println!(
            "    \x1b[90m… and {} more (use --json for full list)\x1b[0m",
            sessions.len() - 20
        );
    }

    println!();
    Ok(())
}

// ─── Plugins ──────────────────────────────────────────────────────────────────

fn cmd_plugins(json_output: bool) -> Result<()> {
    let plugins = claude_data::read_installed_plugins()?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&plugins)?);
        return Ok(());
    }

    println!();
    println!(
        "  \x1b[1;36m🔌 Installed plugins\x1b[0m  ({} total)",
        plugins.len()
    );
    println!("  \x1b[90m{}\x1b[0m", "─".repeat(50));

    if plugins.is_empty() {
        println!("  No plugins installed.");
        println!();
        return Ok(());
    }

    for p in &plugins {
        println!("    \x1b[1m{}\x1b[0m", p.name);
        if !p.description.is_empty() {
            println!("      \x1b[90m{}\x1b[0m", p.description);
        }
    }

    println!();
    Ok(())
}

// ─── Projects ─────────────────────────────────────────────────────────────────

fn cmd_projects(period: StatsPeriod, json_output: bool) -> Result<()> {
    let db_path = analytics::db_path()?;
    if !db_path.exists() {
        println!("No analytics data yet. Run \x1b[1mbudi sync\x1b[0m to import transcripts.");
        return Ok(());
    }
    let conn = analytics::open_db(&db_path)?;
    let (since, until) = period_date_range(period);
    let repos = analytics::repo_usage(&conn, since.as_deref(), until.as_deref(), 20)?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&repos)?);
        return Ok(());
    }

    let period_label = period_label(period);
    println!();
    println!(
        "  \x1b[1;36m📁 Projects\x1b[0m — \x1b[1m{}\x1b[0m",
        period_label
    );
    println!("  \x1b[90m{}\x1b[0m", "─".repeat(50));

    if repos.is_empty() {
        println!("  No data for this period.");
        println!();
        return Ok(());
    }

    let max_msgs = repos.first().map(|f| f.message_count).unwrap_or(1);
    for r in &repos {
        let bar_len = ((r.message_count as f64 / max_msgs as f64) * 16.0) as usize;
        let bar: String = "█".repeat(bar_len);
        println!(
            "    \x1b[1m{:<30}\x1b[0m {:>5} msgs  {:>8} tok  \x1b[36m{}\x1b[0m",
            r.repo_id,
            r.message_count,
            format_tokens(r.input_tokens + r.output_tokens),
            bar
        );
    }

    println!();
    Ok(())
}

// ─── Sync ─────────────────────────────────────────────────────────────────────

fn cmd_sync() -> Result<()> {
    let db_path = analytics::db_path()?;
    let mut conn = analytics::open_db(&db_path)?;

    println!("Syncing transcripts...");
    let (files_synced, messages_ingested) = analytics::sync_all(&mut conn)?;

    if files_synced == 0 && messages_ingested == 0 {
        println!("Already up to date.");
    } else {
        println!(
            "Synced \x1b[1m{}\x1b[0m new messages from \x1b[1m{}\x1b[0m files.",
            messages_ingested, files_synced
        );
    }
    println!("Database: {}", db_path.display());
    Ok(())
}

// ─── Dashboard ───────────────────────────────────────────────────────────────

fn cmd_dashboard() -> Result<()> {
    let url = format!(
        "http://{}:{}/dashboard",
        config::DEFAULT_DAEMON_HOST,
        config::DEFAULT_DAEMON_PORT,
    );
    println!("{}", url);
    // Try to open in browser
    let _ = Command::new("open").arg(&url).spawn();
    Ok(())
}

// ─── Update ──────────────────────────────────────────────────────────────────

fn cmd_update() -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    println!("Current version: v{}", current);
    println!("Checking for updates...");

    // Fetch latest release tag from GitHub API
    let client = Client::builder().timeout(Duration::from_secs(10)).build()?;
    let resp = client
        .get("https://api.github.com/repos/siropkin/budi/releases/latest")
        .header("User-Agent", "budi-cli")
        .send()
        .context("Failed to check for updates")?;

    if !resp.status().is_success() {
        anyhow::bail!("GitHub API returned {}", resp.status());
    }

    let release: Value = resp.json()?;
    let latest_tag = release
        .get("tag_name")
        .and_then(|v| v.as_str())
        .context("Could not parse release tag")?;
    let latest = latest_tag.strip_prefix('v').unwrap_or(latest_tag);

    if latest == current {
        println!("\x1b[32m✓\x1b[0m Already up to date (v{}).", current);
        return Ok(());
    }

    println!(
        "New version available: \x1b[1mv{}\x1b[0m → \x1b[1;32mv{}\x1b[0m",
        current, latest
    );
    println!("Updating...");

    // Run the standalone installer
    let status = Command::new("sh")
        .args([
            "-c",
            "curl -fsSL https://raw.githubusercontent.com/siropkin/budi/main/scripts/install-standalone.sh | sh",
        ])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("Failed to run installer")?;

    if !status.success() {
        anyhow::bail!("Installer exited with {}", status);
    }

    // Restart daemon with new version
    println!("Restarting daemon...");
    let _ = Command::new("pkill").args(["-f", "budi-daemon"]).status();
    thread::sleep(Duration::from_millis(500));

    if let Ok(cwd) = std::env::current_dir()
        && let Ok(repo_root) = config::find_repo_root(&cwd)
    {
        let config = config::load_or_default(&repo_root)?;
        let _ = ensure_daemon_running(&repo_root, &config);
    }

    println!("\x1b[32m✓\x1b[0m Updated to v{}.", latest);
    Ok(())
}

// ─── Statusline ──────────────────────────────────────────────────────────────

fn cmd_statusline() -> Result<()> {
    let mut input = String::new();
    let _ = io::stdin().read_to_string(&mut input);

    let stdin_json = serde_json::from_str::<Value>(&input).ok();

    let cwd = stdin_json
        .as_ref()
        .and_then(|v| v.get("cwd").and_then(|c| c.as_str()).map(String::from))
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.display().to_string())
        });

    let repo_root = cwd
        .as_deref()
        .and_then(|c| config::find_repo_root(Path::new(c)).ok());

    let repo_initialized = repo_root
        .as_ref()
        .is_some_and(|root| root.join(".claude/settings.local.json").exists());

    // Dashboard link (OSC 8 hyperlink)
    let base = format!(
        "http://{}:{}",
        config::DEFAULT_DAEMON_HOST,
        config::DEFAULT_DAEMON_PORT,
    );
    let dashboard_url = format!("{}/dashboard", base);
    let budi_label = "\x1b[36m📊 budi\x1b[0m";
    let dashboard_link = format!(
        "\x1b]8;;{}\x1b\\\x1b[36m↗ dashboard\x1b[0m\x1b]8;;\x1b\\",
        dashboard_url,
    );

    if !repo_initialized {
        println!("{} \x1b[90m· not set up\x1b[0m", budi_label);
        return Ok(());
    }

    // (session cost removed — statusline now shows day/week/month only)

    // ── Fetch today's aggregate cost from budi daemon ───────────────────
    let client = daemon_client_with_timeout(Duration::from_secs(3));
    let statusline_url = format!("{}/analytics/statusline", base);
    let statusline_data: Option<Value> = client
        .get(&statusline_url)
        .send()
        .ok()
        .filter(|r| r.status().is_success())
        .and_then(|r| r.json::<Value>().ok());

    let today_cost: f64 = statusline_data
        .as_ref()
        .and_then(|v| v.get("today_cost").and_then(|c| c.as_f64()))
        .unwrap_or(0.0);
    let week_cost: f64 = statusline_data
        .as_ref()
        .and_then(|v| v.get("week_cost").and_then(|c| c.as_f64()))
        .unwrap_or(0.0);
    let month_cost: f64 = statusline_data
        .as_ref()
        .and_then(|v| v.get("month_cost").and_then(|c| c.as_f64()))
        .unwrap_or(0.0);

    // ── Build status line segments ──────────────────────────────────────
    let dim = "\x1b[90m";
    let reset = "\x1b[0m";
    let yellow = "\x1b[33m";

    // Format cost like the dashboard: $1.2K, $123, $12.50, $0.42, $0
    fn fmt_cost(c: f64) -> String {
        if c >= 1000.0 {
            format!("${:.1}K", c / 1000.0)
        } else if c >= 100.0 {
            format!("${:.0}", c)
        } else if c > 0.0 {
            format!("${:.2}", c)
        } else {
            "$0".to_string()
        }
    }

    let mut parts: Vec<String> = Vec::new();

    // Day / Week / Month costs from budi daemon
    parts.push(format!("{yellow}{}{reset} today", fmt_cost(today_cost)));
    parts.push(format!("{yellow}{}{reset} week", fmt_cost(week_cost)));
    parts.push(format!("{yellow}{}{reset} month", fmt_cost(month_cost)));

    let joined = parts.join(&format!(" {dim}·{reset} "));
    println!("{budi_label} {dim}·{reset} {joined} {dim}·{reset} {dashboard_link}");

    Ok(())
}

const CLAUDE_USER_SETTINGS: &str = ".claude/settings.json";

fn cmd_statusline_install() -> Result<()> {
    let home = std::env::var("HOME").context("HOME not set")?;
    let settings_path = PathBuf::from(&home).join(CLAUDE_USER_SETTINGS);
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
    settings["statusLine"] = json!({
        "type": "command",
        "command": "budi statusline",
        "padding": 0
    });
    let raw = serde_json::to_string_pretty(&settings)?;
    fs::write(&settings_path, raw)
        .with_context(|| format!("Failed writing {}", settings_path.display()))?;
    eprintln!("Installed budi status line in {}", settings_path.display());
    Ok(())
}

fn install_statusline_if_missing() {
    let Ok(home) = std::env::var("HOME") else {
        return;
    };
    let settings_path = PathBuf::from(&home).join(CLAUDE_USER_SETTINGS);
    let existing = settings_path
        .exists()
        .then(|| fs::read_to_string(&settings_path).ok())
        .flatten()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok());

    if let Some(ref s) = existing
        && s.get("statusLine").is_some()
    {
        return;
    }

    if let Ok(()) = cmd_statusline_install() {
        eprintln!("Status line: installed in {}", settings_path.display());
    }
}

// ─── Hooks Installation ──────────────────────────────────────────────────────

fn write_hooks_to_settings(settings_path: &Path) -> Result<()> {
    if let Some(parent) = settings_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed creating {}", parent.display()))?;
    }

    let mut settings = if settings_path.exists() {
        let raw = fs::read_to_string(settings_path)
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

    settings["hooks"]["SessionStart"] = json!([{
        "hooks": [{ "type": "command", "command": "budi hook session-start" }]
    }]);

    let daemon_url = config::BudiConfig::default().daemon_base_url();

    settings["hooks"]["UserPromptSubmit"] = json!([{
        "hooks": [{
            "type": "http",
            "url": format!("{}/hook/prompt-submit", daemon_url),
            "timeout": 30
        }]
    }]);

    settings["hooks"]["PostToolUse"] = json!([{
        "matcher": "Write|Edit|Read|Glob",
        "hooks": [{
            "type": "http",
            "url": format!("{}/hook/tool-use", daemon_url),
            "timeout": 30
        }]
    }]);

    settings["hooks"]["SubagentStart"] = json!([{
        "hooks": [{ "type": "command", "command": "budi hook subagent-start" }]
    }]);

    settings["hooks"]["Stop"] = json!([{
        "hooks": [{ "type": "command", "command": "budi hook session-end" }]
    }]);

    let raw = serde_json::to_string_pretty(&settings)?;
    fs::write(settings_path, raw)
        .with_context(|| format!("Failed writing {}", settings_path.display()))?;
    Ok(())
}

fn install_hooks(repo_root: &Path) -> Result<()> {
    let settings_path = repo_root.join(CLAUDE_LOCAL_SETTINGS);
    write_hooks_to_settings(&settings_path)
}

fn install_hooks_global() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    let settings_path = PathBuf::from(home).join(CLAUDE_USER_SETTINGS);
    write_hooks_to_settings(&settings_path)?;
    Ok(settings_path)
}

// ─── Daemon Management ──────────────────────────────────────────────────────

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

fn fetch_daemon_stats(config: &BudiConfig) -> Option<Value> {
    let client = daemon_client_with_timeout(Duration::from_secs(HEALTH_TIMEOUT_SECS));
    let url = format!("{}/stats", config.daemon_base_url());
    client
        .get(url)
        .send()
        .ok()
        .and_then(|r| r.json::<Value>().ok())
}

fn fetch_session_stats(config: &BudiConfig, session_id: &str) -> Option<Value> {
    let client = daemon_client_with_timeout(Duration::from_secs(HEALTH_TIMEOUT_SECS));
    let url = format!("{}/session-stats", config.daemon_base_url());
    client
        .post(url)
        .json(&json!({"session_id": session_id}))
        .send()
        .ok()
        .and_then(|r| r.json::<Value>().ok())
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
            "Daemon port is occupied but health endpoint is unavailable at {}.",
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
    let log_hint = config::daemon_log_path(repo_root)
        .map(|p| format!("\nCheck daemon log: {}", p.display()))
        .unwrap_or_default();
    anyhow::bail!(
        "Daemon failed to become healthy at {}.{log_hint}",
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
    let output = Command::new("ps")
        .arg("-p")
        .arg(pid.to_string())
        .arg("-o")
        .arg("command=")
        .output()
        .ok()?;
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
    let spaced = format!("--port {port}");
    let inline = format!("--port={port}");
    command.contains("budi-daemon")
        && command.contains("serve")
        && (command.contains(&spaced) || command.contains(&inline))
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
            return Err(err).with_context(|| format!("Failed to send {signal} to pid {pid}"));
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
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)?;
    }
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

// ─── Hook Logging ────────────────────────────────────────────────────────────

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

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_parses_init() {
        let _ = Cli::command();
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
    fn help_shows_expected_commands() {
        let mut command = Cli::command();
        let help = command.render_help().to_string();
        let lower = help.to_ascii_lowercase();
        assert!(lower.contains("init"));
        assert!(lower.contains("doctor"));
        assert!(lower.contains("repo"));
        assert!(lower.contains("stats"));
        assert!(lower.contains("sync"));
    }
}
