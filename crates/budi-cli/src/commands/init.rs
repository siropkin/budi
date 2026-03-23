use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use budi_core::config::{self, CLAUDE_LOCAL_SETTINGS};
use serde_json::{Value, json};

use crate::daemon::ensure_daemon_running;

pub fn cmd_init(repo_root: Option<PathBuf>, no_daemon: bool, global: bool) -> Result<()> {
    let repo_root = super::resolve_repo_root(repo_root)?;
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
    install_starship_if_detected();

    if !no_daemon {
        ensure_daemon_running(&repo_root, &config)?;
    }

    // Auto-sync existing transcripts on first run
    let sync_result = super::sync::init_auto_sync();

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
    let settings_path = PathBuf::from(home).join(super::statusline::CLAUDE_USER_SETTINGS);
    write_hooks_to_settings(&settings_path)?;
    Ok(settings_path)
}

// ─── Starship ────────────────────────────────────────────────────────────────

pub fn is_starship_installed() -> bool {
    Command::new("which")
        .arg("starship")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

pub fn starship_config_path() -> PathBuf {
    if let Ok(p) = std::env::var("STARSHIP_CONFIG") {
        return PathBuf::from(p);
    }
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join("starship.toml");
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".config/starship.toml")
}

pub fn is_budi_configured_in_starship() -> bool {
    let path = starship_config_path();
    fs::read_to_string(&path)
        .unwrap_or_default()
        .contains("[custom.budi]")
}

const STARSHIP_BUDI_MODULE: &str = r#"
# Budi — AI code analytics (budi statusline --format=starship)
[custom.budi]
command = "budi statusline --format=starship"
when = "command -v budi-daemon"
format = "[$output]($style) "
style = "cyan"
shell = ["sh"]
"#;

fn install_starship_module() -> Result<()> {
    let path = starship_config_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed creating {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("Failed opening {}", path.display()))?;
    file.write_all(STARSHIP_BUDI_MODULE.as_bytes())
        .with_context(|| format!("Failed writing {}", path.display()))?;
    Ok(())
}

fn install_starship_if_detected() {
    if !is_starship_installed() {
        return;
    }
    if is_budi_configured_in_starship() {
        return;
    }
    match install_starship_module() {
        Ok(()) => {
            eprintln!(
                "Starship: installed budi module in {}",
                starship_config_path().display()
            );
        }
        Err(e) => {
            tracing::warn!("Failed to install Starship module: {e}");
        }
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
