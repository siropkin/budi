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
use budi_core::pricing::display;
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
        // #443 acceptance: JSON consumers see the Budi display-name
        // alias overlay alongside the LiteLLM pricing status. The
        // overlay answers "how does a raw provider model id map to
        // the canonical `budi stats --models` display name?" without
        // having to dump the whole 3k-entry LiteLLM manifest.
        let aliases = json_alias_catalogue();
        let combined = if let Some(r) = &refresh_body {
            serde_json::json!({ "refresh": r, "status": status, "aliases": aliases })
        } else {
            serde_json::json!({ "status": status, "aliases": aliases })
        };
        super::print_json(&combined)?;
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
    render_alias_map_text();
    Ok(())
}

/// Build the JSON shape for the #443 alias catalogue exposed by
/// `budi pricing status --format json`. Each entry is
/// `{raw_model, display_name, effort_modifier}` where
/// `effort_modifier` is `null` for rows without one.
fn json_alias_catalogue() -> Vec<serde_json::Value> {
    display::known_aliases()
        .iter()
        .map(|(raw, display_name, effort)| {
            serde_json::json!({
                "raw_model": raw,
                "display_name": display_name,
                "effort_modifier": effort,
            })
        })
        .collect()
}

/// Render the #443 display-name alias overlay so operators can answer
/// "what does `claude-4.5-opus-high-thinking` actually resolve to?"
/// without reading code. Kept compact — curated Budi-owned entries
/// rather than every LiteLLM manifest id.
fn render_alias_map_text() {
    let bold = ansi("\x1b[1m");
    let bold_cyan = ansi("\x1b[1;36m");
    let dim = ansi("\x1b[90m");
    let reset = ansi("\x1b[0m");

    let entries = display::known_aliases();
    if entries.is_empty() {
        return;
    }

    // Label column width is the longest raw alias + 2 trailing
    // spaces, capped so a freakishly long upstream id never pushes
    // the display column off the right edge.
    let label_width = entries
        .iter()
        .map(|(raw, _, _)| raw.chars().count())
        .max()
        .unwrap_or(0)
        .min(40);

    println!("  {bold_cyan} Display-name aliases{reset}");
    println!("  {dim}{}{reset}", "─".repeat(40));
    println!(
        "  {bold}{:<w$}{reset}  {bold}DISPLAY NAME{reset}",
        "RAW MODEL",
        w = label_width
    );
    for (raw, display_name, effort) in entries {
        let shown = match effort {
            Some(e) => format!("{display_name} · {e}"),
            None => (*display_name).to_string(),
        };
        println!("  {:<w$}  {shown}", raw, w = label_width);
    }
    println!();
}

fn render_refresh_text(body: &Value) {
    let green = ansi("\x1b[32m");
    let red = ansi("\x1b[31m");
    let yellow = ansi("\x1b[33m");
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
        // ADR-0091 §2 amendment (8.3.1 / #483): surface row-level
        // rejections so the operator sees why the kept-model count
        // might be one or two short of the raw upstream payload.
        if let Some(rejected) = body.get("rejected_upstream_rows").and_then(Value::as_array)
            && !rejected.is_empty()
        {
            println!(
                "  {yellow}!{reset} {n} upstream row{s} skipped (see below)",
                n = rejected.len(),
                s = if rejected.len() == 1 { "" } else { "s" },
            );
        }
    } else {
        let err = body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("refresh failed");
        println!();
        // #493 (RC-3): parse the daemon's validation-error shape and
        // render a fresh-user-friendly explanation instead of echoing
        // the raw 502 body text. The three whole-payload validation
        // failures produce distinct structured reasons via
        // `ValidationError::Display`; grepping the `err` string for
        // their prefixes classifies the failure without widening the
        // `/pricing/refresh` wire contract.
        let (headline, detail) = classify_refresh_error(err);
        println!("  {red}✗{reset} {headline}");
        if let Some(d) = detail {
            println!("    {dim}{d}{reset}");
        }
        println!(
            "    {dim}Budi is continuing with its previous pricing source. Run `budi pricing status` to see which.{reset}"
        );
    }
}

/// RC-3 (#493): translate a daemon `/pricing/refresh` error string into
/// a headline + optional detail pair the CLI can render without echoing
/// the raw 502 body. Every match arm below corresponds to a
/// `ValidationError::Display` shape from
/// `budi_core::pricing::ValidationError`; the fallback preserves the
/// daemon's exact text so nothing is silently dropped.
fn classify_refresh_error(err: &str) -> (String, Option<String>) {
    // The common pre-amendment "one bad row blocked every refresh" path
    // was `validation rejected: model X price $Y/M exceeds sanity ceiling
    // $1000/M`. 8.3.1's row-level rejection means this specific whole-
    // payload failure should never fire from a properly-deployed daemon
    // — but if an operator runs a mixed daemon/CLI version (or a future
    // validation shape reuses the phrasing), the friendly wrap still
    // applies.
    if err.contains("exceeds sanity ceiling") {
        return (
            "Pricing manifest refresh rejected by the sanity ceiling".to_string(),
            Some(format!(
                "Upstream row over the $1,000/M per-token ceiling: {}",
                shorten_error_for_human_eye(err)
            )),
        );
    }
    if err.contains("previously-known models retained") {
        return (
            "Pricing manifest refresh below the retention floor".to_string(),
            Some(format!(
                "Fewer than 95% of previously-known models survived this refresh. Details: {}",
                shorten_error_for_human_eye(err)
            )),
        );
    }
    if err.contains("exceeds") && err.contains("-byte cap") {
        return (
            "Pricing manifest refresh rejected: payload too large".to_string(),
            Some(format!(
                "Upstream payload exceeded the 10 MB size cap. Details: {}",
                shorten_error_for_human_eye(err)
            )),
        );
    }
    if err.contains("negative or NaN price") {
        return (
            "Pricing manifest refresh rejected: malformed price value".to_string(),
            Some(format!(
                "One or more upstream rows had a negative or NaN price. Details: {}",
                shorten_error_for_human_eye(err)
            )),
        );
    }
    if err.contains("upstream fetch failed") || err.contains("upstream read failed") {
        return (
            "Pricing manifest refresh could not reach upstream".to_string(),
            Some("Network error fetching the LiteLLM manifest. Check connectivity, or set `BUDI_PRICING_REFRESH=0` to disable the refresher."
                .to_string()),
        );
    }
    // Fallback: preserve the daemon's exact text so no information is
    // dropped. Still reads better than `Daemon returned 502 Bad Gateway
    // { ... JSON ... }`.
    (format!("Refresh failed: {err}"), None)
}

/// Trim the daemon's raw `validation rejected:` prefix so the wrapped
/// output doesn't repeat the word "rejected" next to the headline.
fn shorten_error_for_human_eye(err: &str) -> String {
    err.strip_prefix("validation rejected: ")
        .unwrap_or(err)
        .to_string()
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

    // ADR-0091 §2 amendment (8.3.1 / #483): rows the most-recent
    // refresh tick skipped for failing per-row sanity (NaN, negative,
    // or > $1,000/M). Pre-8.3.1 a single bad row would whole-payload-
    // reject the refresh; the amendment surfaces them here instead.
    let rejected = body
        .get("rejected_upstream_rows")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !rejected.is_empty() {
        println!();
        println!(
            "  {yellow}!{reset} {bold}Rejected upstream rows{reset} {dim}(skipped by row-level sanity; rest of manifest still refreshed){reset}"
        );
        for entry in rejected.iter().take(10) {
            let model = entry.get("model_id").and_then(Value::as_str).unwrap_or("?");
            let reason = entry.get("reason").and_then(Value::as_str).unwrap_or("?");
            println!("    {dim}•{reset} {model} — {reason}");
        }
        if rejected.len() > 10 {
            println!(
                "    {dim}• … {} more (run with --format json for the full list){reset}",
                rejected.len() - 10
            );
        }
    }
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #493 (RC-3): every `ValidationError::Display` shape from
    /// `budi_core::pricing::ValidationError` has a matching classifier
    /// arm. The pre-fix pattern was that a validation failure
    /// surfaced as `Error: Daemon returned 502 Bad Gateway: {...raw
    /// JSON...}` — this test locks in the friendly wrap for every
    /// variant so a future Display string change fails here first.
    #[test]
    fn classify_refresh_error_covers_every_validation_variant() {
        let cases: &[(&str, &str)] = &[
            // SanityCeilingExceeded
            (
                "validation rejected: model wandb/Qwen/Qwen3-Coder-480B-A35B-Instruct price $100000.00/M exceeds sanity ceiling $1000/M",
                "sanity ceiling",
            ),
            // RetentionBelowFloor
            (
                "validation rejected: only 80 of required 95 previously-known models retained",
                "retention floor",
            ),
            // PayloadTooLarge
            (
                "validation rejected: payload 11000000 bytes exceeds 10485760-byte cap",
                "payload too large",
            ),
            // NegativePrice / NaN
            (
                "validation rejected: model foo has a negative or NaN price",
                "malformed price",
            ),
            // Upstream network failure
            (
                "upstream fetch failed: request timed out",
                "could not reach upstream",
            ),
        ];
        for (raw, expected_headline_fragment) in cases {
            let (headline, detail) = classify_refresh_error(raw);
            assert!(
                headline
                    .to_lowercase()
                    .contains(&expected_headline_fragment.to_lowercase()),
                "headline for {raw:?} was {headline:?}, expected to contain {expected_headline_fragment:?}",
            );
            assert!(
                detail.is_some(),
                "every classified variant should have a non-empty detail line (raw={raw:?})",
            );
            // The friendly output must NOT contain the raw
            // `validation rejected:` prefix — that's daemon-speak.
            assert!(
                !headline.contains("validation rejected"),
                "headline must drop the 'validation rejected:' prefix (raw={raw:?}, headline={headline:?})",
            );
        }

        // Fallback path: preserve the exact text so nothing is lost.
        let (headline, detail) = classify_refresh_error("some unexpected daemon error");
        assert_eq!(headline, "Refresh failed: some unexpected daemon error");
        assert!(detail.is_none());
    }

    #[test]
    fn shorten_error_strips_validation_rejected_prefix() {
        assert_eq!(
            shorten_error_for_human_eye("validation rejected: foo bar"),
            "foo bar"
        );
        assert_eq!(shorten_error_for_human_eye("foo bar"), "foo bar");
        assert_eq!(shorten_error_for_human_eye(""), "");
    }
}
