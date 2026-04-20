use std::collections::HashMap;
use std::fs;
use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};
use budi_core::config;
use chrono::Utc;
use serde_json::{Value, json};

use crate::StatuslineFormat;
use crate::daemon::daemon_client_with_timeout;

pub const CLAUDE_USER_SETTINGS: &str = ".claude/settings.json";

use super::format_cost as fmt_cost;

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

    // Prefer the new rolling-window fields (`cost_1d` / `cost_7d` /
    // `cost_30d`), falling back to the deprecated calendar aliases so older
    // daemons still render something useful during a mixed-version window.
    let get_cost = |primary: &str, legacy: &str| -> f64 {
        data.get(primary)
            .and_then(|v| v.as_f64())
            .or_else(|| data.get(legacy).and_then(|v| v.as_f64()))
            .unwrap_or(0.0)
    };

    let cost_1d = fmt_cost(get_cost("cost_1d", "today_cost"));
    let cost_7d = fmt_cost(get_cost("cost_7d", "week_cost"));
    let cost_30d = fmt_cost(get_cost("cost_30d", "month_cost"));

    vals.insert("1d".to_string(), cost_1d.clone());
    vals.insert("7d".to_string(), cost_7d.clone());
    vals.insert("30d".to_string(), cost_30d.clone());

    // Legacy aliases — users with older `~/.config/budi/statusline.toml`
    // files written against the 8.0 slot vocabulary keep rendering, since
    // slot names are normalized during config load.
    vals.insert("today".to_string(), cost_1d);
    vals.insert("week".to_string(), cost_7d);
    vals.insert("month".to_string(), cost_30d);

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

    // Health slot: just the session cost (no traffic-light emojis or tips per #127)
    if data.get("health_state").is_some()
        && let Some(v) = data.get("session_cost").and_then(|v| v.as_f64())
    {
        vals.insert("health".to_string(), fmt_cost(v));
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

const FALLBACK_SLOTS: &[&str] = &["1d", "7d", "30d"];

/// Render slots as a separator-joined string.
/// Falls back to basic cost slots if the requested slots produce nothing.
fn render_slots(slots: &[String], values: &HashMap<String, String>, sep: &str) -> String {
    let result: String = slots
        .iter()
        .filter_map(|slot| values.get(slot).map(|v| format!("{v} {slot}")))
        .collect::<Vec<_>>()
        .join(sep);
    if result.is_empty() {
        FALLBACK_SLOTS
            .iter()
            .filter_map(|slot| values.get(*slot).map(|v| format!("{v} {slot}")))
            .collect::<Vec<_>>()
            .join(sep)
    } else {
        result
    }
}

/// Session-aware rendering for coach/full presets.
///
/// Session view: `📊 budi · $0.03 msg · $1.24 session · {extra}`
/// Falls back to period view when no session data is available.
///
/// `budi_label` is pre-formatted (may include ANSI/OSC 8 for Claude format).
fn render_coach(
    data: &Value,
    extra_slots: &[(&str, &HashMap<String, String>)],
    ansi: bool,
    budi_label: &str,
) -> Option<String> {
    let _state = data.get("health_state")?.as_str()?;

    let (dim, reset) = if ansi {
        ("\x1b[90m", "\x1b[0m")
    } else {
        ("", "")
    };

    let session_cost = data.get("session_cost").and_then(|v| v.as_f64())?;

    let mut parts: Vec<String> = vec![format!("📊 {budi_label}")];

    // Last message cost (if available)
    if let Some(msg_cost) = data.get("last_message_cost").and_then(|v| v.as_f64()) {
        parts.push(format!("{} msg", fmt_cost(msg_cost)));
    }

    parts.push(format!("{} session", fmt_cost(session_cost)));

    for (slot_name, values) in extra_slots {
        if let Some(v) = values.get(*slot_name) {
            parts.push(format!("{v} {slot_name}"));
        }
    }

    let sep = format!(" {dim}·{reset} ");
    Some(parts.join(&sep))
}

/// Legacy custom-template tokens whose values silently shifted from calendar
/// to rolling semantics in 8.2 (ADR-0088 §4). Users with a custom
/// `statusline.toml` referencing these keep rendering, but the underlying
/// number moved, so we nudge them once per day to switch.
const LEGACY_STATUSLINE_TOKENS: &[&str] = &["{today}", "{week}", "{month}"];

/// Relative name (under `BUDI_HOME`) of the marker file that remembers the
/// last UTC date on which we emitted the legacy-token nudge. One marker
/// covers all legacy tokens — the nudge text already names all three.
const LEGACY_STATUSLINE_NUDGE_MARKER: &str = "statusline-legacy-nudge";

/// Returns the sorted set of legacy tokens present in `template`.
fn detect_legacy_statusline_tokens(template: &str) -> Vec<&'static str> {
    LEGACY_STATUSLINE_TOKENS
        .iter()
        .copied()
        .filter(|tok| template.contains(tok))
        .collect()
}

fn legacy_nudge_marker_path() -> Option<PathBuf> {
    config::budi_home_dir()
        .ok()
        .map(|d| d.join(LEGACY_STATUSLINE_NUDGE_MARKER))
}

/// If `template` uses legacy tokens and we haven't already nudged today,
/// print a one-line deprecation note to stderr and persist today's UTC date
/// in the marker file so subsequent renders on the same day stay quiet.
///
/// All filesystem errors are swallowed: a prompt-hot path must never fail a
/// render because a marker couldn't be written.
fn nudge_legacy_statusline_tokens(template: &str) {
    nudge_legacy_statusline_tokens_inner(template, legacy_nudge_marker_path, &mut io::stderr());
}

fn nudge_legacy_statusline_tokens_inner(
    template: &str,
    marker_path: impl FnOnce() -> Option<PathBuf>,
    sink: &mut dyn io::Write,
) {
    let found = detect_legacy_statusline_tokens(template);
    if found.is_empty() {
        return;
    }

    let today = Utc::now().format("%Y-%m-%d").to_string();
    let marker = marker_path();

    if let Some(ref path) = marker
        && let Ok(existing) = fs::read_to_string(path)
        && existing.trim() == today
    {
        return;
    }

    // Nudge first, persist second — if the write fails we still want the
    // user to see the note.
    let _ = writeln!(
        sink,
        "budi: `{{today}}` / `{{week}}` / `{{month}}` in ~/.config/budi/statusline.toml \
         now render the rolling `1d` / `7d` / `30d` values from the statusline contract. \
         Switch to `{{1d}}` / `{{7d}}` / `{{30d}}` to silence this notice."
    );

    if let Some(path) = marker {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(&path, format!("{today}\n"));
    }
}

pub fn cmd_statusline(format: StatuslineFormat, provider: Option<String>) -> Result<()> {
    let stdin_json = if io::stdin().is_terminal() {
        None
    } else {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut input = String::new();
            let _ = io::stdin().read_to_string(&mut input);
            let _ = tx.send(input);
        });
        match rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(input) if !input.trim().is_empty() => serde_json::from_str::<Value>(&input).ok(),
            _ => None,
        }
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

    let cfg = crate::client::DaemonClient::load_config();
    let base = cfg.daemon_base_url();

    // Load statusline config (for Claude/Custom formats, and to determine needed query params)
    let sl_config = config::load_statusline_config();
    let needed = sl_config.required_slots();

    // Detect git branch if needed
    let branch = if needed.contains(&"branch".to_string()) {
        cwd.as_deref().and_then(detect_git_branch)
    } else {
        None
    };

    // Provider scoping: the Claude Code statusline shows Claude Code usage
    // only by default (ADR-0088 §4, #224). Other formats are unscoped unless
    // an explicit `--provider` is passed so downstream consumers can reuse
    // the shared status contract with their own provider filter.
    let effective_provider = provider.or_else(|| match format {
        StatuslineFormat::Claude => Some("claude_code".to_string()),
        _ => None,
    });

    // Build query params for the daemon
    let mut query_params: Vec<(&str, String)> = Vec::new();
    if let Some(ref p) = effective_provider {
        query_params.push(("provider", p.clone()));
    }
    let needs_session =
        needed.contains(&"session".to_string()) || needed.contains(&"health".to_string());
    if let Some(ref sid) = session_id
        && needs_session
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
    let Ok(client) = daemon_client_with_timeout(timeout) else {
        if format == StatuslineFormat::Claude {
            println!("\x1b[36mbudi\x1b[0m \x1b[90m--\x1b[0m");
        }
        return Ok(());
    };
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
    let has_health = statusline_data.get("health_state").is_some();

    // Extra slots for coach rendering (slots beyond session+health, e.g. "today" in "full" preset)
    let effective = sl_config.effective_slots();
    let extra: Vec<(&str, &HashMap<String, String>)> = effective
        .iter()
        .filter(|s| *s != "session" && *s != "health")
        .map(|s| (s.as_str(), &values))
        .collect();

    let cloud_base = budi_core::config::DEFAULT_CLOUD_ENDPOINT;
    let budi_url = if session_id.is_some() {
        format!("{cloud_base}/dashboard/sessions")
    } else {
        format!("{cloud_base}/dashboard")
    };

    match format {
        StatuslineFormat::Json => {
            println!(
                "{}",
                serde_json::to_string(&statusline_data).unwrap_or_default()
            );
        }
        StatuslineFormat::Custom => {
            if let Some(ref template) = sl_config.format {
                nudge_legacy_statusline_tokens(template);
                println!("{}", render_template(template, &values));
            } else if has_health {
                let line = render_coach(&statusline_data, &extra, false, "budi")
                    .unwrap_or_else(|| render_slots(&effective, &values, " · "));
                println!("{line}");
            } else {
                println!("{}", render_slots(&effective, &values, " · "));
            }
        }
        StatuslineFormat::Starship => {
            let line = if has_health {
                render_coach(&statusline_data, &extra, false, "budi")
                    .unwrap_or_else(|| render_slots(&effective, &values, " · "))
            } else {
                render_slots(&effective, &values, " · ")
            };
            println!("{line}");
        }
        StatuslineFormat::Claude => {
            let budi_link = format!(
                "\x1b[36m\x1b]8;;{}\x1b\\budi\x1b]8;;\x1b\\\x1b[0m",
                budi_url,
            );
            let dim = "\x1b[90m";
            let reset = "\x1b[0m";
            let yellow = "\x1b[33m";

            let render_cost_line = |slots: &[String]| -> String {
                let mut parts: Vec<String> = slots
                    .iter()
                    .filter_map(|slot| {
                        values
                            .get(slot)
                            .map(|v| format!("{yellow}{v}{reset} {slot}"))
                    })
                    .collect();
                if parts.is_empty() {
                    parts = FALLBACK_SLOTS
                        .iter()
                        .filter_map(|slot| {
                            values
                                .get(*slot)
                                .map(|v| format!("{yellow}{v}{reset} {slot}"))
                        })
                        .collect();
                }
                let joined = parts.join(&format!(" {dim}·{reset} "));
                format!("{budi_link} {dim}·{reset} {joined}")
            };

            if has_health {
                let line = render_coach(&statusline_data, &extra, true, &budi_link)
                    .unwrap_or_else(|| render_cost_line(&effective));
                println!("{line}");
            } else {
                println!("{}", render_cost_line(&effective));
            }
        }
    }

    Ok(())
}

/// The budi statusline command string used in settings.
pub(crate) const BUDI_STATUSLINE_CMD: &str = "budi statusline";

/// Suffix appended to an existing command to merge budi output after it.
pub(crate) fn budi_statusline_suffix() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        ""
    }
    #[cfg(not(target_os = "windows"))]
    {
        r#"; budi_out=$(budi statusline 2>/dev/null || true); [ -n "$budi_out" ] && printf " %s" "$budi_out""#
    }
}

/// Check if a statusLine command already includes budi.
pub(crate) fn statusline_has_budi(cmd: &str) -> bool {
    cmd.contains("budi statusline") || cmd.contains("budi_out=$(budi")
}

pub fn cmd_statusline_install() -> Result<()> {
    let home = budi_core::config::home_dir()?;
    let settings_path = home.join(CLAUDE_USER_SETTINGS);
    if let Some(parent) = settings_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed creating {}", parent.display()))?;
    }
    let mut settings = super::read_json_object_strict(&settings_path)?;

    match super::integrations::apply_statusline(&mut settings)? {
        super::integrations::StatuslineApply::AlreadyConfigured => {
            println!(
                "Status line already includes budi in {}",
                settings_path.display()
            );
            Ok(())
        }
        super::integrations::StatuslineApply::Changed => {
            let raw = serde_json::to_string_pretty(&settings)?;
            fs::write(&settings_path, raw)
                .with_context(|| format!("Failed writing {}", settings_path.display()))?;
            println!("Installed budi status line in {}", settings_path.display());
            Ok(())
        }
        super::integrations::StatuslineApply::ManualMergeRequired => {
            anyhow::bail!(
                "statusLine already exists in {} and merge is shell-dependent on Windows. \
                 Please set `statusLine.command` manually to include `budi statusline`.",
                settings_path.display()
            )
        }
    }
}

/// Remove legacy budi hooks from ~/.claude/settings.json.
/// Old budi versions installed hooks that call `budi hook <subcommand>` (with arguments).
/// Preserves new-style hooks that use just `budi hook` (no arguments).
pub fn remove_legacy_hooks() {
    let Ok(home) = budi_core::config::home_dir() else {
        return;
    };
    let settings_path = home.join(CLAUDE_USER_SETTINGS);
    if !settings_path.exists() {
        return;
    }
    let Ok(raw) = fs::read_to_string(&settings_path) else {
        return;
    };
    let Ok(mut settings) = serde_json::from_str::<Value>(&raw) else {
        return;
    };

    if !remove_legacy_budi_hooks_from_value(&mut settings) {
        return;
    }

    if let Ok(out) = serde_json::to_string_pretty(&settings)
        && fs::write(&settings_path, &out).is_ok()
    {
        eprintln!(
            "  Cleaned up legacy budi hooks from {}",
            settings_path.display()
        );
    }
}

/// Remove old-style budi hooks (with subcommand args) from a settings Value.
/// Returns true if any changes were made.
pub(crate) fn remove_legacy_budi_hooks_from_value(settings: &mut Value) -> bool {
    let Some(hooks) = settings.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
        return false;
    };

    let mut changed = false;
    let event_keys: Vec<String> = hooks.keys().cloned().collect();

    for key in &event_keys {
        if let Some(arr) = hooks.get_mut(key).and_then(|v| v.as_array_mut()) {
            let before = arr.len();
            arr.retain(|entry| {
                let cmd = entry.get("command").and_then(|c| c.as_str()).unwrap_or("");
                // Remove old-style: "budi hook user-prompt-submit", "budi hook stop", etc.
                // Keep new-style: "budi hook" (exactly, no trailing args)
                !is_legacy_budi_hook(cmd)
            });
            if arr.len() != before {
                changed = true;
            }
        }
    }

    if !changed {
        return false;
    }

    // Remove empty event arrays
    let empty_keys: Vec<String> = hooks
        .iter()
        .filter(|(_, v)| v.as_array().is_some_and(|a| a.is_empty()))
        .map(|(k, _)| k.clone())
        .collect();
    for key in &empty_keys {
        hooks.remove(key);
    }

    if hooks.is_empty()
        && let Some(obj) = settings.as_object_mut()
    {
        obj.remove("hooks");
    }

    true
}

/// Check if a command string is an old-style budi hook (with subcommand arguments).
fn is_legacy_budi_hook(cmd: &str) -> bool {
    let trimmed = cmd.trim();
    // Old style: "budi hook <something>" — has args after "budi hook"
    if trimmed.starts_with("budi hook ") || trimmed.starts_with("budi hook\t") {
        let after = trimmed.strip_prefix("budi hook").unwrap().trim();
        !after.is_empty()
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_cost_formats_correctly() {
        assert_eq!(fmt_cost(0.0), "$0.00");
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
            "cost_1d": 1.23,
            "cost_7d": 5.0,
            "cost_30d": 0.0,
            "branch_cost": 12.5,
            "active_provider": "claude_code"
        });
        let vals = build_slot_values(&data);
        assert_eq!(vals.get("1d").unwrap(), "$1.23");
        assert_eq!(vals.get("7d").unwrap(), "$5.00");
        assert_eq!(vals.get("30d").unwrap(), "$0.00");
        // Legacy aliases continue to resolve to the same rolling values.
        assert_eq!(vals.get("today").unwrap(), "$1.23");
        assert_eq!(vals.get("week").unwrap(), "$5.00");
        assert_eq!(vals.get("month").unwrap(), "$0.00");
        assert_eq!(vals.get("branch").unwrap(), "$12.50");
        assert_eq!(vals.get("provider").unwrap(), "claude_code");
        assert!(!vals.contains_key("session")); // not in response
    }

    #[test]
    fn build_slot_values_falls_back_to_legacy_field_names() {
        // Simulate an older daemon that only knows today_cost / week_cost /
        // month_cost. The CLI must still render something during a
        // mixed-version window.
        let data = json!({
            "today_cost": 2.5,
            "week_cost": 10.0,
            "month_cost": 40.0,
        });
        let vals = build_slot_values(&data);
        assert_eq!(vals.get("1d").unwrap(), "$2.50");
        assert_eq!(vals.get("7d").unwrap(), "$10.00");
        assert_eq!(vals.get("30d").unwrap(), "$40.00");
    }

    #[test]
    fn remove_legacy_hooks_removes_budi_entries() {
        let mut settings = json!({
            "hooks": {
                "UserPromptSubmit": [
                    {"type": "command", "command": "budi hook user-prompt-submit"}
                ],
                "PostToolUse": [
                    {"type": "command", "command": "budi hook post-tool-use"}
                ]
            },
            "statusLine": {"type": "command", "command": "budi statusline"}
        });
        assert!(remove_legacy_budi_hooks_from_value(&mut settings));
        // hooks object removed entirely since all entries were budi
        assert!(settings.get("hooks").is_none());
        // statusLine untouched
        assert!(settings.get("statusLine").is_some());
    }

    #[test]
    fn remove_legacy_hooks_preserves_non_budi() {
        let mut settings = json!({
            "hooks": {
                "UserPromptSubmit": [
                    {"type": "command", "command": "budi hook user-prompt-submit"},
                    {"type": "command", "command": "other-tool do-something"}
                ]
            }
        });
        assert!(remove_legacy_budi_hooks_from_value(&mut settings));
        let hooks = settings.get("hooks").unwrap();
        let arr = hooks.get("UserPromptSubmit").unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["command"], "other-tool do-something");
    }

    #[test]
    fn remove_legacy_hooks_noop_without_hooks() {
        let mut settings = json!({"statusLine": {"type": "command"}});
        assert!(!remove_legacy_budi_hooks_from_value(&mut settings));
    }

    #[test]
    fn remove_legacy_hooks_preserves_new_style_budi_hook() {
        let mut settings = json!({
            "hooks": {
                "PostToolUse": [
                    {"matcher": "", "hooks": [{"type": "command", "command": "budi hook", "async": true}]}
                ],
                "UserPromptSubmit": [
                    {"type": "command", "command": "budi hook user-prompt-submit"}
                ]
            }
        });
        assert!(remove_legacy_budi_hooks_from_value(&mut settings));
        let hooks = settings.get("hooks").unwrap();
        // PostToolUse with new-style "budi hook" should be preserved
        let arr = hooks.get("PostToolUse").unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 1);
        // UserPromptSubmit with old-style should be removed
        assert!(hooks.get("UserPromptSubmit").is_none());
    }

    #[test]
    fn is_legacy_budi_hook_detection() {
        assert!(is_legacy_budi_hook("budi hook user-prompt-submit"));
        assert!(is_legacy_budi_hook("budi hook post-tool-use"));
        assert!(!is_legacy_budi_hook("budi hook"));
        assert!(!is_legacy_budi_hook("budi statusline"));
        assert!(!is_legacy_budi_hook("other-tool do-something"));
    }

    #[test]
    fn statusline_has_budi_detects_presence() {
        assert!(statusline_has_budi("budi statusline"));
        assert!(statusline_has_budi(
            "some-cmd; budi statusline 2>/dev/null || true"
        ));
        assert!(!statusline_has_budi("echo hello"));
        assert!(!statusline_has_budi("other-tool --flag"));
    }

    #[test]
    fn detect_legacy_statusline_tokens_finds_all() {
        assert_eq!(
            detect_legacy_statusline_tokens("{1d} | {7d}"),
            Vec::<&str>::new()
        );
        assert_eq!(
            detect_legacy_statusline_tokens("{today} | {week} | {month}"),
            vec!["{today}", "{week}", "{month}"]
        );
        assert_eq!(
            detect_legacy_statusline_tokens("spent {today} so far"),
            vec!["{today}"]
        );
        assert_eq!(
            detect_legacy_statusline_tokens("{week} {branch} {1d}"),
            vec!["{week}"]
        );
    }

    #[test]
    fn nudge_legacy_statusline_tokens_silent_without_legacy() {
        let dir =
            std::env::temp_dir().join(format!("budi-nudge-test-silent-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let marker = dir.join("marker");
        let mut out = Vec::<u8>::new();
        nudge_legacy_statusline_tokens_inner("{1d} {7d} {30d}", || Some(marker.clone()), &mut out);
        assert!(out.is_empty(), "no nudge expected for canonical tokens");
        assert!(!marker.exists(), "no marker should be written");
    }

    #[test]
    fn nudge_legacy_statusline_tokens_writes_once_per_day() {
        let dir = std::env::temp_dir().join(format!("budi-nudge-test-once-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let marker = dir.join("statusline-legacy-nudge");

        let marker_fn = || Some(marker.clone());

        let mut first = Vec::<u8>::new();
        nudge_legacy_statusline_tokens_inner("{today} | {week}", marker_fn, &mut first);
        let first_text = String::from_utf8(first).unwrap();
        assert!(
            first_text.contains("now render the rolling"),
            "first render should nudge, got {first_text:?}"
        );
        assert!(marker.exists(), "marker should be written after nudging");
        let stored = fs::read_to_string(&marker).unwrap();
        assert_eq!(stored.trim(), Utc::now().format("%Y-%m-%d").to_string());

        let mut second = Vec::<u8>::new();
        nudge_legacy_statusline_tokens_inner("{today} | {week}", marker_fn, &mut second);
        assert!(
            second.is_empty(),
            "second render on the same day should stay quiet"
        );

        // Simulate "yesterday" — nudge should fire again and overwrite.
        fs::write(&marker, "1970-01-01\n").unwrap();
        let mut third = Vec::<u8>::new();
        nudge_legacy_statusline_tokens_inner("{month}", marker_fn, &mut third);
        assert!(
            !third.is_empty(),
            "stale marker should allow the nudge to fire again"
        );
        let refreshed = fs::read_to_string(&marker).unwrap();
        assert_eq!(refreshed.trim(), Utc::now().format("%Y-%m-%d").to_string());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn nudge_legacy_statusline_tokens_survives_missing_marker_dir() {
        // Marker path whose parent does not yet exist — nudge must still
        // emit, and the marker gets written after directory creation.
        let dir =
            std::env::temp_dir().join(format!("budi-nudge-test-mkdir-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let marker = dir.join("nested").join("statusline-legacy-nudge");
        let mut out = Vec::<u8>::new();
        nudge_legacy_statusline_tokens_inner("{today}", || Some(marker.clone()), &mut out);
        assert!(!out.is_empty());
        assert!(marker.exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
