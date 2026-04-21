//! `budi pricing` subcommand — manifest status + manual refresh.
//!
//! Surfaces what [`budi_core::pricing::current_state`] knows about the
//! in-memory pricing manifest: which layer is authoritative (disk cache
//! vs. embedded baseline), the version embedded in newly-priced rows,
//! when the cache was last refreshed, the count of known models, and any
//! model ids seen in transcripts that aren't in the manifest yet (those
//! get `cost_cents = 0` until a refresh resolves them, ADR-0091 §2).
//!
//! The `--refresh` flag triggers an immediate manifest fetch via
//! `POST /pricing/refresh` and then prints the post-refresh status. This
//! is the only user-facing way to skip the 24 h worker cadence.
//!
//! Output format matches the `budi cloud status` contract: `--format
//! text` is the default, `--format json` emits the daemon payload
//! verbatim for scripting. Exit code is `2` on refresh failure so CI
//! scripts can branch on status without parsing the body.

use anyhow::Result;
use serde_json::Value;

use crate::StatsFormat;
use crate::client::DaemonClient;

use super::{ansi, format_cost};

/// `budi pricing status [--json] [--refresh]`
pub fn cmd_pricing_status(format: StatsFormat, refresh: bool) -> Result<()> {
    let client = DaemonClient::connect()?;

    let refresh_body = if refresh {
        Some(client.pricing_refresh()?)
    } else {
        None
    };

    let status = client.pricing_status()?;

    if matches!(format, StatsFormat::Json) {
        let combined = if let Some(r) = &refresh_body {
            serde_json::json!({ "refresh": r, "status": status })
        } else {
            status.clone()
        };
        println!("{}", serde_json::to_string_pretty(&combined)?);
        if let Some(r) = refresh_body.as_ref()
            && r.get("ok").and_then(Value::as_bool) != Some(true)
        {
            std::process::exit(2);
        }
        return Ok(());
    }

    if let Some(r) = &refresh_body {
        render_refresh_text(r);
        if r.get("ok").and_then(Value::as_bool) != Some(true) {
            render_status_text(&status);
            std::process::exit(2);
        }
    }
    render_status_text(&status);
    Ok(())
}

fn render_refresh_text(body: &Value) {
    let green = ansi("\x1b[32m");
    let red = ansi("\x1b[31m");
    let dim = ansi("\x1b[90m");
    let reset = ansi("\x1b[0m");
    let ok = body.get("ok").and_then(Value::as_bool).unwrap_or(false);
    if ok {
        let version = body.get("version").and_then(Value::as_u64).unwrap_or(0);
        let known = body
            .get("known_model_count")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let backfilled = body
            .get("backfilled_rows")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        println!();
        println!(
            "  {green}✓{reset} Manifest refreshed — now v{version} ({known} models, {backfilled} rows backfilled)"
        );
    } else {
        let err = body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("refresh failed");
        println!();
        println!("  {red}✗{reset} Refresh failed: {err}");
        println!("    {dim}previous cache stays authoritative{reset}");
    }
}

fn render_status_text(body: &Value) {
    let green = ansi("\x1b[32m");
    let yellow = ansi("\x1b[33m");
    let dim = ansi("\x1b[90m");
    let bold = ansi("\x1b[1m");
    let bold_cyan = ansi("\x1b[1;36m");
    let reset = ansi("\x1b[0m");

    let source_label = body
        .get("source_label")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let version = body.get("manifest_version").and_then(Value::as_u64);
    let fetched_at = body.get("fetched_at").and_then(Value::as_str);
    let next_refresh = body.get("next_refresh_at").and_then(Value::as_str);
    let known = body
        .get("known_model_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let baseline_build = body
        .get("embedded_baseline_build")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let unknowns = body
        .get("unknown_models")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    println!();
    println!("  {bold_cyan} Pricing manifest{reset}");
    println!("  {dim}{}{reset}", "─".repeat(40));
    println!("  {bold}Source{reset}           {green}{source_label}{reset}");
    match version {
        Some(v) => println!("  {bold}Manifest version{reset} v{v}"),
        None => println!("  {bold}Manifest version{reset} {dim}(embedded){reset}"),
    }
    if let Some(ts) = fetched_at {
        println!("  {bold}Fetched at{reset}       {ts}");
    }
    if let Some(ts) = next_refresh {
        println!("  {bold}Next refresh{reset}     {ts}");
    }
    println!("  {bold}Known models{reset}     {known}");
    println!("  {bold}Embedded baseline{reset} v{baseline_build} {dim}(release snapshot){reset}");

    if !unknowns.is_empty() {
        println!();
        println!(
            "  {yellow}!{reset} {bold}Unknown models seen{reset} {dim}(priced at $0.00; auto-backfill on next refresh){reset}"
        );
        for entry in unknowns.iter().take(10) {
            let model = entry.get("model_id").and_then(Value::as_str).unwrap_or("?");
            let provider = entry.get("provider").and_then(Value::as_str).unwrap_or("?");
            let count = entry
                .get("message_count")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let cost = format_cost(0.0);
            println!("    {dim}•{reset} {model} ({provider}) — {count} messages, {cost}");
        }
        if unknowns.len() > 10 {
            println!(
                "    {dim}• … {} more (run with --format json for the full list){reset}",
                unknowns.len() - 10
            );
        }
    }
    println!();
}
