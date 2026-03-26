use std::fs;
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
    if !home.is_empty() {
        print!("Removing Claude Code hooks... ");
        match remove_claude_code_hooks(&home) {
            Ok(true) => {
                // Verify removal by re-reading the file
                let settings_path = PathBuf::from(&home).join(CLAUDE_USER_SETTINGS);
                if verify_no_budi_hooks_cc(&settings_path) {
                    println!("{green}✓{reset} removed (verified)");
                } else {
                    println!("{yellow}✓{reset} removed but some budi hooks may remain — check {}", settings_path.display());
                }
            }
            Ok(false) => println!("none found"),
            Err(e) => println!("{yellow}warning: {e}{reset}"),
        }

        // 3. Remove hooks from Cursor
        print!("Removing Cursor hooks... ");
        match remove_cursor_hooks(&home) {
            Ok(true) => {
                let hooks_path = PathBuf::from(&home).join(".cursor/hooks.json");
                if verify_no_budi_hooks_cursor(&hooks_path) {
                    println!("{green}✓{reset} removed (verified)");
                } else {
                    println!("{yellow}✓{reset} removed but some budi hooks may remain — check {}", hooks_path.display());
                }
            }
            Ok(false) => println!("none found"),
            Err(e) => println!("{yellow}warning: {e}{reset}"),
        }

        // 4. Remove status line from Claude Code
        print!("Removing status line... ");
        match remove_statusline(&home) {
            Ok(true) => println!("{green}✓{reset} removed"),
            Ok(false) => println!("none found"),
            Err(e) => println!("{yellow}warning: {e}{reset}"),
        }

        // 5. Remove data
        if !keep_data {
            if !yes {
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

    println!();
    println!("{green}✓{reset} budi uninstalled.");
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
            .args(["-f", "budi-daemon serve"])
            .output()
            .context("failed to run pkill")?;
        Ok(output.status.success())
    }
}

fn remove_claude_code_hooks(home: &str) -> Result<bool> {
    let settings_path = PathBuf::from(home).join(CLAUDE_USER_SETTINGS);
    if !settings_path.exists() {
        return Ok(false);
    }

    let raw = fs::read_to_string(&settings_path)?;
    let mut settings: Value = serde_json::from_str(&raw)?;

    let Some(hooks) = settings.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
        return Ok(false);
    };

    let mut changed = false;
    let events: Vec<String> = hooks.keys().cloned().collect();
    for event in events {
        let Some(arr) = hooks.get_mut(&event).and_then(|v| v.as_array_mut()) else {
            continue;
        };

        let before = arr.len();
        arr.retain(|entry| !is_budi_hook_entry_cc(entry));
        if arr.len() < before {
            changed = true;
        }

        // Remove empty event arrays
        if arr.is_empty() {
            hooks.remove(&event);
        }
    }

    // Remove empty hooks object
    if hooks.is_empty() {
        settings
            .as_object_mut()
            .expect("settings is object")
            .remove("hooks");
    }

    if changed {
        let out = serde_json::to_string_pretty(&settings)?;
        fs::write(&settings_path, out)?;
    }

    Ok(changed)
}

fn remove_cursor_hooks(home: &str) -> Result<bool> {
    let hooks_path = PathBuf::from(home).join(".cursor/hooks.json");
    if !hooks_path.exists() {
        return Ok(false);
    }

    let raw = fs::read_to_string(&hooks_path)?;
    let mut config: Value = serde_json::from_str(&raw)?;

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
        arr.retain(|entry| !is_budi_hook_entry_cursor(entry));
        if arr.len() < before {
            changed = true;
        }

        if arr.is_empty() {
            hooks.remove(&event);
        }
    }

    if changed {
        let out = serde_json::to_string_pretty(&config)?;
        fs::write(&hooks_path, out)?;
    }

    Ok(changed)
}

fn remove_statusline(home: &str) -> Result<bool> {
    let settings_path = PathBuf::from(home).join(CLAUDE_USER_SETTINGS);
    if !settings_path.exists() {
        return Ok(false);
    }

    let raw = fs::read_to_string(&settings_path)?;
    let mut settings: Value = serde_json::from_str(&raw)?;

    let obj = settings
        .as_object_mut()
        .context("settings is not an object")?;
    if obj.remove("statusLine").is_none() {
        return Ok(false);
    }

    let out = serde_json::to_string_pretty(&settings)?;
    fs::write(&settings_path, out)?;
    Ok(true)
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

fn print_binary_removal_hint() {
    println!();
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

/// Check if a Claude Code hook entry contains a budi hook command (any variant).
fn is_budi_hook_entry_cc(entry: &Value) -> bool {
    entry
        .get("hooks")
        .and_then(|h| h.as_array())
        .map(|hooks| {
            hooks.iter().any(|h| {
                h.get("command")
                    .and_then(|c| c.as_str())
                    .is_some_and(is_budi_cmd)
            })
        })
        .unwrap_or(false)
}

/// Check if a Cursor hook entry is a budi hook command (any variant).
fn is_budi_hook_entry_cursor(entry: &Value) -> bool {
    entry
        .get("command")
        .and_then(|c| c.as_str())
        .is_some_and(is_budi_cmd)
}

/// Match any variant of the budi hook command (with or without `|| true` wrapper).
fn is_budi_cmd(cmd: &str) -> bool {
    let trimmed = cmd.trim();
    trimmed == "budi hook" || trimmed.starts_with("budi hook ")
}

/// Re-read Claude Code settings and confirm no budi hooks remain.
fn verify_no_budi_hooks_cc(path: &PathBuf) -> bool {
    let Ok(raw) = fs::read_to_string(path) else {
        return true; // file gone = hooks gone
    };
    let Ok(settings) = serde_json::from_str::<Value>(&raw) else {
        return true;
    };
    let Some(hooks) = settings.get("hooks").and_then(|h| h.as_object()) else {
        return true;
    };
    !hooks.values().any(|arr| {
        arr.as_array()
            .map(|a| a.iter().any(|e| is_budi_hook_entry_cc(e)))
            .unwrap_or(false)
    })
}

/// Re-read Cursor hooks and confirm no budi hooks remain.
fn verify_no_budi_hooks_cursor(path: &PathBuf) -> bool {
    let Ok(raw) = fs::read_to_string(path) else {
        return true;
    };
    let Ok(config) = serde_json::from_str::<Value>(&raw) else {
        return true;
    };
    let Some(hooks) = config.get("hooks").and_then(|h| h.as_object()) else {
        return true;
    };
    !hooks.values().any(|arr| {
        arr.as_array()
            .map(|a| a.iter().any(|e| is_budi_hook_entry_cursor(e)))
            .unwrap_or(false)
    })
}
