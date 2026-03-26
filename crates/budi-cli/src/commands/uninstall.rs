use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};
use serde_json::Value;

use super::statusline::CLAUDE_USER_SETTINGS;

pub fn cmd_uninstall(keep_data: bool) -> Result<()> {
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
    let home = std::env::var("HOME").unwrap_or_default();
    if !home.is_empty() {
        print!("Removing Claude Code hooks... ");
        match remove_claude_code_hooks(&home) {
            Ok(true) => println!("{green}✓{reset} removed"),
            Ok(false) => println!("none found"),
            Err(e) => println!("{yellow}warning: {e}{reset}"),
        }

        // 3. Remove hooks from Cursor
        print!("Removing Cursor hooks... ");
        match remove_cursor_hooks(&home) {
            Ok(true) => println!("{green}✓{reset} removed"),
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
            print!("Removing data... ");
            match remove_data() {
                Ok(true) => println!("{green}✓{reset} removed"),
                Ok(false) => println!("none found"),
                Err(e) => println!("{yellow}warning: {e}{reset}"),
            }
        } else {
            println!("Keeping data (--keep-data)");
        }
    }

    println!();
    println!("{green}✓{reset} budi uninstalled.");
    println!();
    println!("To remove the binaries:");
    println!("  brew uninstall budi");
    println!("  # or: rm ~/.local/bin/budi ~/.local/bin/budi-daemon");

    Ok(())
}

fn stop_daemon() -> Result<bool> {
    let output = Command::new("pkill")
        .args(["-f", "budi-daemon"])
        .output()
        .context("failed to run pkill")?;
    Ok(output.status.success())
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
