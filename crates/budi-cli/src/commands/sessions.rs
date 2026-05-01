use anyhow::{Context, Result};
use budi_core::analytics::SessionHealth;
use budi_core::pricing::display;

use crate::StatsPeriod;
use crate::client::DaemonClient;
use crate::commands::stats::{format_cost_cents, format_tokens, period_date_range, period_label};

use super::ansi;
use super::print_json;

/// Width budget for the model column in the text-view session list. The
/// canonical display name (`pricing::display::combined_label`) rarely
/// exceeds 26 chars for the model families we surface; the column stays
/// wide enough to render the full canonical name without mid-word
/// truncation under normal traffic, but still fits an 80-column
/// terminal alongside repo + cost columns.
const MODEL_COL_WIDTH: usize = 28;

/// Short-UUID prefix length rendered by `budi sessions` by default
/// (#445). `--full-uuid` surfaces the 36-char identifier for scripting
/// and for `budi sessions <id>` lookup. Eight hex chars is long enough
/// to remain unambiguous at the < 1M session scale Budi is designed
/// around; the fresh-user smoke pass showed 36 chars dominating the
/// visible row width.
const SHORT_UUID_LEN: usize = 8;

pub fn cmd_sessions(
    period: StatsPeriod,
    search: Option<&str>,
    ticket: Option<&str>,
    activity: Option<&str>,
    limit: usize,
    full_uuid: bool,
    json_output: bool,
) -> Result<()> {
    let client = DaemonClient::connect().context(
        "Could not reach budi daemon. Run `budi init` to set up, or `budi doctor` to diagnose.",
    )?;

    let (since, until) = period_date_range(period);
    let sessions = client.sessions(
        since.as_deref(),
        until.as_deref(),
        search,
        ticket,
        activity,
        limit,
        0,
    )?;

    if json_output {
        print_json(&sessions)?;
        return Ok(());
    }

    let bold_cyan = ansi("\x1b[1;36m");
    let bold = ansi("\x1b[1m");
    let dim = ansi("\x1b[90m");
    let yellow = ansi("\x1b[33m");
    let green = ansi("\x1b[32m");
    let red = ansi("\x1b[31m");
    let reset = ansi("\x1b[0m");

    println!();
    // Build a compact filter suffix so the header shows which attribution
    // dimension scoped the result. Both ticket and activity live in the
    // same space (tag-derived filters) and can compose in principle.
    let mut filter_bits: Vec<String> = Vec::new();
    if let Some(t) = ticket {
        filter_bits.push(format!("ticket: {t}"));
    }
    if let Some(a) = activity {
        filter_bits.push(format!("activity: {a}"));
    }
    if filter_bits.is_empty() {
        println!(
            "  {bold_cyan} Sessions{reset} — {bold}{}{reset} {dim}({} total){reset}",
            period_label(period),
            sessions.total_count
        );
    } else {
        println!(
            "  {bold_cyan} Sessions{reset} — {bold}{}{reset} {dim}({}, {} total){reset}",
            period_label(period),
            filter_bits.join(", "),
            sessions.total_count
        );
    }
    println!("  {dim}{}{reset}", "─".repeat(80));

    if sessions.sessions.is_empty() {
        println!("  No sessions for this period.");
        println!();
        return Ok(());
    }

    let mut has_extra_models = false;
    for s in &sessions.sessions {
        let time = s
            .started_at
            .as_deref()
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map(|dt| {
                dt.with_timezone(&chrono::Local)
                    .format("%m/%d %H:%M")
                    .to_string()
            })
            .unwrap_or_else(|| "--".to_string());

        let raw_model = s.models.first().map(|m| m.as_str()).unwrap_or("--");
        let model_display = if raw_model == "--" {
            "--".to_string()
        } else {
            display::resolve(raw_model).combined_label()
        };
        let extra_count = s.models.len().saturating_sub(1);
        let model_extra = if extra_count > 0 {
            has_extra_models = true;
            format!(" +{extra_count}")
        } else {
            String::new()
        };
        let model_cell =
            truncate_on_char_boundary(&format!("{model_display}{model_extra}"), MODEL_COL_WIDTH);

        let repo = s
            .repo_ids
            .first()
            .map(|r| r.rsplit('/').next().unwrap_or(r))
            .unwrap_or("--");

        let health = match s.health_state.as_deref() {
            Some("green") => format!("{green}●{reset}"),
            Some("yellow") => format!("{yellow}●{reset}"),
            Some("red") => format!("{red}●{reset}"),
            _ => format!("{dim}○{reset}"),
        };

        let cost_str = if s.cost_lag_hint.is_some() {
            format!("{}*", format_cost_cents(s.cost_cents))
        } else {
            format_cost_cents(s.cost_cents)
        };

        let id_cell = render_session_id(&s.id, full_uuid);

        println!(
            "  {health} {dim}{time}{reset}  {dim}{id_cell}{reset}  {model_cell:<width$}  {repo:<12}  {yellow}{cost_str:>8}{reset}",
            width = MODEL_COL_WIDTH,
        );
    }

    let has_lag = sessions.sessions.iter().any(|s| s.cost_lag_hint.is_some());
    if has_lag {
        println!("  {dim}* {}{reset}", budi_core::analytics::CURSOR_LAG_HINT);
    }
    if has_extra_models {
        // Footer legend for the `+N` notation surfaced in the model
        // column. #445 acceptance: the undocumented compactor needs a
        // one-line explanation rather than leaving readers to guess.
        println!(
            "  {dim}+N = N additional model(s) used in this session (pass --full-uuid and `budi sessions <id>` to see the full list){reset}"
        );
    }

    if sessions.total_count > sessions.sessions.len() as u64 {
        println!(
            "  {dim}… {} more sessions (use --limit or --search to filter){reset}",
            sessions.total_count - sessions.sessions.len() as u64
        );
    }

    println!();
    Ok(())
}

pub fn cmd_session_detail(session_id: &str, json_output: bool) -> Result<()> {
    let client = DaemonClient::connect().context(
        "Could not reach budi daemon. Run `budi init` to set up, or `budi doctor` to diagnose.",
    )?;

    let resolved_id = if session_id == "latest" || session_id == "current" {
        resolve_session_token(&client, session_id)?
    } else {
        session_id.to_string()
    };

    let session = client.session_detail(&resolved_id)?;

    let Some(s) = session else {
        anyhow::bail!("Session '{}' not found.", resolved_id);
    };
    let session_id = resolved_id.as_str();

    if json_output {
        let tags = client.session_tags(session_id).unwrap_or_default();
        let health = client.session_health(Some(session_id)).ok();
        let mut obj = serde_json::to_value(&s)?;
        if let Some(map) = obj.as_object_mut() {
            map.insert("tags".to_string(), serde_json::to_value(&tags)?);
            if let Some(h) = health {
                map.insert("health".to_string(), serde_json::to_value(&h)?);
            }
        }
        print_json(&obj)?;
        return Ok(());
    }

    let bold_cyan = ansi("\x1b[1;36m");
    let bold = ansi("\x1b[1m");
    let dim = ansi("\x1b[90m");
    let yellow = ansi("\x1b[33m");
    let green = ansi("\x1b[32m");
    let reset = ansi("\x1b[0m");

    println!();
    println!(
        "  {bold_cyan} Session{reset} {bold}{}{reset}",
        &session_id[..session_id.len().min(12)]
    );
    println!("  {dim}{}{reset}", "─".repeat(50));

    if let Some(ref title) = s.title {
        println!("  {bold}Title{reset}      {title}");
    }
    println!("  {bold}Agent{reset}      {}", s.provider);
    println!(
        "  {bold}Models{reset}     {}",
        if s.models.is_empty() {
            "--".to_string()
        } else {
            s.models.join(", ")
        }
    );

    if let Some(ref t) = s.started_at {
        println!("  {bold}Started{reset}    {t}");
    }
    if let Some(ms) = s.duration_ms {
        println!("  {bold}Duration{reset}   {}", format_duration_ms(ms));
    }

    if !s.repo_ids.is_empty() {
        println!("  {bold}Repos{reset}      {}", s.repo_ids.join(", "));
    }
    if !s.git_branches.is_empty() {
        println!("  {bold}Branches{reset}   {}", s.git_branches.join(", "));
    }
    // R1.5 (#293): surface the rule-based work outcome whenever we
    // could derive one. Rationale is intentionally shown in dim text
    // so operators can see *why* a label was picked without it
    // competing visually with the main session stats.
    if let Some(ref outcome) = s.work_outcome {
        let colored = match outcome.as_str() {
            "committed" | "branch_merged" => format!("{green}{outcome}{reset}"),
            "no_commit" => format!("{yellow}{outcome}{reset}"),
            _ => outcome.to_string(),
        };
        println!("  {bold}Outcome{reset}    {colored}");
        if let Some(ref rationale) = s.work_outcome_rationale {
            println!("             {dim}{rationale}{reset}");
        }
    }

    println!();
    println!("  {bold}Messages{reset}   {}", s.message_count);
    println!(
        "  {bold}Input{reset}      {}",
        format_tokens(s.input_tokens)
    );
    println!(
        "  {bold}Output{reset}     {}",
        format_tokens(s.output_tokens)
    );
    println!(
        "  {bold}Est. cost{reset}  {yellow}{}{reset}",
        format_cost_cents(s.cost_cents)
    );
    if let Some(ref hint) = s.cost_lag_hint {
        println!("             {dim}{hint}{reset}");
    }

    // Tags
    if let Ok(tags) = client.session_tags(session_id)
        && !tags.is_empty()
    {
        println!();
        println!("  {bold}Tags{reset}");
        for tag in &tags {
            println!("    {dim}{}{reset}: {}", tag.key, tag.value);
        }
    }

    // Vitals (inlined from the former `budi vitals` command — issue #585).
    // Every session detail now renders the full vitals block (Prompt
    // Growth, Cache Reuse, Retry Loops, Cost Acceleration) so the
    // common-case detail view is genuinely useful without a parallel
    // `budi vitals` verb.
    if let Ok(health) = client.session_health(Some(session_id)) {
        println!();
        render_vitals_block(&health);
    }

    println!();
    Ok(())
}

/// Resolve a literal session token (`latest` or `current`) into a
/// concrete session id by asking the daemon's
/// `/analytics/sessions/resolve` endpoint (#603).
///
/// `current` includes the CLI's cwd as a query param so the daemon
/// can walk `~/.claude/projects/<encoded-cwd>/` for the most-recent
/// transcript. When the daemon falls back to `latest` (no transcripts
/// for this cwd) it returns a `fallback_reason` we mirror verbatim
/// on stderr so the user understands why their `/budi` invocation
/// surfaced a different session than they expected.
fn resolve_session_token(client: &DaemonClient, token: &str) -> Result<String> {
    let cwd = std::env::current_dir().ok();
    let cwd_str = cwd.as_ref().and_then(|p| p.to_str());
    let resolved = client
        .resolve_session_token(token, cwd_str)
        .map_err(|e| {
            // The daemon returns 404 when there are no sessions at
            // all — wrap that into the same friendly nudge we used
            // to print client-side so the failure mode is unchanged
            // for fresh users.
            if format!("{e:#}").contains("no sessions found") {
                anyhow::anyhow!("No sessions yet — run an AI agent and try again.")
            } else {
                e
            }
        })?;
    if let Some(ref reason) = resolved.fallback_reason {
        eprintln!("budi: {reason}");
    }
    Ok(resolved.session_id)
}

/// Render the four-vital health block for a session. Inlined into
/// `budi sessions <id>` (issue #585) and replaces the standalone
/// `budi vitals` command output. Skips the leading "N messages · $X.XX
/// total" summary because the surrounding detail view already shows
/// those fields.
fn render_vitals_block(h: &SessionHealth) {
    let detail_name = |name: &str| -> String {
        match name {
            "context_drag" => "Prompt Growth".to_string(),
            "cache_efficiency" => "Cache Reuse".to_string(),
            "thrashing" => "Retry Loops".to_string(),
            "cost_acceleration" => "Cost Acceleration".to_string(),
            _ => name.to_string(),
        }
    };

    let bold = ansi("\x1b[1m");
    let dim = ansi("\x1b[90m");
    let reset = ansi("\x1b[0m");
    let green = ansi("\x1b[32m");
    let yellow = ansi("\x1b[33m");
    let red = ansi("\x1b[31m");

    let state_icon = |s: &str| -> &str {
        match s {
            "red" => "🔴",
            "yellow" => "🟡",
            "gray" | "insufficient_data" => "⚪",
            _ => "🟢",
        }
    };
    let state_color = |s: &str| -> &str {
        match s {
            "red" => red,
            "yellow" => yellow,
            "gray" | "insufficient_data" => dim,
            _ => green,
        }
    };
    let state_label = |s: &str| -> String {
        match s {
            "insufficient_data" => "INSUFFICIENT DATA".to_string(),
            _ => s.to_uppercase(),
        }
    };

    let icon = state_icon(&h.state);
    let color = state_color(&h.state);
    println!(
        "  {icon} {bold}Vitals: {color}{}{reset}",
        state_label(&h.state)
    );

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
                println!("    {vi} {bold}{name}{reset}: {vc}{}{reset}", v.label);
            }
            None => {
                println!("    {dim}⚪ {name}: N/A{reset}");
            }
        }
    }

    if !h.details.is_empty() {
        println!();
        for d in &h.details {
            let di = state_icon(&d.state);
            let dc = state_color(&d.state);
            println!(
                "  {di} {dc}{bold}{} ({}):{reset}",
                detail_name(&d.vital),
                d.label
            );
            println!("    {}", d.tip);
            for action in &d.actions {
                println!("    - {action}");
            }
            println!();
        }
    }

    if h.state == "green" || h.state == "insufficient_data" {
        let tip_color = state_color(&h.state);
        println!("    {tip_color}{}{reset}", h.tip);
    }
}

fn format_duration_ms(ms: i64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// Render the session id cell. Default is the 8-char short form
/// (#445); `--full-uuid` surfaces the complete identifier.
///
/// UTF-8 safety: the session id is produced by `uuid::Uuid::to_string`
/// and is always ASCII hex + dashes, so ASCII-byte truncation is
/// boundary-safe. The helper still uses char iteration rather than
/// byte slicing so a future non-ASCII id shape (unlikely but not
/// impossible) would not panic — the #389 / #383 / #404 bug class.
fn render_session_id(id: &str, full_uuid: bool) -> String {
    if full_uuid {
        return id.to_string();
    }
    id.chars().take(SHORT_UUID_LEN).collect()
}

/// Truncate `s` to at most `max` characters (not bytes), appending an
/// ellipsis when the string was actually shortened. Returns the input
/// unchanged when it already fits. Used for the model column so a
/// canonical display label that exceeds the column width degrades
/// gracefully without slicing mid-codepoint.
fn truncate_on_char_boundary(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let char_count = s.chars().count();
    if char_count <= max {
        return s.to_string();
    }
    let take = max.saturating_sub(1);
    let mut out: String = s.chars().take(take).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// Tests (#445)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_uuid_is_eight_chars_by_default() {
        let id = "1d027675-4ad0-43b2-b396-88b6ee28f7ba";
        let rendered = render_session_id(id, false);
        assert_eq!(rendered, "1d027675");
        assert_eq!(rendered.len(), SHORT_UUID_LEN);
    }

    #[test]
    fn full_uuid_returns_unchanged_string() {
        let id = "1d027675-4ad0-43b2-b396-88b6ee28f7ba";
        let rendered = render_session_id(id, true);
        assert_eq!(rendered, id);
    }

    #[test]
    fn short_uuid_is_utf8_boundary_safe_on_non_ascii_id() {
        // Regression guard for the #389 / #383 / #404 byte-slice bug
        // class — a hypothetical non-ASCII session id must not panic
        // even though canonical Budi ids are ASCII hex.
        let id = "café-1234-5678-abcd";
        let rendered = render_session_id(id, false);
        // Eight chars — `c`, `a`, `f`, `é`, `-`, `1`, `2`, `3`.
        assert_eq!(rendered.chars().count(), SHORT_UUID_LEN);
    }

    #[test]
    fn truncate_on_char_boundary_preserves_short_strings() {
        assert_eq!(
            truncate_on_char_boundary("Claude Opus 4.7", 28),
            "Claude Opus 4.7"
        );
    }

    #[test]
    fn truncate_on_char_boundary_uses_ellipsis_on_overflow() {
        let out = truncate_on_char_boundary("Claude Opus 4.7 · thinking-high (experimental)", 28);
        // Exactly 28 chars — 27 from the source + 1 ellipsis.
        assert_eq!(out.chars().count(), 28);
        assert!(out.ends_with('…'));
        // Must not panic, must not slice mid-codepoint.
        assert!(out.is_char_boundary(out.len()));
    }

    #[test]
    fn truncate_on_char_boundary_handles_multibyte_codepoints() {
        // Five-character string with four multi-byte codepoints
        // followed by an ASCII byte. `max = 3` keeps two chars plus
        // the ellipsis. The previous `shorten_model` implementation
        // used byte indexing, which would have panicked on this.
        let out = truncate_on_char_boundary("αβγδe", 3);
        assert_eq!(out.chars().count(), 3);
        assert!(out.ends_with('…'));
        assert!(out.starts_with('α'));
    }

    #[test]
    fn truncate_on_char_boundary_zero_max_returns_empty() {
        assert_eq!(truncate_on_char_boundary("anything", 0), "");
    }
}
