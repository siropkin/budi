use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::Result;
use budi_core::config;
use serde_json::Value;

use crate::daemon::ensure_daemon_running;

pub fn cmd_init(repo_root: Option<PathBuf>, no_daemon: bool) -> Result<()> {
    let repo_root = super::resolve_repo_root(repo_root)?;
    let config = config::load_or_default(&repo_root)?;
    config::ensure_repo_layout(&repo_root)?;
    config::save(&repo_root, &config)?;

    install_statusline_if_missing();

    if !no_daemon {
        ensure_daemon_running(&repo_root, &config)?;
    }

    // Auto-sync existing transcripts on first run
    let sync_result = super::sync::init_auto_sync();

    let dashboard_url = format!("{}/dashboard", config.daemon_base_url());

    println!();
    println!(
        "\x1b[1;36m  budi\x1b[0m initialized in {}",
        repo_root.display()
    );
    println!();
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
