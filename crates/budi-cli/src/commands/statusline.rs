use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use budi_core::config;
use serde_json::{Value, json};

use crate::StatuslineFormat;
use crate::daemon::daemon_client_with_timeout;

pub const CLAUDE_USER_SETTINGS: &str = ".claude/settings.json";

pub fn cmd_statusline(format: StatuslineFormat) -> Result<()> {
    let mut input = String::new();
    let _ = io::stdin().read_to_string(&mut input);

    let stdin_json = serde_json::from_str::<Value>(&input).ok();

    let cwd = stdin_json
        .as_ref()
        .and_then(|v| v.get("cwd").and_then(|c| c.as_str()).map(String::from))
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.display().to_string())
        });

    let repo_root = cwd
        .as_deref()
        .and_then(|c| config::find_repo_root(Path::new(c)).ok());

    let repo_initialized = repo_root
        .as_ref()
        .is_some_and(|root| root.join(".claude/settings.local.json").exists());

    let base = format!(
        "http://{}:{}",
        config::DEFAULT_DAEMON_HOST,
        config::DEFAULT_DAEMON_PORT,
    );

    // For starship/json: output nothing on error (Starship hides empty modules)
    if !repo_initialized {
        if format == StatuslineFormat::Claude {
            let budi_label = "\x1b[36m📊 budi\x1b[0m";
            println!("{} \x1b[90m· not set up\x1b[0m", budi_label);
        }
        return Ok(());
    }

    // Shorter timeout for shell prompts to avoid blocking the prompt
    let timeout = match format {
        StatuslineFormat::Starship => Duration::from_millis(300),
        _ => Duration::from_secs(3),
    };
    let client = daemon_client_with_timeout(timeout);
    let statusline_url = format!("{}/analytics/statusline", base);
    let statusline_data: Option<Value> = client
        .get(&statusline_url)
        .send()
        .ok()
        .filter(|r| r.status().is_success())
        .and_then(|r| r.json::<Value>().ok());

    let today_cost: f64 = statusline_data
        .as_ref()
        .and_then(|v| v.get("today_cost").and_then(|c| c.as_f64()))
        .unwrap_or(0.0);
    let week_cost: f64 = statusline_data
        .as_ref()
        .and_then(|v| v.get("week_cost").and_then(|c| c.as_f64()))
        .unwrap_or(0.0);
    let month_cost: f64 = statusline_data
        .as_ref()
        .and_then(|v| v.get("month_cost").and_then(|c| c.as_f64()))
        .unwrap_or(0.0);

    // Format cost like the dashboard: $1.2K, $123, $12.50, $0.42, $0
    fn fmt_cost(c: f64) -> String {
        if c >= 1000.0 {
            format!("${:.1}K", c / 1000.0)
        } else if c >= 100.0 {
            format!("${:.0}", c)
        } else if c > 0.0 {
            format!("${:.2}", c)
        } else {
            "$0".to_string()
        }
    }

    match format {
        StatuslineFormat::Json => {
            println!(
                "{}",
                json!({
                    "today_cost": today_cost,
                    "week_cost": week_cost,
                    "month_cost": month_cost,
                })
            );
        }
        StatuslineFormat::Starship => {
            // Compact plain text — Starship wraps with its own styling
            println!(
                "{} · {} · {}",
                fmt_cost(today_cost),
                fmt_cost(week_cost),
                fmt_cost(month_cost),
            );
        }
        StatuslineFormat::Claude => {
            let dashboard_url = format!("{}/dashboard", base);
            let budi_label = "\x1b[36m📊 budi\x1b[0m";
            let dashboard_link = format!(
                "\x1b]8;;{}\x1b\\\x1b[36m↗ dashboard\x1b[0m\x1b]8;;\x1b\\",
                dashboard_url,
            );
            let dim = "\x1b[90m";
            let reset = "\x1b[0m";
            let yellow = "\x1b[33m";

            let mut parts: Vec<String> = Vec::new();
            parts.push(format!("{yellow}{}{reset} today", fmt_cost(today_cost)));
            parts.push(format!("{yellow}{}{reset} week", fmt_cost(week_cost)));
            parts.push(format!("{yellow}{}{reset} month", fmt_cost(month_cost)));

            let joined = parts.join(&format!(" {dim}·{reset} "));
            println!("{budi_label} {dim}·{reset} {joined} {dim}·{reset} {dashboard_link}");
        }
    }

    Ok(())
}

pub fn cmd_statusline_install() -> Result<()> {
    let home = std::env::var("HOME").context("HOME not set")?;
    let settings_path = PathBuf::from(&home).join(CLAUDE_USER_SETTINGS);
    if let Some(parent) = settings_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed creating {}", parent.display()))?;
    }
    let mut settings = if settings_path.exists() {
        let raw = fs::read_to_string(&settings_path)
            .with_context(|| format!("Failed reading {}", settings_path.display()))?;
        serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    };
    if !settings.is_object() {
        settings = json!({});
    }
    settings["statusLine"] = json!({
        "type": "command",
        "command": "budi statusline",
        "padding": 0
    });
    let raw = serde_json::to_string_pretty(&settings)?;
    fs::write(&settings_path, raw)
        .with_context(|| format!("Failed writing {}", settings_path.display()))?;
    eprintln!("Installed budi status line in {}", settings_path.display());
    Ok(())
}
