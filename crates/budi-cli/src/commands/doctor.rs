use std::path::{Path, PathBuf};

use anyhow::Result;
use budi_core::config::{self, CLAUDE_LOCAL_SETTINGS};

use super::init::{is_budi_configured_in_starship, is_starship_installed, starship_config_path};
use crate::daemon::{daemon_health, ensure_daemon_running, fetch_daemon_stats};

pub fn cmd_doctor(repo_root: Option<PathBuf>) -> Result<()> {
    let repo_root = super::resolve_repo_root(repo_root)?;
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

    // Starship integration (only shown if starship is installed)
    if is_starship_installed() {
        let configured = is_budi_configured_in_starship();
        doctor_check("starship", configured, Some(&starship_config_path()));
        if !configured {
            issues.push(
                "Starship detected but budi module not configured. Run `budi init` to fix.".into(),
            );
        }
    }

    // Database schema check
    if let Ok(db_path) = budi_core::analytics::db_path() {
        if db_path.exists() {
            if let Ok(conn) = budi_core::analytics::open_db(&db_path) {
                let current = budi_core::migration::current_version(&conn);
                let target = budi_core::migration::SCHEMA_VERSION;
                if current >= target {
                    println!("  [ok] database schema: v{}", current);
                } else {
                    println!("  [!!] database schema: v{} (needs v{})", current, target);
                    issues.push(format!(
                        "Database needs migration (v{} → v{}). Run `budi sync` or `budi update`.",
                        current, target
                    ));
                }
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
