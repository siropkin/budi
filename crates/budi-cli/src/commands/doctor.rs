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

    if let Some(ref root) = repo_root {
        println!("budi doctor — {}", root.display());
    } else {
        println!("budi doctor — global (not in a git repo)");
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
            println!("  \x1b[32m\u{2713}\x1b[0m config: using defaults");
        }
    } else {
        println!("  \x1b[90m-\x1b[0m git repo: not in a git repository (global mode)");
        println!("  \x1b[32m\u{2713}\x1b[0m config: using defaults");
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
                    issues.push("Daemon failed to start. Check logs with `budi -vv doctor`.".to_string());
                }
            }
            Err(e) => {
                doctor_check("daemon start", false, None);
                println!("    {e}");
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
                    println!("  \x1b[31m\u{2717}\x1b[0m database: not created yet");
                    issues.push("No database. Run `budi sync` to create it.".into());
                } else if current >= target {
                    println!("  \x1b[32m\u{2713}\x1b[0m database schema: v{}", current);
                } else {
                    println!("  \x1b[31m\u{2717}\x1b[0m database schema: v{} (needs v{})", current, target);
                    issues.push(format!(
                        "Database needs migration (v{} → v{}). Run `budi sync` or `budi update`.",
                        current, target
                    ));
                }
            }
        }
    }

    // Check hooks installation — validate structure, not just string presence
    let home = std::env::var("HOME").unwrap_or_default();
    let claude_settings = format!("{}/.claude/settings.json", home);
    let cursor_hooks = format!("{}/.cursor/hooks.json", home);

    let claude_ok = validate_claude_hooks(&claude_settings);
    let cursor_ok = validate_cursor_hooks(&cursor_hooks);

    if claude_ok || cursor_ok {
        let sources: Vec<&str> = [
            claude_ok.then_some("Claude Code"),
            cursor_ok.then_some("Cursor"),
        ]
        .into_iter()
        .flatten()
        .collect();
        println!("  \x1b[32m\u{2713}\x1b[0m hooks: {}", sources.join(", "));
    } else {
        println!("  \x1b[31m\u{2717}\x1b[0m hooks: no hooks found or misconfigured");
        println!("    Run `budi init` to install hooks");
        issues.push("No hooks installed. Run `budi init` to set up hooks.".into());
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
        println!("  \x1b[32m\u{2713}\x1b[0m transcripts: {}", sources.join(", "));
    } else {
        println!("  \x1b[31m\u{2717}\x1b[0m transcripts: no transcript directories found");
        issues.push("No transcript directories found. Use Claude Code or Cursor to generate data.".into());
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

/// Validate Claude Code hooks: check that budi hook entries exist in the correct nested format.
fn validate_claude_hooks(path: &str) -> bool {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let settings: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let hooks = match settings.get("hooks").and_then(|v| v.as_object()) {
        Some(h) => h,
        None => return false,
    };
    // Check at least SessionStart and PostToolUse have budi hook
    let required = ["SessionStart", "PostToolUse"];
    required.iter().all(|event| {
        hooks.get(*event)
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().any(|entry| {
                entry.get("hooks")
                    .and_then(|h| h.as_array())
                    .map(|hooks| hooks.iter().any(|h| {
                        h.get("command").and_then(|c| c.as_str())
                            .is_some_and(|c| c.trim() == "budi hook")
                    }))
                    .unwrap_or(false)
            }))
            .unwrap_or(false)
    })
}

/// Validate Cursor hooks: check that budi hook entries exist in the flat format.
fn validate_cursor_hooks(path: &str) -> bool {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let config: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let hooks = match config.get("hooks").and_then(|v| v.as_object()) {
        Some(h) => h,
        None => return false,
    };
    let required = ["sessionStart", "postToolUse"];
    required.iter().all(|event| {
        hooks.get(*event)
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().any(|entry| {
                entry.get("command").and_then(|c| c.as_str())
                    .is_some_and(|c| c.trim() == "budi hook")
            }))
            .unwrap_or(false)
    })
}

fn doctor_check(label: &str, ok: bool, path: Option<&Path>) {
    let (mark, color) = if ok { ("\u{2713}", "\x1b[32m") } else { ("\u{2717}", "\x1b[31m") };
    let reset = if std::env::var("NO_COLOR").is_err() { "\x1b[0m" } else { "" };
    let c = if std::env::var("NO_COLOR").is_err() { color } else { "" };
    if let Some(p) = path {
        println!("  {c}{mark}{reset} {label}: {}", p.display());
    } else {
        println!("  {c}{mark}{reset} {label}");
    }
}
