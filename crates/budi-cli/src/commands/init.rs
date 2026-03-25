use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::Result;
use budi_core::config;
use serde_json::{Value, json};

use crate::daemon::ensure_daemon_running;

pub fn cmd_init(local: bool, repo_root: Option<PathBuf>, no_daemon: bool) -> Result<()> {
    let repo_root = if local || repo_root.is_some() {
        let root = super::try_resolve_repo_root(repo_root);
        if root.is_none() {
            anyhow::bail!(
                "Not in a git repository. Use `budi init` (without --local) for global setup,\n\
                 or run from inside a git repo."
            );
        }
        root
    } else {
        None
    };

    // Config defaults are fine without a repo root.
    let config = match &repo_root {
        Some(root) => {
            let cfg = config::load_or_default(root)?;
            config::ensure_repo_layout(root)?;
            config::save(root, &cfg)?;
            cfg
        }
        None => config::BudiConfig::default(),
    };

    super::statusline::remove_legacy_hooks();
    install_statusline_if_missing();
    install_hooks();

    if !no_daemon {
        ensure_daemon_running(repo_root.as_deref(), &config)?;
    }

    // Auto-sync existing transcripts on first run
    let sync_result = super::sync::init_auto_sync();

    let dashboard_url = format!("{}/dashboard", config.daemon_base_url());

    println!();
    if let Some(ref root) = repo_root {
        let is_reinit = config::repo_paths(root)
            .map(|p| p.data_dir.join("analytics.db").exists())
            .unwrap_or(false);
        if is_reinit {
            println!(
                "\x1b[1;36m  budi\x1b[0m re-initialized in {}",
                root.display()
            );
        } else {
            println!(
                "\x1b[1;36m  budi\x1b[0m initialized in {}",
                root.display()
            );
        }
    } else {
        println!("\x1b[1;36m  budi\x1b[0m initialized (global)");
    }
    println!();
    if let Some(ref root) = repo_root {
        println!(
            "  Data:      {}",
            config::repo_paths(root)
                .map(|p| p.data_dir.display().to_string())
                .unwrap_or_else(|_| "~/.local/share/budi".to_string())
        );
    } else {
        println!(
            "  Data:      {}",
            config::budi_home_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "~/.local/share/budi".to_string())
        );
    }
    println!("  Dashboard: {dashboard_url}");
    println!();
    match sync_result {
        Ok((files, msgs)) if files > 0 => {
            println!(
                "  Synced \x1b[1m{msgs}\x1b[0m messages from \x1b[1m{files}\x1b[0m transcript files."
            );
        }
        Ok(_) => {
            println!("  No existing transcripts found (data syncs automatically every 30s).");
        }
        Err(e) => {
            tracing::warn!("auto-sync failed: {e}");
            println!("  Auto-sync skipped (run `budi sync` manually).");
        }
    }
    println!();
    println!("  \x1b[1mNext steps:\x1b[0m");
    println!("    1. Open the dashboard: \x1b[4m{dashboard_url}\x1b[0m");
    println!("    2. Run `budi doctor` to verify everything is working");
    println!();

    open_url_in_browser(&dashboard_url);

    Ok(())
}

pub fn open_url_in_browser(url: &str) {
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

fn install_statusline_if_missing() {
    let Ok(home) = std::env::var("HOME") else {
        return;
    };
    let settings_path = PathBuf::from(&home).join(super::statusline::CLAUDE_USER_SETTINGS);
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

    if let Ok(()) = super::statusline::cmd_statusline_install() {
        eprintln!("Status line: installed in {}", settings_path.display());
    }
}

// ---------------------------------------------------------------------------
// Hook installation
// ---------------------------------------------------------------------------

/// The budi hook command string — same for all hook events.
const BUDI_HOOK_CMD: &str = "budi hook";

/// Install budi hooks for Claude Code and Cursor.
/// Merges with existing hooks — never overwrites non-budi entries.
fn install_hooks() {
    install_claude_code_hooks();
    install_cursor_hooks();
}

/// Install hooks into ~/.claude/settings.json.
/// Uses Claude Code's nested format: hooks → EventName → [{ matcher, hooks: [{ type, command }] }]
fn install_claude_code_hooks() {
    let Ok(home) = std::env::var("HOME") else {
        return;
    };
    let settings_path = PathBuf::from(&home).join(super::statusline::CLAUDE_USER_SETTINGS);

    let mut settings = if settings_path.exists() {
        fs::read_to_string(&settings_path)
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .unwrap_or_else(|| json!({}))
    } else {
        json!({})
    };
    if !settings.is_object() {
        settings = json!({});
    }

    let hooks = settings
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }

    let cc_events = [
        "SessionStart",
        "SessionEnd",
        "PostToolUse",
        "SubagentStop",
        "PreCompact",
        "Stop",
        "UserPromptSubmit",
    ];

    let budi_hook_entry = json!({
        "matcher": "",
        "hooks": [{
            "type": "command",
            "command": BUDI_HOOK_CMD,
            "async": true
        }]
    });

    let mut changed = false;
    for event in &cc_events {
        let event_arr = hooks
            .as_object_mut()
            .unwrap()
            .entry(*event)
            .or_insert_with(|| json!([]));
        if !event_arr.is_array() {
            *event_arr = json!([]);
        }

        // Check if budi hook already installed for this event
        let already_installed = event_arr.as_array().unwrap().iter().any(|entry| {
            // Check nested hooks array format
            entry
                .get("hooks")
                .and_then(|h| h.as_array())
                .map(|arr| {
                    arr.iter().any(|h| {
                        h.get("command")
                            .and_then(|c| c.as_str())
                            .is_some_and(|c| c.trim() == BUDI_HOOK_CMD)
                    })
                })
                .unwrap_or(false)
        });

        if !already_installed {
            event_arr.as_array_mut().unwrap().push(budi_hook_entry.clone());
            changed = true;
        }
    }

    if changed {
        if let Ok(out) = serde_json::to_string_pretty(&settings) {
            if fs::write(&settings_path, out).is_ok() {
                eprintln!("  Hooks: installed Claude Code hooks in {}", settings_path.display());
            }
        }
    }
}

/// Install hooks into ~/.cursor/hooks.json.
/// Uses Cursor's flat format: hooks → eventName → [{ command, type }]
fn install_cursor_hooks() {
    let Ok(home) = std::env::var("HOME") else {
        return;
    };
    let hooks_path = PathBuf::from(&home).join(".cursor/hooks.json");

    // Ensure directory exists
    if let Some(parent) = hooks_path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let mut config = if hooks_path.exists() {
        fs::read_to_string(&hooks_path)
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .unwrap_or_else(|| json!({"version": 1, "hooks": {}}))
    } else {
        json!({"version": 1, "hooks": {}})
    };

    if config.get("version").is_none() {
        config["version"] = json!(1);
    }
    if config.get("hooks").is_none() || !config["hooks"].is_object() {
        config["hooks"] = json!({});
    }

    let cursor_events = [
        "sessionStart",
        "sessionEnd",
        "postToolUse",
        "subagentStop",
        "preCompact",
        "stop",
        "afterFileEdit",
    ];

    let budi_hook_entry = json!({
        "command": BUDI_HOOK_CMD,
        "type": "command"
    });

    let mut changed = false;
    let hooks = config.get_mut("hooks").unwrap();

    for event in &cursor_events {
        let event_arr = hooks
            .as_object_mut()
            .unwrap()
            .entry(*event)
            .or_insert_with(|| json!([]));
        if !event_arr.is_array() {
            *event_arr = json!([]);
        }

        // Check if already installed
        let already_installed = event_arr.as_array().unwrap().iter().any(|entry| {
            entry
                .get("command")
                .and_then(|c| c.as_str())
                .is_some_and(|c| c.trim() == BUDI_HOOK_CMD)
        });

        if !already_installed {
            event_arr.as_array_mut().unwrap().push(budi_hook_entry.clone());
            changed = true;
        }
    }

    if changed {
        if let Ok(out) = serde_json::to_string_pretty(&config) {
            if fs::write(&hooks_path, out).is_ok() {
                eprintln!("  Hooks: installed Cursor hooks in {}", hooks_path.display());
            }
        }
    }
}
