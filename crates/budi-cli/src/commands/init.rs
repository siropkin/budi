use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use budi_core::config;
use serde_json::{Value, json};

use crate::daemon::ensure_daemon_running;

pub fn cmd_init(local: bool, repo_root: Option<PathBuf>, no_daemon: bool, no_open: bool, no_sync: bool) -> Result<()> {
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

    let hook_warnings = install_hooks();
    if !hook_warnings.is_empty() {
        eprintln!("  Warning: hook installation had issues:");
        for w in &hook_warnings {
            eprintln!("    - {w}");
        }
        eprintln!("  Run `budi doctor` to diagnose.");
    }

    if !no_daemon {
        ensure_daemon_running(repo_root.as_deref(), &config)?;
    }

    // Auto-sync existing transcripts on first run
    let sync_result = if no_sync {
        Ok((0, 0))
    } else {
        super::sync::init_auto_sync()
    };

    let dashboard_url = format!("{}/dashboard", config.daemon_base_url());

    let bold_cyan = super::ansi("\x1b[1;36m");
    let bold = super::ansi("\x1b[1m");
    let underline = super::ansi("\x1b[4m");
    let reset = super::ansi("\x1b[0m");

    let is_reinit = repo_root.as_ref().map_or(false, |root| {
        config::repo_paths(root)
            .map(|p| p.data_dir.join("analytics.db").exists())
            .unwrap_or(false)
    });

    println!();
    if let Some(ref root) = repo_root {
        if is_reinit {
            println!(
                "{bold_cyan}  budi{reset} re-initialized in {}",
                root.display()
            );
        } else {
            println!(
                "{bold_cyan}  budi{reset} initialized in {}",
                root.display()
            );
        }
    } else {
        println!("{bold_cyan}  budi{reset} initialized (global)");
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
                "  Synced {bold}{msgs}{reset} messages from {bold}{files}{reset} transcript files."
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
    println!("  {bold}Next steps:{reset}");
    println!("    1. Open the dashboard: {underline}{dashboard_url}{reset}");
    println!("    2. Run `budi stats` to see your spending");
    println!();

    // Only open browser on fresh init (not re-init) and when --no-open is not set
    if !no_open && !is_reinit {
        open_url_in_browser(&dashboard_url);
    }

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
        println!("Status line: installed in {}", settings_path.display());
    }
}

// ---------------------------------------------------------------------------
// Hook installation
// ---------------------------------------------------------------------------

/// The budi hook command string — same for all hook events.
const BUDI_HOOK_CMD: &str = "budi hook";

/// Install budi hooks for Claude Code and Cursor.
/// Merges with existing hooks — never overwrites non-budi entries.
/// Returns a list of warning messages for any hooks that failed to install.
fn install_hooks() -> Vec<String> {
    let mut warnings = Vec::new();
    if let Err(e) = install_claude_code_hooks() {
        warnings.push(format!("Claude Code hooks: {e}"));
    }
    if let Err(e) = install_cursor_hooks() {
        warnings.push(format!("Cursor hooks: {e}"));
    }
    warnings
}

/// Install hooks into ~/.claude/settings.json.
/// Uses Claude Code's nested format: hooks → EventName → [{ matcher, hooks: [{ type, command }] }]
fn install_claude_code_hooks() -> Result<()> {
    let home = std::env::var("HOME").context("HOME not set")?;
    let settings_path = PathBuf::from(&home).join(super::statusline::CLAUDE_USER_SETTINGS);

    let mut settings = if settings_path.exists() {
        let raw = fs::read_to_string(&settings_path)
            .with_context(|| format!("Failed to read {}", settings_path.display()))?;
        serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    };
    if !settings.is_object() {
        settings = json!({});
    }

    let hooks_obj = settings
        .as_object_mut()
        .expect("settings is guaranteed to be object above");
    let hooks = hooks_obj.entry("hooks").or_insert_with(|| json!({}));
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
        let hooks_map = hooks
            .as_object_mut()
            .expect("hooks is guaranteed to be object above");
        let event_arr = hooks_map.entry(*event).or_insert_with(|| json!([]));
        if !event_arr.is_array() {
            *event_arr = json!([]);
        }

        // Check if budi hook already installed for this event
        let arr = event_arr.as_array().expect("event_arr is guaranteed to be array above");
        let already_installed = arr.iter().any(|entry| {
            entry
                .get("hooks")
                .and_then(|h| h.as_array())
                .map(|hooks| {
                    hooks.iter().any(|h| {
                        h.get("command")
                            .and_then(|c| c.as_str())
                            .is_some_and(|c| c.trim() == BUDI_HOOK_CMD)
                    })
                })
                .unwrap_or(false)
        });

        if !already_installed {
            event_arr.as_array_mut().expect("checked above").push(budi_hook_entry.clone());
            changed = true;
        }
    }

    if changed {
        let out = serde_json::to_string_pretty(&settings)?;
        fs::write(&settings_path, out)
            .with_context(|| format!("Failed to write {}", settings_path.display()))?;
        println!("  Hooks: installed Claude Code hooks in {}", settings_path.display());
    } else {
        println!("  Hooks: Claude Code hooks already installed");
    }
    Ok(())
}

/// Install hooks into ~/.cursor/hooks.json.
/// Uses Cursor's flat format: hooks → eventName → [{ command, type }]
fn install_cursor_hooks() -> Result<()> {
    let home = std::env::var("HOME").context("HOME not set")?;
    let hooks_path = PathBuf::from(&home).join(".cursor/hooks.json");

    // Ensure directory exists
    if let Some(parent) = hooks_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }

    let mut config = if hooks_path.exists() {
        let raw = fs::read_to_string(&hooks_path)
            .with_context(|| format!("Failed to read {}", hooks_path.display()))?;
        serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| json!({"version": 1, "hooks": {}}))
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
    let hooks = config.get_mut("hooks").expect("hooks key guaranteed above");

    for event in &cursor_events {
        let hooks_map = hooks.as_object_mut().expect("hooks is guaranteed to be object above");
        let event_arr = hooks_map.entry(*event).or_insert_with(|| json!([]));
        if !event_arr.is_array() {
            *event_arr = json!([]);
        }

        let arr = event_arr.as_array().expect("event_arr is guaranteed to be array above");
        let already_installed = arr.iter().any(|entry| {
            entry
                .get("command")
                .and_then(|c| c.as_str())
                .is_some_and(|c| c.trim() == BUDI_HOOK_CMD)
        });

        if !already_installed {
            event_arr.as_array_mut().expect("checked above").push(budi_hook_entry.clone());
            changed = true;
        }
    }

    if changed {
        let out = serde_json::to_string_pretty(&config)?;
        fs::write(&hooks_path, out)
            .with_context(|| format!("Failed to write {}", hooks_path.display()))?;
        println!("  Hooks: installed Cursor hooks in {}", hooks_path.display());
    } else {
        println!("  Hooks: Cursor hooks already installed");
    }
    Ok(())
}
