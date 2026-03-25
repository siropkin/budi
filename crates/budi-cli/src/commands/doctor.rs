use std::path::{Path, PathBuf};

use anyhow::Result;
use budi_core::config;

use crate::daemon::{daemon_health, ensure_daemon_running};

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

    // Database schema check (via daemon if healthy, otherwise skip)
    if daemon_health(&config) {
        if let Ok(client) = crate::client::DaemonClient::connect() {
            if let Ok(sv) = client.schema_version() {
                let exists = sv.get("exists").and_then(|v| v.as_bool()).unwrap_or(false);
                let current = sv.get("current").and_then(|v| v.as_u64()).unwrap_or(0);
                let target = sv.get("target").and_then(|v| v.as_u64()).unwrap_or(0);
                if !exists {
                    println!("  [!!] database: not created yet");
                    issues.push("No database. Run `budi sync` to create it.".into());
                } else if current >= target {
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

    // Check hooks installation
    let home = std::env::var("HOME").unwrap_or_default();
    let claude_settings = format!("{}/.claude/settings.json", home);
    let cursor_hooks = format!("{}/.cursor/hooks.json", home);

    let claude_ok = std::fs::read_to_string(&claude_settings)
        .map(|s| s.contains("budi hook"))
        .unwrap_or(false);
    let cursor_ok = std::fs::read_to_string(&cursor_hooks)
        .map(|s| s.contains("budi hook"))
        .unwrap_or(false);

    if claude_ok || cursor_ok {
        let sources: Vec<&str> = [
            claude_ok.then_some("Claude Code"),
            cursor_ok.then_some("Cursor"),
        ]
        .into_iter()
        .flatten()
        .collect();
        println!("  [ok] hooks: {}", sources.join(", "));
    } else {
        println!("  [!!] hooks: no hooks found");
        println!("    Run `budi init` to install hooks");
        issues.push("No hooks installed. Run `budi init` to set up hooks.".into());
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
