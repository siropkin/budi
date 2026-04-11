use std::fs;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};
use serde_json::Value;

use super::statusline::CLAUDE_USER_SETTINGS;

pub fn cmd_uninstall(keep_data: bool, yes: bool) -> Result<()> {
    let green = super::ansi("\x1b[32m");
    let yellow = super::ansi("\x1b[33m");
    let reset = super::ansi("\x1b[0m");

    // 1. Stop the daemon
    print!("Stopping daemon... ");
    match stop_daemon() {
        Ok(true) => println!("{green}✓{reset} stopped"),
        Ok(false) => println!("{yellow}not running{reset}"),
        Err(e) => println!("{yellow}warning: {e}{reset}"),
    }

    // 2. Remove hooks from Claude Code
    let home = budi_core::config::home_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    if home.is_empty() {
        eprintln!(
            "{yellow}warning: could not determine home directory — skipping hook/config cleanup{reset}"
        );
    } else {
        // 2-6. Remove Claude Code integrations (single file pass)
        match remove_all_from_claude_code(&home) {
            Ok((hooks, otel, mcp, statusline)) => {
                let label = |removed| {
                    if removed {
                        format!("{green}✓{reset} removed")
                    } else {
                        "none found".to_string()
                    }
                };
                println!("Removing Claude Code hooks... {}", label(hooks));
                println!("Removing OTEL env vars... {}", label(otel));
                println!("Removing MCP server... {}", label(mcp));
                println!("Removing status line... {}", label(statusline));
            }
            Err(e) => {
                println!("Removing Claude Code integrations... {yellow}warning: {e}{reset}");
            }
        }

        // 3. Remove hooks from Cursor
        print!("Removing Cursor hooks... ");
        match remove_cursor_hooks(&home) {
            Ok(true) => {
                let hooks_path = PathBuf::from(&home).join(".cursor/hooks.json");
                if verify_no_budi_hooks_cursor(&hooks_path) {
                    println!("{green}✓{reset} removed (verified)");
                } else {
                    println!(
                        "{yellow}✓{reset} removed but some budi hooks may remain — check {}",
                        hooks_path.display()
                    );
                }
            }
            Ok(false) => println!("none found"),
            Err(e) => println!("{yellow}warning: {e}{reset}"),
        }

        // 7. Remove data
        if !keep_data {
            if !yes {
                if !std::io::stdin().is_terminal() {
                    anyhow::bail!(
                        "Non-interactive terminal. Use `budi uninstall --yes` to skip confirmation."
                    );
                }
                eprint!("Remove all analytics data and config? This cannot be undone. [y/N] ");
                let mut answer = String::new();
                std::io::stdin()
                    .read_line(&mut answer)
                    .context("Failed to read stdin")?;
                if !matches!(answer.trim(), "y" | "Y") {
                    println!("Keeping data.");
                    print_binary_removal_hint();
                    return Ok(());
                }
            }
            print!("Removing data... ");
            match remove_data() {
                Ok(true) => println!("{green}✓{reset} removed"),
                Ok(false) => println!("none found"),
                Err(e) => println!("{yellow}warning: {e}{reset}"),
            }
            print!("Removing config... ");
            match remove_config() {
                Ok(true) => println!("{green}✓{reset} removed"),
                Ok(false) => println!("none found"),
                Err(e) => println!("{yellow}warning: {e}{reset}"),
            }
        } else {
            println!("Keeping data and config (--keep-data)");
        }
    }

    // Remove macOS LaunchAgents if present
    #[cfg(target_os = "macos")]
    {
        print!("Removing LaunchAgents... ");
        match remove_launch_agents() {
            Ok(true) => println!("{green}✓{reset} removed"),
            Ok(false) => println!("none found"),
            Err(e) => println!("{yellow}warning: {e}{reset}"),
        }
    }

    println!();
    println!("{green}✓{reset} budi uninstalled.");
    println!();
    let bold = super::ansi("\x1b[1m");
    println!("{bold}Important:{reset} Binaries are still installed. Remove them manually:");
    print_binary_removal_hint();

    Ok(())
}

fn stop_daemon() -> Result<bool> {
    if cfg!(target_os = "windows") {
        let output = Command::new("taskkill")
            .args(["/F", "/IM", "budi-daemon.exe"])
            .output()
            .context("failed to run taskkill")?;
        Ok(output.status.success())
    } else {
        let output = Command::new("pkill")
            .args(["-f", "budi-daemon"])
            .output()
            .context("failed to run pkill")?;
        Ok(output.status.success())
    }
}

/// Remove all budi integrations from ~/.claude/settings.json in a single read-modify-write pass.
/// Returns (hooks_removed, otel_removed, mcp_removed, statusline_removed).
fn remove_all_from_claude_code(home: &str) -> Result<(bool, bool, bool, bool)> {
    let settings_path = PathBuf::from(home).join(CLAUDE_USER_SETTINGS);
    if !settings_path.exists() {
        return Ok((false, false, false, false));
    }

    let mut settings = super::read_json_or_default(&settings_path)?;

    // 1. Remove hooks
    let hooks_removed = {
        let mut changed = false;
        if let Some(hooks) = settings.get_mut("hooks").and_then(|h| h.as_object_mut()) {
            let events: Vec<String> = hooks.keys().cloned().collect();
            for event in events {
                if let Some(arr) = hooks.get_mut(&event).and_then(|v| v.as_array_mut()) {
                    let before = arr.len();
                    arr.retain(|entry| !super::is_budi_cc_hook_entry(entry));
                    if arr.len() < before {
                        changed = true;
                    }
                    if arr.is_empty() {
                        hooks.remove(&event);
                    }
                }
            }
            if hooks.is_empty()
                && let Some(obj) = settings.as_object_mut()
            {
                obj.remove("hooks");
            }
        }
        changed
    };

    // 2. Remove OTEL env vars
    let otel_removed = {
        let mut changed = false;
        let is_budi_endpoint = settings
            .get("env")
            .and_then(|e| e.as_object())
            .is_some_and(|env| {
                let endpoint_local = env
                    .get("OTEL_EXPORTER_OTLP_ENDPOINT")
                    .and_then(|v| v.as_str())
                    .is_some_and(|url| {
                        let lower = url.to_lowercase();
                        lower.contains("127.0.0.1") || lower.contains("localhost")
                    });
                endpoint_local
                    && env
                        .get("CLAUDE_CODE_ENABLE_TELEMETRY")
                        .and_then(|v| v.as_str())
                        == Some("1")
                    && env
                        .get("OTEL_EXPORTER_OTLP_PROTOCOL")
                        .and_then(|v| v.as_str())
                        == Some("http/json")
            });
        if is_budi_endpoint
            && let Some(env) = settings.get_mut("env").and_then(|e| e.as_object_mut())
        {
            for key in &[
                "CLAUDE_CODE_ENABLE_TELEMETRY",
                "OTEL_EXPORTER_OTLP_ENDPOINT",
                "OTEL_EXPORTER_OTLP_PROTOCOL",
                "OTEL_METRICS_EXPORTER",
                "OTEL_LOGS_EXPORTER",
            ] {
                if env.remove(*key).is_some() {
                    changed = true;
                }
            }
            if env.is_empty()
                && let Some(obj) = settings.as_object_mut()
            {
                obj.remove("env");
            }
        }
        changed
    };

    // 3. Remove MCP server
    let mcp_removed = {
        let removed = settings
            .get_mut("mcpServers")
            .and_then(|m| m.as_object_mut())
            .and_then(|mcp| mcp.remove("budi"))
            .is_some();
        if removed
            && let Some(mcp) = settings.get("mcpServers").and_then(|m| m.as_object())
            && mcp.is_empty()
            && let Some(obj) = settings.as_object_mut()
        {
            obj.remove("mcpServers");
        }
        removed
    };

    // 4. Remove statusline
    let statusline_removed = settings
        .as_object_mut()
        .and_then(|obj| obj.remove("statusLine"))
        .is_some();

    if hooks_removed || otel_removed || mcp_removed || statusline_removed {
        super::atomic_write_json(&settings_path, &settings)?;
    }

    Ok((hooks_removed, otel_removed, mcp_removed, statusline_removed))
}

fn remove_cursor_hooks(home: &str) -> Result<bool> {
    let hooks_path = PathBuf::from(home).join(".cursor/hooks.json");
    if !hooks_path.exists() {
        return Ok(false);
    }

    let mut config = super::read_json_or_default(&hooks_path)?;

    let Some(hooks) = config.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
        return Ok(false);
    };

    let mut changed = false;
    let events: Vec<String> = hooks.keys().cloned().collect();
    for event in events {
        let Some(arr) = hooks.get_mut(&event).and_then(|v| v.as_array_mut()) else {
            continue;
        };

        let before = arr.len();
        arr.retain(|entry| !super::is_budi_cursor_hook_entry(entry));
        if arr.len() < before {
            changed = true;
        }

        if arr.is_empty() {
            hooks.remove(&event);
        }
    }

    if changed {
        super::atomic_write_json(&hooks_path, &config)?;
    }

    Ok(changed)
}

fn remove_data() -> Result<bool> {
    let data_dir = budi_core::config::budi_home_dir()?;
    if !data_dir.exists() {
        return Ok(false);
    }
    fs::remove_dir_all(&data_dir)?;
    Ok(true)
}

fn remove_config() -> Result<bool> {
    let config_dir = budi_core::config::budi_config_dir()?;
    if !config_dir.exists() {
        return Ok(false);
    }
    fs::remove_dir_all(&config_dir)?;
    Ok(true)
}

/// Remove macOS LaunchAgent plists for budi.
#[cfg(target_os = "macos")]
fn remove_launch_agents() -> Result<bool> {
    let home = budi_core::config::home_dir()?;
    let launch_agents_dir = home.join("Library/LaunchAgents");
    if !launch_agents_dir.is_dir() {
        return Ok(false);
    }
    let mut removed_any = false;
    for entry in fs::read_dir(&launch_agents_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with("com.siropkin.budi.") && name_str.ends_with(".plist") {
            // Try to unload first
            let _ = Command::new("launchctl")
                .args(["unload", &entry.path().to_string_lossy()])
                .output();
            fs::remove_file(entry.path())?;
            removed_any = true;
        }
    }
    Ok(removed_any)
}

fn print_binary_removal_hint() {
    println!("To remove the binaries:");
    if cfg!(target_os = "windows") {
        let bin_dir = std::env::var("LOCALAPPDATA")
            .map(|d| format!("{}\\budi\\bin", d))
            .unwrap_or_else(|_| "%LOCALAPPDATA%\\budi\\bin".to_string());
        println!("  # If installed via PowerShell:");
        println!("  Remove-Item -Recurse -Force \"{}\"", bin_dir);
    } else {
        println!("  # If installed via Homebrew:");
        println!("  brew uninstall budi");
        println!("  # If installed via shell script:");
        println!("  rm ~/.local/bin/budi ~/.local/bin/budi-daemon");
    }
}

/// Re-read Cursor hooks and confirm no budi hooks remain.
fn verify_no_budi_hooks_cursor(path: &PathBuf) -> bool {
    let Ok(raw) = fs::read_to_string(path) else {
        return true;
    };
    let Ok(config) = serde_json::from_str::<Value>(&raw) else {
        return false;
    };
    let Some(hooks) = config.get("hooks").and_then(|h| h.as_object()) else {
        return true;
    };
    !hooks.values().any(|arr| {
        arr.as_array()
            .map(|a| a.iter().any(super::is_budi_cursor_hook_entry))
            .unwrap_or(false)
    })
}
