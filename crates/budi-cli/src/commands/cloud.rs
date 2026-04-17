//! `budi cloud` subcommands — manual cloud sync and freshness reporting.
//!
//! 8.1 R2.1 (issue #225) introduces `budi cloud sync` as the user-facing way
//! to say "push my latest local data to cloud now" without waiting for the
//! background worker interval (ADR-0083 §9). `budi cloud status` shows the
//! same readiness snapshot the daemon exposes at `GET /cloud/status` so users
//! can answer "is cloud sync healthy?" without reading logs.
//!
//! Both commands follow the normalized CLI output contract shared with
//! `budi stats` / `budi sessions`:
//! - `--format text` is the default, human-readable with ✓/✗ status lines.
//! - `--format json` emits the daemon response body verbatim for scripting.

use anyhow::Result;
use serde_json::Value;

use crate::StatsFormat;
use crate::client::DaemonClient;

use super::ansi;

/// `budi cloud sync` — flush the pending cloud queue now.
pub fn cmd_cloud_sync(format: StatsFormat) -> Result<()> {
    let client = DaemonClient::connect()?;
    let body = client.cloud_sync()?;

    if matches!(format, StatsFormat::Json) {
        println!("{}", serde_json::to_string_pretty(&body)?);
        // Exit non-zero on non-ok result so scripts can branch on status.
        if body.get("ok").and_then(Value::as_bool) != Some(true) {
            std::process::exit(2);
        }
        return Ok(());
    }

    render_sync_text(&body);
    if body.get("ok").and_then(Value::as_bool) != Some(true) {
        std::process::exit(2);
    }
    Ok(())
}

/// `budi cloud status` — report cloud sync readiness and last-synced-at.
pub fn cmd_cloud_status(format: StatsFormat) -> Result<()> {
    let client = DaemonClient::connect()?;
    let body = client.cloud_status()?;

    if matches!(format, StatsFormat::Json) {
        println!("{}", serde_json::to_string_pretty(&body)?);
        return Ok(());
    }

    render_status_text(&body);
    Ok(())
}

fn render_sync_text(body: &Value) {
    let green = ansi("\x1b[32m");
    let red = ansi("\x1b[31m");
    let yellow = ansi("\x1b[33m");
    let dim = ansi("\x1b[90m");
    let bold = ansi("\x1b[1m");
    let reset = ansi("\x1b[0m");

    let ok = body.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let result = body
        .get("result")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let message = body.get("message").and_then(Value::as_str).unwrap_or("");
    let endpoint = body.get("endpoint").and_then(Value::as_str).unwrap_or("");
    let records = body
        .get("records_upserted")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let rollups = body
        .get("rollups_attempted")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let sessions = body
        .get("sessions_attempted")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let watermark = body.get("watermark").and_then(Value::as_str);

    println!();
    let (icon, color, headline) = match (ok, result) {
        (true, "success") => (
            "✓",
            green,
            format!("Cloud sync complete ({records} records pushed)"),
        ),
        (true, "empty_payload") => ("✓", green, "Nothing new to sync".to_string()),
        (_, "disabled") => ("-", dim, "Cloud sync is disabled".to_string()),
        (_, "not_configured") => ("!", yellow, "Cloud sync is not configured".to_string()),
        (_, "auth_failure") => ("✗", red, "Cloud sync failed: authentication".to_string()),
        (_, "schema_mismatch") => ("✗", red, "Cloud sync failed: schema mismatch".to_string()),
        (_, "transient_error") => ("✗", red, "Cloud sync failed: transient error".to_string()),
        _ => ("✗", red, format!("Cloud sync result: {result}")),
    };

    println!("  {color}{icon}{reset} {bold}{headline}{reset}");
    if !message.is_empty() {
        println!("    {dim}{message}{reset}");
    }
    if !endpoint.is_empty() {
        println!("    {dim}endpoint{reset}   {endpoint}");
    }
    if rollups > 0 || sessions > 0 {
        println!("    {dim}attempted{reset}  {rollups} rollups, {sessions} sessions");
    }
    if let Some(wm) = watermark {
        println!("    {dim}watermark{reset}  {wm}");
    }
    println!();
}

fn render_status_text(body: &Value) {
    let green = ansi("\x1b[32m");
    let red = ansi("\x1b[31m");
    let yellow = ansi("\x1b[33m");
    let dim = ansi("\x1b[90m");
    let bold_cyan = ansi("\x1b[1;36m");
    let bold = ansi("\x1b[1m");
    let reset = ansi("\x1b[0m");

    let enabled = body
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let configured = body
        .get("configured")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let ready = body.get("ready").and_then(Value::as_bool).unwrap_or(false);
    let endpoint = body.get("endpoint").and_then(Value::as_str).unwrap_or("");
    let last_synced_at = body.get("last_synced_at").and_then(Value::as_str);
    let watermark = body.get("rollup_watermark").and_then(Value::as_str);
    let pending_rollups = body
        .get("pending_rollups")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let pending_sessions = body
        .get("pending_sessions")
        .and_then(Value::as_i64)
        .unwrap_or(0);

    println!();
    println!("  {bold_cyan} budi cloud status{reset}");
    println!("  {dim}{}{reset}", "─".repeat(40));

    let (state_icon, state_color, state_label) = if ready {
        ("✓", green, "ready")
    } else if enabled && !configured {
        ("!", yellow, "enabled but missing api_key")
    } else if enabled {
        ("!", yellow, "enabled but not fully configured")
    } else {
        ("-", dim, "disabled")
    };
    println!(
        "  {state_color}{state_icon}{reset} {bold}State{reset}      {state_color}{state_label}{reset}"
    );

    if !endpoint.is_empty() {
        println!("    {dim}endpoint{reset}   {endpoint}");
    }

    match last_synced_at {
        Some(ts) => println!("    {dim}last sync{reset}  {ts}"),
        None => println!("    {dim}last sync{reset}  {red}never{reset}"),
    }
    if let Some(wm) = watermark {
        println!("    {dim}watermark{reset}  {wm}");
    }
    if pending_rollups > 0 || pending_sessions > 0 {
        println!(
            "    {dim}pending{reset}    {yellow}{pending_rollups} rollups, {pending_sessions} sessions{reset}  (run `budi cloud sync`)"
        );
    }

    if !enabled {
        println!();
        println!(
            "  {dim}Cloud sync is off. Enable it by setting `enabled = true` in{reset} ~/.config/budi/cloud.toml"
        );
    } else if !configured {
        println!();
        println!(
            "  {yellow}Cloud sync is enabled but missing credentials.{reset} See ~/.config/budi/cloud.toml"
        );
    }
    println!();
}
