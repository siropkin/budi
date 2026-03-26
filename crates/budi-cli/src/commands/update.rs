use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use budi_core::config;
use reqwest::blocking::Client;
use serde_json::Value;

use crate::daemon::ensure_daemon_running;

pub fn cmd_update(yes: bool) -> Result<()> {
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

    let green = super::ansi("\x1b[32m");
    let bold = super::ansi("\x1b[1m");
    let bold_green = super::ansi("\x1b[1;32m");
    let yellow = super::ansi("\x1b[33m");
    let reset = super::ansi("\x1b[0m");

    if latest == current {
        println!("{green}✓{reset} Already up to date (v{}).", current);
        return Ok(());
    }

    println!(
        "New version available: {bold}v{}{reset} → {bold_green}v{}{reset}",
        current, latest
    );

    if !yes {
        println!("This will download and run the budi installer from GitHub.");
        eprint!("Continue? [y/N] ");
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer).context("Failed to read stdin")?;
        if !matches!(answer.trim(), "y" | "Y") {
            println!("Aborted.");
            return Ok(());
        }
    }

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
        Ok(_) => println!("{green}✓{reset} Database migrated."),
        Err(e) => println!("{yellow}!{reset} Migration warning: {}", e),
    }

    // Verify installed version
    match Command::new("budi").arg("--version").output() {
        Ok(output) if output.status.success() => {
            let installed = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let installed_ver = installed.strip_prefix("budi ").unwrap_or(&installed);
            if installed_ver.contains(latest) {
                println!("{green}✓{reset} Updated to v{}.", latest);
            } else {
                println!("{yellow}!{reset} Expected v{}, but `budi --version` reports: {}", latest, installed);
            }
        }
        _ => {
            println!("{green}✓{reset} Updated to v{} (could not verify installed version).", latest);
        }
    }
    Ok(())
}
