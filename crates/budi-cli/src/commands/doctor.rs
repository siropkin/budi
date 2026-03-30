use std::path::{Path, PathBuf};

use anyhow::Result;
use budi_core::config;

use crate::daemon::{daemon_health, ensure_daemon_running};

pub fn cmd_doctor(repo_root: Option<PathBuf>) -> Result<()> {
    let repo_root = super::try_resolve_repo_root(repo_root);

    let config = match &repo_root {
        Some(root) => config::load_or_default(root)?,
        None => config::BudiConfig::default(),
    };
    let mut issues: Vec<String> = Vec::new();

    let green = super::ansi("\x1b[32m");
    let red = super::ansi("\x1b[31m");
    let dim = super::ansi("\x1b[90m");
    let reset = super::ansi("\x1b[0m");

    if let Some(ref root) = repo_root {
        println!("budi doctor — {}", root.display());
    } else {
        println!("budi doctor — global mode");
    }
    println!();

    if let Some(ref root) = repo_root {
        let has_git = root.join(".git").exists();
        doctor_check("git repo", has_git, None);
        if !has_git {
            issues.push("Not a git repository.".into());
        }

        let paths = config::repo_paths(root)?;
        let has_config = paths.config_file.exists();
        if has_config {
            doctor_check("config", true, Some(&paths.config_file));
        } else {
            println!("  {green}\u{2713}{reset} config: using defaults");
        }
    } else {
        println!("  {dim}-{reset} git repo: not in a git repository (global mode)");
        println!("  {green}\u{2713}{reset} config: using defaults");
    }

    // Check that budi-daemon binary exists on PATH (cross-platform).
    let daemon_bin_found = std::process::Command::new("budi-daemon")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    doctor_check("budi-daemon binary", daemon_bin_found, None);
    if !daemon_bin_found {
        issues.push(
            "budi-daemon binary not found on PATH — copy it alongside budi or add to PATH".into(),
        );
    }

    // Version consistency: CLI and daemon should be the same version
    let cli_version = env!("CARGO_PKG_VERSION");
    let daemon_version = std::process::Command::new("budi-daemon")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .strip_prefix("budi-daemon ")
                .unwrap_or(String::from_utf8_lossy(&o.stdout).trim())
                .to_string()
        });
    match daemon_version {
        Some(ref dv) if dv == cli_version => {
            println!("  {green}\u{2713}{reset} version: v{cli_version} (CLI and daemon match)");
        }
        Some(ref dv) => {
            let yellow = super::ansi("\x1b[33m");
            println!("  {yellow}!{reset} version: CLI v{cli_version} != daemon v{dv}");
            issues.push(format!(
                "Version mismatch: CLI v{cli_version} but daemon v{dv}. Run `budi update` or reinstall."
            ));
        }
        None if daemon_bin_found => {
            println!("  {dim}-{reset} version: v{cli_version} (could not read daemon version)");
        }
        None => {} // Already reported as missing binary
    }

    let health = daemon_health(&config);
    doctor_check("daemon", health, None);
    if !health {
        println!("  Attempting daemon start...");
        match ensure_daemon_running(repo_root.as_deref(), &config) {
            Ok(()) => {
                let retry = daemon_health(&config);
                doctor_check("daemon (retry)", retry, None);
                if !retry {
                    issues.push(
                        "Daemon failed to start. Check logs with `RUST_LOG=debug budi doctor`."
                            .to_string(),
                    );
                }
            }
            Err(e) => {
                doctor_check("daemon start", false, None);
                println!("    {e}");
                issues.push(format!("Daemon start error: {e}"));
            }
        }
    }

    // Database file existence, readability, and integrity check
    if let Ok(db_path) = budi_core::analytics::db_path() {
        if db_path.exists() {
            match std::fs::File::open(&db_path) {
                Ok(_) => {
                    println!(
                        "  {green}\u{2713}{reset} database file: readable at {}",
                        db_path.display()
                    );
                    // Integrity check: verify DB is not corrupted
                    match budi_core::analytics::open_db(&db_path) {
                        Ok(conn) => {
                            match conn.query_row("PRAGMA integrity_check", [], |row| {
                                row.get::<_, String>(0)
                            }) {
                                Ok(ref result) if result == "ok" => {
                                    println!("  {green}\u{2713}{reset} database integrity: ok");
                                }
                                Ok(result) => {
                                    println!("  {red}\u{2717}{reset} database integrity: {result}");
                                    issues
                                        .push(format!("Database integrity check failed: {result}"));
                                }
                                Err(e) => {
                                    println!(
                                        "  {red}\u{2717}{reset} database integrity: could not check ({e})"
                                    );
                                    issues.push(format!("Database integrity check error: {e}"));
                                }
                            }
                        }
                        Err(e) => {
                            println!(
                                "  {red}\u{2717}{reset} database open: failed ({e}). Try `budi migrate`"
                            );
                            issues.push(format!("Database cannot be opened: {e}"));
                        }
                    }
                }
                Err(e) => {
                    println!(
                        "  {red}\u{2717}{reset} database file: not readable at {} ({e})",
                        db_path.display()
                    );
                    issues.push(format!("Database file is not readable: {e}"));
                }
            }
        } else {
            println!(
                "  {dim}-{reset} database file: not yet created at {}",
                db_path.display()
            );
        }
        // Check for hook delivery errors
        if let Ok(home) = budi_core::config::budi_home_dir() {
            let log_path = home.join("hook-debug.log");
            if log_path.exists()
                && let Ok(meta) = std::fs::metadata(&log_path)
                && meta.len() > 0
            {
                let yellow = super::ansi("\x1b[33m");
                println!(
                    "  {yellow}!{reset} hook errors: found in {}",
                    log_path.display()
                );
                issues.push("Hook delivery errors logged. Check hook-debug.log".to_string());
            }
        }
    }

    // Disk space check (warn if < 100MB available)
    {
        let check_path = budi_core::analytics::db_path()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .or_else(|| budi_core::config::budi_home_dir().ok());
        if let Some(ref dir) = check_path {
            match check_available_disk_mb(dir) {
                Some(mb) if mb < 100 => {
                    let yellow = super::ansi("\x1b[33m");
                    println!("  {yellow}!{reset} disk space: {mb} MB available (< 100 MB)");
                    issues.push(format!("Low disk space: only {mb} MB available"));
                }
                Some(mb) => {
                    println!("  {green}\u{2713}{reset} disk space: {mb} MB available");
                }
                None => {
                    println!("  {dim}-{reset} disk space: could not determine");
                }
            }
        }
    }

    // Database schema check (via daemon if healthy, otherwise check file existence)
    if daemon_health(&config) {
        if let Ok(client) = crate::client::DaemonClient::connect()
            && let Ok(sv) = client.schema_version()
        {
            let exists = sv.get("exists").and_then(|v| v.as_bool()).unwrap_or(false);
            let current = sv.get("current").and_then(|v| v.as_u64()).unwrap_or(0);
            let target = sv.get("target").and_then(|v| v.as_u64()).unwrap_or(0);
            if !exists {
                println!("  {red}\u{2717}{reset} database: not created yet");
                issues.push("No database. Run `budi sync` to create it.".into());
            } else if current >= target {
                println!("  {green}\u{2713}{reset} database schema: v{}", current);
            } else {
                println!(
                    "  {red}\u{2717}{reset} database schema: v{} (needs v{})",
                    current, target
                );
                issues.push(format!(
                    "Database needs migration (v{} → v{}). Run `budi init` or `budi update`.",
                    current, target
                ));
            }
        }
    } else {
        // Daemon is down — check if the database file at least exists
        if let Ok(db_path) = budi_core::analytics::db_path() {
            if db_path.exists() {
                println!(
                    "  {dim}-{reset} database: file exists at {} (daemon down, cannot check schema)",
                    db_path.display()
                );
            } else {
                println!(
                    "  {red}\u{2717}{reset} database: not found at {}",
                    db_path.display()
                );
                issues.push("Database not found. Run `budi sync` to create it.".into());
            }
        }
    }

    // Check hooks installation — validate structure, not just string presence
    let home = budi_core::config::home_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let claude_settings = format!("{}/.claude/settings.json", home);
    let cursor_hooks = format!("{}/.cursor/hooks.json", home);

    // Validate hook JSON syntax before deeper checks
    if Path::new(&claude_settings).exists() {
        match std::fs::read_to_string(&claude_settings).and_then(|raw| {
            serde_json::from_str::<serde_json::Value>(&raw)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        }) {
            Ok(_) => {}
            Err(e) => {
                println!(
                    "  {red}\u{2717}{reset} hook JSON syntax: {} is invalid: {e}",
                    claude_settings
                );
                issues.push(format!(
                    "Claude Code settings has invalid JSON: {e}. Fix or delete the file."
                ));
            }
        }
    }
    if Path::new(&cursor_hooks).exists() {
        match std::fs::read_to_string(&cursor_hooks).and_then(|raw| {
            serde_json::from_str::<serde_json::Value>(&raw)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        }) {
            Ok(_) => {}
            Err(e) => {
                println!(
                    "  {red}\u{2717}{reset} hook JSON syntax: {} is invalid: {e}",
                    cursor_hooks
                );
                issues.push(format!(
                    "Cursor hooks has invalid JSON: {e}. Fix or delete the file."
                ));
            }
        }
    }

    let (claude_ok, claude_missing) = validate_claude_hooks(&claude_settings);
    let cursor_dir_exists = Path::new(&format!("{home}/.cursor")).is_dir();
    let (cursor_ok, cursor_missing) = if cursor_dir_exists {
        validate_cursor_hooks(&cursor_hooks)
    } else {
        (false, vec![])
    };

    if claude_ok || cursor_ok {
        let sources: Vec<&str> = [
            claude_ok.then_some("Claude Code"),
            cursor_ok.then_some("Cursor"),
        ]
        .into_iter()
        .flatten()
        .collect();
        println!("  {green}\u{2713}{reset} hooks: {}", sources.join(", "));

        // Report partial issues for providers that have hooks but are incomplete
        if !claude_ok && !claude_missing.is_empty() && claude_missing[0] != "file not readable" {
            let yellow = super::ansi("\x1b[33m");
            println!(
                "  {yellow}!{reset} Claude Code hooks: missing events: {}",
                claude_missing.join(", ")
            );
            issues.push(format!(
                "Claude Code hooks missing events: {}. Run `budi init` to fix.",
                claude_missing.join(", ")
            ));
        }
        if cursor_dir_exists
            && !cursor_ok
            && !cursor_missing.is_empty()
            && cursor_missing[0] != "file not readable"
        {
            let yellow = super::ansi("\x1b[33m");
            println!(
                "  {yellow}!{reset} Cursor hooks: missing events: {}",
                cursor_missing.join(", ")
            );
            issues.push(format!(
                "Cursor hooks missing events: {}. Run `budi init` to fix.",
                cursor_missing.join(", ")
            ));
        } else if cursor_dir_exists && !cursor_ok {
            let dim = super::ansi("\x1b[90m");
            println!("  {dim}-{reset} hooks: Cursor hooks missing or misconfigured");
        }
    } else {
        println!("  {red}\u{2717}{reset} hooks: no hooks found or misconfigured");
        println!("    Run `budi init` to install hooks");
        println!(
            "    Tip: set BUDI_HOOK_DEBUG=1 to log hook failures to ~/.local/share/budi/hook-debug.log"
        );
        issues.push("No hooks installed. Run `budi init` to set up hooks.".into());
    }

    // Print hook debug hint if any hook-related issues were found
    if !claude_ok || (cursor_dir_exists && !cursor_ok) {
        println!();
        println!(
            "  {dim}Tip: set BUDI_HOOK_DEBUG=1 to log hook delivery failures to ~/.local/share/budi/hook-debug.log{reset}"
        );
    }

    // Check MCP server configuration in Claude Code settings
    {
        let mcp_ok = check_mcp_config(&claude_settings);
        if mcp_ok {
            println!("  {green}\u{2713}{reset} MCP: budi server configured");
        } else {
            let yellow = super::ansi("\x1b[33m");
            println!(
                "  {yellow}!{reset} MCP: budi server not configured. Run `budi init` to enable AI agent integration"
            );
        }
    }

    // Check OTEL configuration in Claude Code settings
    {
        let otel_ok = check_otel_config(&claude_settings, &config);
        if otel_ok {
            println!("  {green}\u{2713}{reset} OTEL: configured for exact cost tracking");
        } else {
            let yellow = super::ansi("\x1b[33m");
            println!(
                "  {yellow}!{reset} OTEL: not configured. Run `budi init` to enable exact cost tracking"
            );
            // Not a hard issue — JSONL still works, just estimated cost
        }
    }

    // Check transcript directories exist
    let cc_transcripts = format!("{}/.claude/transcripts", home);
    let cursor_transcripts = format!("{}/.cursor/projects", home);
    let has_cc = Path::new(&cc_transcripts).is_dir();
    let has_cursor = Path::new(&cursor_transcripts).is_dir();
    if has_cc || has_cursor {
        let sources: Vec<&str> = [
            has_cc.then_some("Claude Code"),
            has_cursor.then_some("Cursor"),
        ]
        .into_iter()
        .flatten()
        .collect();
        println!(
            "  {green}\u{2713}{reset} transcripts: {}",
            sources.join(", ")
        );
    } else {
        println!("  {red}\u{2717}{reset} transcripts: no transcript directories found");
        issues.push(
            "No transcript directories found. Use Claude Code or Cursor to generate data.".into(),
        );
    }

    println!();
    if issues.is_empty() {
        println!("All checks passed.");
    } else {
        println!("Issues found:");
        for issue in &issues {
            println!("  - {issue}");
        }
        anyhow::bail!("{} issue(s) found", issues.len());
    }
    Ok(())
}

use super::{CC_HOOK_EVENTS, CURSOR_HOOK_EVENTS};

/// Validate Claude Code hooks: check all expected events have a budi hook entry.
/// Returns (ok, missing_events) — ok is true only when all events are correctly configured.
fn validate_claude_hooks(path: &str) -> (bool, Vec<String>) {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return (false, vec!["file not readable".into()]),
    };
    let settings: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return (false, vec!["invalid JSON".into()]),
    };
    let hooks = match settings.get("hooks").and_then(|v| v.as_object()) {
        Some(h) => h,
        None => return (false, vec!["no hooks key".into()]),
    };

    let mut missing = Vec::new();
    for event in CC_HOOK_EVENTS {
        let ok = hooks
            .get(*event)
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().any(super::is_budi_cc_hook_entry))
            .unwrap_or(false);
        if !ok {
            missing.push((*event).to_string());
        }
    }
    (missing.is_empty(), missing)
}

/// Validate Cursor hooks: check all expected events have a budi hook entry.
/// Returns (ok, missing_events) — ok is true only when all events are correctly configured.
fn validate_cursor_hooks(path: &str) -> (bool, Vec<String>) {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return (false, vec!["file not readable".into()]),
    };
    let config: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return (false, vec!["invalid JSON".into()]),
    };
    let hooks = match config.get("hooks").and_then(|v| v.as_object()) {
        Some(h) => h,
        None => return (false, vec!["no hooks key".into()]),
    };

    let mut missing = Vec::new();
    for event in CURSOR_HOOK_EVENTS {
        let ok = hooks
            .get(*event)
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().any(super::is_budi_cursor_hook_entry))
            .unwrap_or(false);
        if !ok {
            missing.push((*event).to_string());
        }
    }
    (missing.is_empty(), missing)
}

fn doctor_check(label: &str, ok: bool, path: Option<&Path>) {
    let (mark, color) = if ok {
        ("\u{2713}", "\x1b[32m")
    } else {
        ("\u{2717}", "\x1b[31m")
    };
    let c = super::ansi(color);
    let reset = super::ansi("\x1b[0m");
    if let Some(p) = path {
        println!("  {c}{mark}{reset} {label}: {}", p.display());
    } else {
        println!("  {c}{mark}{reset} {label}");
    }
}

/// Check if OTEL env vars are correctly configured in Claude Code settings.
fn check_otel_config(settings_path: &str, config: &config::BudiConfig) -> bool {
    let Ok(raw) = std::fs::read_to_string(settings_path) else {
        return false;
    };
    let Ok(settings) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    let Some(env) = settings.get("env").and_then(|e| e.as_object()) else {
        return false;
    };

    let expected_endpoint = format!("http://127.0.0.1:{}", config.daemon_port);
    let checks = [
        ("CLAUDE_CODE_ENABLE_TELEMETRY", Some("1")),
        (
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            Some(expected_endpoint.as_str()),
        ),
        ("OTEL_EXPORTER_OTLP_PROTOCOL", Some("http/json")),
        ("OTEL_METRICS_EXPORTER", Some("otlp")),
        ("OTEL_LOGS_EXPORTER", Some("otlp")),
    ];

    checks.iter().all(|(key, expected_val)| {
        env.get(*key)
            .and_then(|v| v.as_str())
            .is_some_and(|v| expected_val.is_none_or(|exp| v == exp))
    })
}

/// Check if the budi MCP server is configured in Claude Code settings.
fn check_mcp_config(settings_path: &str) -> bool {
    let Ok(raw) = std::fs::read_to_string(settings_path) else {
        return false;
    };
    let Ok(settings) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    settings
        .get("mcpServers")
        .and_then(|m| m.get("budi"))
        .and_then(|b| b.get("command"))
        .and_then(|c| c.as_str())
        .is_some_and(|c| c.contains("budi"))
}

/// Check available disk space in MB. Uses `df -k` on Unix, skips on Windows.
fn check_available_disk_mb(path: &Path) -> Option<u64> {
    if cfg!(target_os = "windows") {
        // df is not available on Windows; skip disk space check.
        return None;
    }
    let output = std::process::Command::new("df")
        .arg("-k")
        .arg(path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    // df -k output: second line, fourth column is available KB
    let line = stdout.lines().nth(1)?;
    let available_kb: u64 = line.split_whitespace().nth(3)?.parse().ok()?;
    Some(available_kb / 1024)
}
