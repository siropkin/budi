use anyhow::{Context, Result};

use crate::StatsPeriod;
use crate::client::DaemonClient;
use crate::commands::stats::{format_cost_cents, format_tokens, period_date_range, period_label};

use super::ansi;

pub fn cmd_sessions(
    period: StatsPeriod,
    search: Option<&str>,
    ticket: Option<&str>,
    activity: Option<&str>,
    limit: usize,
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
        println!("{}", serde_json::to_string_pretty(&sessions)?);
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

        let model = s.models.first().map(|m| m.as_str()).unwrap_or("--");
        let model_short = shorten_model(model);
        let model_extra = if s.models.len() > 1 {
            format!(" +{}", s.models.len() - 1)
        } else {
            String::new()
        };

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

        println!(
            "  {health} {dim}{time}{reset}  {dim}{}{reset}  {:<20}  {:<12}  {yellow}{:>8}{reset}",
            &s.id,
            format!("{model_short}{model_extra}"),
            repo,
            cost_str,
        );
    }

    let has_lag = sessions.sessions.iter().any(|s| s.cost_lag_hint.is_some());
    if has_lag {
        println!("  {dim}* {}{reset}", budi_core::analytics::CURSOR_LAG_HINT);
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

    let session = client.session_detail(session_id)?;

    let Some(s) = session else {
        anyhow::bail!("Session '{}' not found.", session_id);
    };

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
        println!("{}", serde_json::to_string_pretty(&obj)?);
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

    // Health
    if let Ok(health) = client.session_health(Some(session_id)) {
        println!();
        let state_icon = match health.state.as_str() {
            "red" => "🔴",
            "yellow" => "🟡",
            "gray" | "insufficient_data" => "⚪",
            _ => "🟢",
        };
        let state_label = match health.state.as_str() {
            "insufficient_data" => "insufficient data".to_string(),
            other => other.to_string(),
        };
        println!("  {state_icon} {bold}Health: {state_label}{reset}");
        if health.state == "green" || health.state == "insufficient_data" {
            println!("    {green}{}{reset}", health.tip);
        }
        for d in &health.details {
            let di = match d.state.as_str() {
                "red" => "🔴",
                "yellow" => "🟡",
                _ => "⚪",
            };
            println!("    {di} {bold}{}{reset}: {}", d.vital, d.tip);
        }
    }

    println!();
    Ok(())
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

fn shorten_model(model: &str) -> &str {
    // Take last part after any slash (org/model → model)
    let base = model.rsplit('/').next().unwrap_or(model);
    // Truncate for display (max 20 chars)
    if base.len() > 20 { &base[..20] } else { base }
}
