use std::fs;
use std::io;
use std::path::PathBuf;

use anyhow::Result;
use budi_core::analytics::SessionHealth;
use budi_core::config;
use chrono::Utc;

use crate::client::DaemonClient;

pub fn cmd_vitals(session_id: Option<String>) -> Result<()> {
    let client = DaemonClient::connect()?;
    let health = client.session_health(session_id.as_deref())?;
    render_vitals(&health);
    Ok(())
}

/// Relative name (under `BUDI_HOME`) of the marker file that remembers the
/// last UTC date on which we emitted the `budi health` → `budi vitals`
/// deprecation nudge. One marker per day keeps the hint visible without
/// spamming every invocation (mirrors the statusline legacy-token nudge in
/// #345).
const HEALTH_ALIAS_NUDGE_MARKER: &str = "health-alias-nudge";

fn health_alias_marker_path() -> Option<PathBuf> {
    config::budi_home_dir()
        .ok()
        .map(|d| d.join(HEALTH_ALIAS_NUDGE_MARKER))
}

/// Emit a one-per-day stderr nudge telling the caller that `budi health` has
/// been renamed to `budi vitals`. Called from the deprecated alias in
/// `main.rs` before we run the real command so the user always sees the hint
/// at least once a day.
///
/// Filesystem errors are swallowed so a CLI invocation never fails just
/// because the marker file couldn't be written.
pub fn nudge_health_alias() {
    nudge_health_alias_inner(health_alias_marker_path, &mut io::stderr());
}

fn nudge_health_alias_inner(
    marker_path: impl FnOnce() -> Option<PathBuf>,
    sink: &mut dyn io::Write,
) {
    let today = Utc::now().format("%Y-%m-%d").to_string();
    let marker = marker_path();

    if let Some(ref path) = marker
        && let Ok(existing) = fs::read_to_string(path)
        && existing.trim() == today
    {
        return;
    }

    let _ = writeln!(
        sink,
        "budi: `budi health` has been renamed to `budi vitals` and will be \
         removed in 8.3. Switch to `budi vitals` to silence this notice."
    );

    if let Some(path) = marker {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(&path, format!("{today}\n"));
    }
}

fn render_vitals(h: &SessionHealth) {
    let detail_name = |name: &str| -> String {
        match name {
            "context_drag" => "Prompt Growth".to_string(),
            "cache_efficiency" => "Cache Reuse".to_string(),
            "thrashing" => "Retry Loops".to_string(),
            "cost_acceleration" => "Cost Acceleration".to_string(),
            _ => name.to_string(),
        }
    };

    let bold = super::ansi("\x1b[1m");
    let dim = super::ansi("\x1b[90m");
    let reset = super::ansi("\x1b[0m");
    let green = super::ansi("\x1b[32m");
    let yellow = super::ansi("\x1b[33m");
    let red = super::ansi("\x1b[31m");

    let state_icon = |s: &str| -> &str {
        match s {
            "red" => "🔴",
            "yellow" => "🟡",
            "gray" => "⚪",
            _ => "🟢",
        }
    };
    let state_color = |s: &str| -> &str {
        match s {
            "red" => red,
            "yellow" => yellow,
            "gray" => dim,
            _ => green,
        }
    };

    let icon = state_icon(&h.state);
    let color = state_color(&h.state);
    println!(
        "{icon} {bold}Session Health: {color}{}{reset}",
        h.state.to_uppercase()
    );
    let cost_dollars = h.total_cost_cents / 100.0;
    let cost_display = if cost_dollars == 0.0 {
        0.0
    } else {
        cost_dollars
    };
    println!(
        "  {dim}{} messages · ${:.2} total{reset}",
        h.message_count, cost_display
    );
    if let Some(ref hint) = h.cost_lag_hint {
        println!("  {dim}* {hint}{reset}");
    }
    println!();

    let vitals: Vec<(&str, &Option<budi_core::analytics::VitalScore>)> = vec![
        ("Prompt Growth", &h.vitals.context_drag),
        ("Cache Reuse", &h.vitals.cache_efficiency),
        ("Retry Loops", &h.vitals.thrashing),
        ("Cost Acceleration", &h.vitals.cost_acceleration),
    ];

    for (name, vital) in &vitals {
        match vital {
            Some(v) => {
                let vi = state_icon(&v.state);
                let vc = state_color(&v.state);
                println!("  {vi} {bold}{name}{reset}: {vc}{}{reset}", v.label);
            }
            None => {
                println!("  {dim}⚪ {name}: N/A{reset}");
            }
        }
    }

    if !h.details.is_empty() {
        println!();
        for d in &h.details {
            let di = state_icon(&d.state);
            let dc = state_color(&d.state);
            println!(
                "{di} {dc}{bold}{} ({}):{reset}",
                detail_name(&d.vital),
                d.label
            );
            println!("  {}", d.tip);
            for action in &d.actions {
                println!("  - {action}");
            }
            println!();
        }
    }

    if h.state == "green" {
        println!();
        println!("  {green}{}{reset}", h.tip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nudge_health_alias_writes_once_per_day() {
        let dir = std::env::temp_dir().join(format!(
            "budi-vitals-nudge-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let _ = fs::remove_dir_all(&dir);
        let marker = dir.join(HEALTH_ALIAS_NUDGE_MARKER);

        let marker_fn = || Some(marker.clone());

        let mut first = Vec::<u8>::new();
        nudge_health_alias_inner(marker_fn, &mut first);
        let first_text = String::from_utf8(first).unwrap();
        assert!(
            first_text.contains("renamed to `budi vitals`"),
            "first invocation should nudge, got {first_text:?}"
        );
        assert!(marker.exists(), "marker should be written after nudging");
        let stored = fs::read_to_string(&marker).unwrap();
        assert_eq!(stored.trim(), Utc::now().format("%Y-%m-%d").to_string());

        let mut second = Vec::<u8>::new();
        nudge_health_alias_inner(marker_fn, &mut second);
        assert!(
            second.is_empty(),
            "second invocation on the same day should stay quiet"
        );

        // Simulate a stale marker (previous day) — nudge should fire again
        // and refresh the date in-place.
        fs::write(&marker, "1970-01-01\n").unwrap();
        let mut third = Vec::<u8>::new();
        nudge_health_alias_inner(marker_fn, &mut third);
        assert!(
            !third.is_empty(),
            "stale marker should allow the nudge to fire again"
        );
        let refreshed = fs::read_to_string(&marker).unwrap();
        assert_eq!(refreshed.trim(), Utc::now().format("%Y-%m-%d").to_string());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn nudge_health_alias_survives_missing_marker_dir() {
        let dir = std::env::temp_dir().join(format!(
            "budi-vitals-nudge-mkdir-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let _ = fs::remove_dir_all(&dir);
        let marker = dir.join("nested").join(HEALTH_ALIAS_NUDGE_MARKER);
        let mut out = Vec::<u8>::new();
        nudge_health_alias_inner(|| Some(marker.clone()), &mut out);
        assert!(!out.is_empty());
        assert!(marker.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn nudge_health_alias_without_budi_home_still_nudges() {
        // If the marker path resolver returns None (e.g. BUDI_HOME unresolvable),
        // we still emit the hint every invocation. Better to nudge loudly
        // than to silently swallow the deprecation signal.
        let mut out = Vec::<u8>::new();
        nudge_health_alias_inner(|| None, &mut out);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("renamed to `budi vitals`"));
    }
}
