use std::collections::HashMap;
use std::fs;
use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};
use budi_core::config;
use serde_json::{Value, json};

use crate::StatuslineFormat;
use crate::daemon::daemon_client_with_timeout;

pub const CLAUDE_USER_SETTINGS: &str = ".claude/settings.json";

/// Format a cost value like the dashboard: $1.2K, $123, $12.50, $0.42, $0
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

/// Detect the current git branch from a directory.
fn detect_git_branch(dir: &str) -> Option<String> {
    Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(dir)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            let branch = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if branch.is_empty() || branch == "HEAD" {
                None
            } else {
                Some(branch)
            }
        })
}

/// Build a map of slot name → display value from the daemon response.
fn build_slot_values(data: &Value) -> HashMap<String, String> {
    let mut vals = HashMap::new();

    let get_cost = |key: &str| -> f64 { data.get(key).and_then(|v| v.as_f64()).unwrap_or(0.0) };

    vals.insert("today".to_string(), fmt_cost(get_cost("today_cost")));
    vals.insert("week".to_string(), fmt_cost(get_cost("week_cost")));
    vals.insert("month".to_string(), fmt_cost(get_cost("month_cost")));

    if let Some(v) = data.get("session_cost").and_then(|v| v.as_f64()) {
        vals.insert("session".to_string(), fmt_cost(v));
    }
    if let Some(v) = data.get("branch_cost").and_then(|v| v.as_f64()) {
        vals.insert("branch".to_string(), fmt_cost(v));
    }
    if let Some(v) = data.get("project_cost").and_then(|v| v.as_f64()) {
        vals.insert("project".to_string(), fmt_cost(v));
    }
    if let Some(v) = data.get("active_provider").and_then(|v| v.as_str()) {
        vals.insert("provider".to_string(), v.to_string());
    }

    vals
}

/// Render a custom format template by replacing {slot} placeholders.
pub fn render_template(template: &str, values: &HashMap<String, String>) -> String {
    let mut result = template.to_string();
    for (key, val) in values {
        result = result.replace(&format!("{{{key}}}"), val);
    }
    result
}

/// Render slots as a separator-joined string.
fn render_slots(slots: &[String], values: &HashMap<String, String>, sep: &str) -> String {
    slots
        .iter()
        .filter_map(|slot| values.get(slot).map(|v| format!("{v} {slot}")))
        .collect::<Vec<_>>()
        .join(sep)
}

pub fn cmd_statusline(format: StatuslineFormat) -> Result<()> {
    let stdin_json = if io::stdin().is_terminal() {
        None
    } else {
        let mut input = String::new();
        let _ = io::stdin().read_to_string(&mut input);
        serde_json::from_str::<Value>(&input).ok()
    };

    let cwd = stdin_json
        .as_ref()
        .and_then(|v| v.get("cwd").and_then(|c| c.as_str()).map(String::from))
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.display().to_string())
        });

    let session_id = stdin_json.as_ref().and_then(|v| {
        v.get("session_id")
            .and_then(|s| s.as_str())
            .map(String::from)
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

    // For starship/json/custom: output nothing on error (Starship hides empty modules)
    if !repo_initialized {
        if format == StatuslineFormat::Claude {
            let budi_label = "\x1b[36m📊 budi\x1b[0m";
            println!("{} \x1b[90m· not set up\x1b[0m", budi_label);
        }
        return Ok(());
    }

    // Load statusline config (for Claude/Custom formats, and to determine needed query params)
    let sl_config = config::load_statusline_config();
    let needed = sl_config.required_slots();

    // Detect git branch if needed
    let branch = if needed.contains(&"branch".to_string()) {
        cwd.as_deref().and_then(detect_git_branch)
    } else {
        None
    };

    // Build query params for the daemon
    let mut query_params: Vec<(&str, String)> = Vec::new();
    if let Some(ref sid) = session_id
        && needed.contains(&"session".to_string())
    {
        query_params.push(("session_id", sid.clone()));
    }
    if let Some(ref b) = branch {
        query_params.push(("branch", b.clone()));
    }
    if let Some(ref root) = repo_root
        && needed.contains(&"project".to_string())
    {
        query_params.push(("project_dir", root.display().to_string()));
    }

    // Shorter timeout for shell prompts to avoid blocking the prompt
    let timeout = match format {
        StatuslineFormat::Starship | StatuslineFormat::Custom => Duration::from_millis(300),
        _ => Duration::from_secs(3),
    };
    let client = daemon_client_with_timeout(timeout);
    let statusline_url = format!("{}/analytics/statusline", base);
    let statusline_data: Value = client
        .get(&statusline_url)
        .query(&query_params)
        .send()
        .ok()
        .filter(|r| r.status().is_success())
        .and_then(|r| r.json::<Value>().ok())
        .unwrap_or_else(|| json!({}));

    let values = build_slot_values(&statusline_data);

    match format {
        StatuslineFormat::Json => {
            println!(
                "{}",
                serde_json::to_string(&statusline_data).unwrap_or_default()
            );
        }
        StatuslineFormat::Custom => {
            if let Some(ref template) = sl_config.format {
                println!("{}", render_template(template, &values));
            } else {
                // No template — render configured slots with " · " separator
                println!("{}", render_slots(&sl_config.slots, &values, " · "));
            }
        }
        StatuslineFormat::Starship => {
            // Use config slots if configured (non-default), otherwise default behavior
            println!("{}", render_slots(&sl_config.slots, &values, " · "));
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

            let parts: Vec<String> = sl_config
                .slots
                .iter()
                .filter_map(|slot| {
                    values
                        .get(slot)
                        .map(|v| format!("{yellow}{v}{reset} {slot}"))
                })
                .collect();

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_cost_formats_correctly() {
        assert_eq!(fmt_cost(0.0), "$0");
        assert_eq!(fmt_cost(0.42), "$0.42");
        assert_eq!(fmt_cost(12.50), "$12.50");
        assert_eq!(fmt_cost(123.0), "$123");
        assert_eq!(fmt_cost(1500.0), "$1.5K");
    }

    #[test]
    fn render_template_replaces_placeholders() {
        let mut values = HashMap::new();
        values.insert("today".to_string(), "$1.23".to_string());
        values.insert("week".to_string(), "$5.00".to_string());
        values.insert("branch".to_string(), "$12.50".to_string());

        assert_eq!(
            render_template("{today} | {week} | {branch}", &values),
            "$1.23 | $5.00 | $12.50"
        );
    }

    #[test]
    fn render_template_leaves_unknown_placeholders() {
        let values = HashMap::new();
        assert_eq!(render_template("{unknown}", &values), "{unknown}");
    }

    #[test]
    fn render_slots_filters_missing() {
        let mut values = HashMap::new();
        values.insert("today".to_string(), "$1.23".to_string());
        values.insert("week".to_string(), "$5.00".to_string());

        let slots = vec![
            "today".to_string(),
            "week".to_string(),
            "branch".to_string(),
        ];
        // "branch" is not in values, so it should be skipped
        assert_eq!(
            render_slots(&slots, &values, " · "),
            "$1.23 today · $5.00 week"
        );
    }

    #[test]
    fn build_slot_values_from_json() {
        let data = json!({
            "today_cost": 1.23,
            "week_cost": 5.0,
            "month_cost": 0.0,
            "branch_cost": 12.5,
            "active_provider": "claude_code"
        });
        let vals = build_slot_values(&data);
        assert_eq!(vals.get("today").unwrap(), "$1.23");
        assert_eq!(vals.get("week").unwrap(), "$5.00");
        assert_eq!(vals.get("month").unwrap(), "$0");
        assert_eq!(vals.get("branch").unwrap(), "$12.50");
        assert_eq!(vals.get("provider").unwrap(), "claude_code");
        assert!(!vals.contains_key("session")); // not in response
    }
}
