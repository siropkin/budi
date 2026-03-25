use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use budi_core::config;
use reqwest::blocking::Client;
use serde_json::Value;

use crate::daemon::ensure_daemon_running;

pub fn cmd_update() -> Result<()> {
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

    // Clean up legacy hooks from settings.json
    crate::commands::statusline::remove_legacy_hooks();

    // Restart daemon with new version
    println!("Restarting daemon...");
    let _ = Command::new("pkill").args(["-f", "budi-daemon"]).status();
    thread::sleep(Duration::from_millis(500));

    {
        let repo_root = std::env::current_dir()
            .ok()
            .and_then(|cwd| config::find_repo_root(&cwd).ok());
        let config = match &repo_root {
            Some(root) => config::load_or_default(root).unwrap_or_default(),
            None => config::BudiConfig::default(),
        };
        let _ = ensure_daemon_running(repo_root.as_deref(), &config);
    }

    // Run database migration after updating binary (via daemon)
    println!("Running database migration...");
    match crate::client::DaemonClient::connect()
        .and_then(|c| c.migrate())
    {
        Ok(_) => println!("\x1b[32m✓\x1b[0m Database migrated."),
        Err(e) => println!("\x1b[33m!\x1b[0m Migration warning: {}", e),
    }

    println!("\x1b[32m✓\x1b[0m Updated to v{}.", latest);
    Ok(())
}
