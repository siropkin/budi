use std::io::IsTerminal;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use budi_core::config;
use reqwest::blocking::Client;
use serde_json::Value;

use crate::daemon::ensure_daemon_running;

pub fn cmd_update(yes: bool, version: Option<String>) -> Result<()> {
    // Detect Homebrew installation and redirect.
    if is_homebrew_install() {
        let bold = super::ansi("\x1b[1m");
        let reset = super::ansi("\x1b[0m");
        println!("budi was installed via {bold}Homebrew{reset}.");
        println!();
        println!("To update, run:");
        println!("  brew upgrade budi");
        println!("  budi init        # restart daemon + re-sync");
        return Ok(());
    }

    let current = env!("CARGO_PKG_VERSION");
    println!("Current version: v{}", current);

    let green = super::ansi("\x1b[32m");
    let bold = super::ansi("\x1b[1m");
    let bold_green = super::ansi("\x1b[1;32m");
    let yellow = super::ansi("\x1b[33m");
    let dim = super::ansi("\x1b[90m");
    let reset = super::ansi("\x1b[0m");

    // Resolve target version — either from --version flag or GitHub API.
    let (latest_tag, latest) = if let Some(ref v) = version {
        let tag = if v.starts_with('v') {
            v.clone()
        } else {
            format!("v{v}")
        };
        let ver = tag.strip_prefix('v').unwrap_or(&tag).to_string();
        println!("Target version: v{}", ver);
        (tag, ver)
    } else {
        println!("Checking for updates...");
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;
        let resp = client
            .get("https://api.github.com/repos/siropkin/budi/releases/latest")
            .header("User-Agent", "budi-cli")
            .send()
            .context("Failed to check for updates")?;

        if !resp.status().is_success() {
            anyhow::bail!("GitHub API returned {}", resp.status());
        }

        let release: Value = resp.json()?;
        let tag = release
            .get("tag_name")
            .and_then(|v| v.as_str())
            .context("Could not parse release tag")?
            .to_string();
        let ver = tag.strip_prefix('v').unwrap_or(&tag).to_string();
        (tag, ver)
    };

    if latest == current && version.is_none() {
        println!("{green}✓{reset} Already up to date (v{}).", current);
        return Ok(());
    }

    println!(
        "New version available: {bold}v{}{reset} → {bold_green}v{}{reset}",
        current, latest
    );

    if !yes {
        println!("This will download and run the budi installer from GitHub.");
        if std::io::stdin().is_terminal() {
            eprint!("Continue? [y/N] ");
            let mut answer = String::new();
            std::io::stdin()
                .read_line(&mut answer)
                .context("Failed to read stdin")?;
            if !matches!(answer.trim(), "y" | "Y") {
                println!("Aborted.");
                return Ok(());
            }
        } else {
            anyhow::bail!("Non-interactive terminal. Use `budi update --yes` to skip confirmation.");
        }
    }

    // Stop daemon BEFORE running the installer.
    // Required on Windows where running executables cannot be overwritten.
    println!("Stopping daemon...");
    stop_all_daemons();
    thread::sleep(Duration::from_millis(500));

    println!("Updating...");

    // Pin the installer to the exact version we resolved to avoid race conditions
    // (a new release published between version check and download).
    let status = if cfg!(target_os = "windows") {
        Command::new("powershell")
            .env("VERSION", &latest_tag)
            .args([
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                "irm https://raw.githubusercontent.com/siropkin/budi/main/scripts/install-standalone.ps1 | iex",
            ])
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .context("Failed to run PowerShell installer")?
    } else {
        Command::new("bash")
            .env("VERSION", &latest_tag)
            .args([
                "-c",
                "curl -fsSL https://raw.githubusercontent.com/siropkin/budi/main/scripts/install-standalone.sh | bash",
            ])
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .context("Failed to run installer")?
    };

    if !status.success() {
        // Installer failed — try to restart the daemon so the old version keeps working
        eprintln!("Installer failed. Attempting to restart daemon with current binaries...");
        let repo_root = std::env::current_dir()
            .ok()
            .and_then(|cwd| config::find_repo_root(&cwd).ok());
        let cfg = match &repo_root {
            Some(root) => config::load_or_default(root).unwrap_or_default(),
            None => config::BudiConfig::default(),
        };
        let _ = ensure_daemon_running(repo_root.as_deref(), &cfg);
        anyhow::bail!("Installer exited with {}", status);
    }

    // Clean up legacy hooks from settings.json
    crate::commands::statusline::remove_legacy_hooks();

    // Run database migration before restarting daemon — migration in a
    // standalone process is fast vs slow inside the daemon's Tokio runtime.
    println!("Running database migration...");
    if let Ok(db_path) = budi_core::analytics::db_path() {
        if db_path.exists() && budi_core::migration::needs_migration_at(&db_path) {
            match budi_core::analytics::open_db_with_migration(&db_path) {
                Ok(_) => println!("{green}✓{reset} Database migrated."),
                Err(e) => println!("{yellow}!{reset} Migration warning: {}", e),
            }
        } else {
            println!("{green}✓{reset} Database up to date.");
        }
    }

    // Restart daemon with new version
    println!("Restarting daemon...");
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

    // Verify installed version
    match Command::new("budi").arg("--version").output() {
        Ok(output) if output.status.success() => {
            let installed = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let installed_ver = installed.strip_prefix("budi ").unwrap_or(&installed);
            if installed_ver == latest || installed_ver == format!("v{latest}") {
                println!("{green}✓{reset} Updated to v{}.", latest);
            } else {
                println!(
                    "{yellow}!{reset} Expected v{}, but `budi --version` reports: {}",
                    latest, installed
                );
            }
        }
        _ => {
            println!(
                "{green}✓{reset} Updated to v{} (could not verify installed version).",
                latest
            );
        }
    }

    println!();
    println!("{dim}Restart Claude Code and Cursor to pick up any changes.{reset}");

    Ok(())
}

/// Check if budi was installed via Homebrew by examining the executable path.
fn is_homebrew_install() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok())
        .map(|p| {
            let s = p.to_string_lossy().to_lowercase();
            s.contains("/cellar/") || s.contains("/homebrew/")
        })
        .unwrap_or(false)
}

/// Stop all budi-daemon processes using platform-appropriate methods.
fn stop_all_daemons() {
    if cfg!(target_os = "windows") {
        let _ = Command::new("taskkill")
            .args(["/F", "/IM", "budi-daemon.exe"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    } else {
        let _ = Command::new("pkill")
            .args(["-f", "budi-daemon serve"])
            .status();
    }
}
