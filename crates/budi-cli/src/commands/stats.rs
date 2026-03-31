use anyhow::{Context, Result};
use budi_core::analytics;
use chrono::{Datelike, Local, NaiveDate, TimeZone};

use crate::StatsPeriod;
use crate::client::DaemonClient;

use super::ansi;

pub fn period_label(period: StatsPeriod) -> &'static str {
    match period {
        StatsPeriod::Today => "Today",
        StatsPeriod::Week => "This week",
        StatsPeriod::Month => "This month",
        StatsPeriod::All => "All time",
    }
}

/// Convert a local NaiveDate at midnight to a UTC RFC3339 string.
/// This matches the statusline endpoint's date handling so that
/// CLI and dashboard/statusline produce identical time ranges.
fn local_midnight_to_utc(date: NaiveDate) -> String {
    let local_dt = Local
        .from_local_datetime(&date.and_hms_opt(0, 0, 0).unwrap())
        .latest()
        .unwrap_or_else(|| chrono::Utc::now().with_timezone(&Local));
    local_dt.with_timezone(&chrono::Utc).to_rfc3339()
}

pub fn period_date_range(period: StatsPeriod) -> (Option<String>, Option<String>) {
    let today = Local::now().date_naive();
    match period {
        StatsPeriod::Today => {
            let since = local_midnight_to_utc(today);
            (Some(since), None)
        }
        StatsPeriod::Week => {
            let weekday = today.weekday().num_days_from_monday();
            let monday = today - chrono::Duration::days(weekday as i64);
            let since = local_midnight_to_utc(monday);
            (Some(since), None)
        }
        StatsPeriod::Month => {
            let first = NaiveDate::from_ymd_opt(today.year(), today.month(), 1)
                .expect("valid first-of-month date");
            let since = local_midnight_to_utc(first);
            (Some(since), None)
        }
        StatsPeriod::All => (None, None),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn cmd_stats(
    period: StatsPeriod,
    projects: bool,
    branches: bool,
    branch: Option<String>,
    models: bool,
    provider: Option<String>,
    tag: Option<String>,
    json_output: bool,
) -> Result<()> {
    // Validate --provider early with a helpful error message
    const KNOWN_PROVIDERS: &[&str] = &["claude_code", "cursor"];
    if let Some(ref p) = provider
        && !KNOWN_PROVIDERS.contains(&p.as_str())
    {
        anyhow::bail!(
            "Unknown provider '{}'. Available providers: {}",
            p,
            KNOWN_PROVIDERS.join(", ")
        );
    }

    let client = DaemonClient::connect().context(
        "Could not reach budi daemon. Run `budi init` to set up, or `budi doctor` to diagnose.",
    )?;

    if let Some(ref tag_filter) = tag {
        return cmd_stats_tags(&client, period, tag_filter, json_output);
    }

    if let Some(ref br) = branch {
        return cmd_stats_branch_detail(&client, period, br, json_output);
    }

    if branches {
        return cmd_stats_branches(&client, period, json_output);
    }

    if models {
        return cmd_stats_models(&client, period, json_output);
    }

    if projects {
        if json_output {
            let (since, until) = period_date_range(period);
            let data = client.projects(since.as_deref(), until.as_deref(), 50)?;
            println!("{}", serde_json::to_string_pretty(&data)?);
            return Ok(());
        }
        return cmd_stats_projects(&client, period);
    }

    if json_output {
        let (since, until) = period_date_range(period);
        let summary = client.summary(since.as_deref(), until.as_deref(), provider.as_deref())?;
        let cost = client.cost(since.as_deref(), until.as_deref(), provider.as_deref())?;
        let mut obj = serde_json::to_value(&summary)?;
        if let Some(map) = obj.as_object_mut() {
            map.insert(
                "total_cost".to_string(),
                serde_json::json!(cost.total_cost),
            );
            map.insert(
                "input_cost".to_string(),
                serde_json::json!(cost.input_cost),
            );
            map.insert(
                "output_cost".to_string(),
                serde_json::json!(cost.output_cost),
            );
            map.insert(
                "cache_write_cost".to_string(),
                serde_json::json!(cost.cache_write_cost),
            );
            map.insert(
                "cache_read_cost".to_string(),
                serde_json::json!(cost.cache_read_cost),
            );
            map.insert(
                "cache_savings".to_string(),
                serde_json::json!(cost.cache_savings),
            );
        }
        println!("{}", serde_json::to_string_pretty(&obj)?);
        return Ok(());
    }

    // When no provider filter and multiple agents detected, show breakdown
    if provider.is_none() {
        let (since, until) = period_date_range(period);
        let providers = client.providers(since.as_deref(), until.as_deref())?;
        if providers.len() > 1 {
            cmd_stats_multi_agent(&client, period, &providers)?;
            return Ok(());
        }
    }

    cmd_stats_summary_filtered(&client, period, provider.as_deref())
}

fn cmd_stats_summary_filtered(
    client: &DaemonClient,
    period: StatsPeriod,
    provider: Option<&str>,
) -> Result<()> {
    let (since, until) = period_date_range(period);
    let summary = client.summary(since.as_deref(), until.as_deref(), provider)?;

    let period_label = period_label(period);
    let provider_label = provider.unwrap_or("all");

    let bold_cyan = ansi("\x1b[1;36m");
    let bold = ansi("\x1b[1m");
    let dim = ansi("\x1b[90m");
    let yellow = ansi("\x1b[33m");
    let green = ansi("\x1b[32m");
    let reset = ansi("\x1b[0m");

    println!();
    if provider.is_some() {
        println!(
            "  {bold_cyan} budi stats{reset} — {bold}{}{reset} {dim}({}){reset}",
            period_label, provider_label
        );
    } else {
        println!(
            "  {bold_cyan} budi stats{reset} — {bold}{}{reset}",
            period_label
        );
    }
    println!("  {dim}{}{reset}", "─".repeat(40));

    if summary.total_messages == 0 {
        println!("  No data for this period.");
        println!();
        return Ok(());
    }

    println!(
        "  {bold}Messages{reset}     {} {dim}({} user, {} assistant){reset}",
        summary.total_messages, summary.total_user_messages, summary.total_assistant_messages
    );
    println!();

    println!(
        "  {bold}Input tokens{reset}  {}",
        format_tokens(summary.total_input_tokens)
    );
    println!(
        "  {bold}Output tokens{reset} {}",
        format_tokens(summary.total_output_tokens)
    );

    // Cost breakdown
    let est = client.cost(since.as_deref(), until.as_deref(), provider)?;
    println!();
    println!(
        "  {bold}Est. cost{reset}     {yellow}{}{reset}",
        format_cost(est.total_cost)
    );
    println!(
        "  {dim}  input {}  output {}  cache write {}  cache read {}{reset}",
        format_cost(est.input_cost),
        format_cost(est.output_cost),
        format_cost(est.cache_write_cost),
        format_cost(est.cache_read_cost)
    );
    if est.cache_savings > 0.0 {
        println!(
            "  {green}  cache savings {}{reset}",
            format_cost(est.cache_savings)
        );
    }

    println!();
    Ok(())
}

fn cmd_stats_multi_agent(
    client: &DaemonClient,
    period: StatsPeriod,
    providers: &[analytics::ProviderStats],
) -> Result<()> {
    let period_label = period_label(period);

    let bold_cyan = ansi("\x1b[1;36m");
    let bold = ansi("\x1b[1m");
    let dim = ansi("\x1b[90m");
    let cyan = ansi("\x1b[36m");
    let yellow = ansi("\x1b[33m");
    let reset = ansi("\x1b[0m");

    println!();
    println!(
        "  {bold_cyan} budi stats{reset} — {bold}{}{reset}",
        period_label
    );
    println!("  {dim}{}{reset}", "─".repeat(40));

    // Per-agent breakdown
    println!("  {bold}Agents{reset}");
    for ps in providers {
        let total_tokens =
            ps.input_tokens + ps.output_tokens + ps.cache_creation_tokens + ps.cache_read_tokens;
        // Use ground-truth cost_cents when available, fall back to estimated
        let cost = if ps.total_cost_cents > 0.0 {
            ps.total_cost_cents / 100.0
        } else {
            ps.estimated_cost
        };
        println!(
            "    {cyan}{:<14}{reset} {:>5} msgs  {}  {yellow}{}{reset}",
            ps.display_name,
            ps.message_count,
            format_tokens(total_tokens),
            format_cost(cost),
        );
    }
    println!();

    // Show combined summary
    let (since, until) = period_date_range(period);
    let summary = client.summary(since.as_deref(), until.as_deref(), None)?;

    println!(
        "  {bold}Total{reset}        {} messages",
        summary.total_messages
    );

    println!(
        "  {bold}Tokens{reset}       {} in, {} out",
        format_tokens(summary.total_input_tokens),
        format_tokens(summary.total_output_tokens),
    );

    println!();
    Ok(())
}

fn cmd_stats_projects(client: &DaemonClient, period: StatsPeriod) -> Result<()> {
    let (since, until) = period_date_range(period);
    let repos = client.projects(since.as_deref(), until.as_deref(), 15)?;

    let period_label = period_label(period);

    let bold_cyan = ansi("\x1b[1;36m");
    let bold = ansi("\x1b[1m");
    let dim = ansi("\x1b[90m");
    let cyan = ansi("\x1b[36m");
    let yellow = ansi("\x1b[33m");
    let reset = ansi("\x1b[0m");

    println!();
    println!(
        "  {bold_cyan} Repositories{reset} — {bold}{}{reset}",
        period_label
    );
    println!("  {dim}{}{reset}", "─".repeat(50));

    if repos.is_empty() {
        println!("  No data for this period.");
        println!();
        return Ok(());
    }

    let max_cost = repos
        .iter()
        .map(|r| r.cost_cents)
        .fold(0.0_f64, f64::max)
        .max(0.01);
    for r in &repos {
        let bar_len = ((r.cost_cents / max_cost) * 16.0) as usize;
        let bar: String = "\u{2588}".repeat(bar_len);
        println!(
            "    {bold}{:<28}{reset} {yellow}{:>8}{reset}  {cyan}{}{reset}",
            r.repo_id,
            format_cost_cents(r.cost_cents),
            bar
        );
    }

    println!();
    Ok(())
}

fn cmd_stats_branches(client: &DaemonClient, period: StatsPeriod, json_output: bool) -> Result<()> {
    let (since, until) = period_date_range(period);
    let branches = client.branches(since.as_deref(), until.as_deref())?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&branches)?);
        return Ok(());
    }

    let period_label = period_label(period);

    let bold_cyan = ansi("\x1b[1;36m");
    let bold = ansi("\x1b[1m");
    let dim = ansi("\x1b[90m");
    let cyan = ansi("\x1b[36m");
    let yellow = ansi("\x1b[33m");
    let reset = ansi("\x1b[0m");

    println!();
    println!(
        "  {bold_cyan} Branches{reset} — {bold}{}{reset}",
        period_label
    );
    println!("  {dim}{}{reset}", "─".repeat(50));

    if branches.is_empty() {
        println!("  No branch data for this period.");
        println!();
        return Ok(());
    }

    let max_cost = branches
        .iter()
        .map(|b| b.cost_cents)
        .fold(0.0_f64, f64::max)
        .max(0.01);
    for b in &branches {
        let branch_name = b
            .git_branch
            .strip_prefix("refs/heads/")
            .unwrap_or(&b.git_branch);
        let repo = if b.repo_id.is_empty() {
            "--".to_string()
        } else {
            b.repo_id
                .rsplit('/')
                .next()
                .unwrap_or(&b.repo_id)
                .to_string()
        };
        let bar_len = ((b.cost_cents / max_cost) * 16.0) as usize;
        let bar: String = "\u{2588}".repeat(bar_len);
        println!(
            "    {bold}{:<28}{reset} {yellow}{:>8}{reset}  {dim}{}{reset}  {cyan}{}{reset}",
            branch_name,
            format_cost_cents(b.cost_cents),
            repo,
            bar
        );
    }

    println!();
    Ok(())
}

fn cmd_stats_branch_detail(
    client: &DaemonClient,
    period: StatsPeriod,
    branch: &str,
    json_output: bool,
) -> Result<()> {
    let (since, until) = period_date_range(period);
    let result = client.branch_detail(branch, since.as_deref(), until.as_deref())?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }

    let period_label = period_label(period);

    let bold_cyan = ansi("\x1b[1;36m");
    let bold = ansi("\x1b[1m");
    let dim = ansi("\x1b[90m");
    let yellow = ansi("\x1b[33m");
    let reset = ansi("\x1b[0m");

    println!();
    println!(
        "  {bold_cyan} Branch{reset} {bold}{}{reset} — {dim}{}{reset}",
        branch, period_label
    );
    println!("  {dim}{}{reset}", "─".repeat(40));

    match result {
        Some(b) => {
            if !b.repo_id.is_empty() {
                println!("  {bold}Repo{reset}       {}", b.repo_id);
            }
            println!("  {bold}Sessions{reset}   {}", b.session_count);
            println!("  {bold}Messages{reset}   {}", b.message_count);
            println!(
                "  {bold}Input{reset}      {}",
                format_tokens(b.input_tokens)
            );
            println!(
                "  {bold}Output{reset}     {}",
                format_tokens(b.output_tokens)
            );
            println!(
                "  {bold}Est. cost{reset}  {yellow}{}{reset}",
                format_cost_cents(b.cost_cents)
            );
        }
        None => {
            println!("  No data found for branch '{}'.", branch);
            println!("  Tip: run `budi sync` first if you haven't synced recently.");
            println!("  Run `budi stats --branches` to see available branches.");
        }
    }

    println!();
    Ok(())
}

fn cmd_stats_models(client: &DaemonClient, period: StatsPeriod, json_output: bool) -> Result<()> {
    let (since, until) = period_date_range(period);
    let models = client.models(since.as_deref(), until.as_deref())?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&models)?);
        return Ok(());
    }

    let period_label = period_label(period);

    let bold_cyan = ansi("\x1b[1;36m");
    let bold = ansi("\x1b[1m");
    let dim = ansi("\x1b[90m");
    let cyan = ansi("\x1b[36m");
    let reset = ansi("\x1b[0m");

    println!();
    println!(
        "  {bold_cyan} Model usage{reset} — {bold}{}{reset}",
        period_label
    );
    println!("  {dim}{}{reset}", "─".repeat(50));

    if models.is_empty() {
        println!("  No data for this period.");
        println!();
        return Ok(());
    }

    let yellow = ansi("\x1b[33m");

    let max_msgs = models
        .iter()
        .map(|m| m.message_count)
        .max()
        .unwrap_or(1)
        .max(1);
    for m in &models {
        let bar_len = ((m.message_count as f64 / max_msgs as f64) * 16.0) as usize;
        let bar: String = "█".repeat(bar_len);
        let total_tok =
            m.input_tokens + m.output_tokens + m.cache_read_tokens + m.cache_creation_tokens;
        println!(
            "    {bold}{:<30}{reset} {:>5} msgs  {:>8} tok  {yellow}{:>8}{reset}  {cyan}{}{reset}",
            m.model,
            m.message_count,
            format_tokens(total_tok),
            format_cost_cents(m.cost_cents),
            bar
        );
    }

    println!();
    Ok(())
}

// ─── Formatting Utilities ────────────────────────────────────────────────────

pub fn format_tokens(n: u64) -> String {
    if n >= 1_000_000_000 {
        format!("{:.1}B", n as f64 / 1_000_000_000.0)
    } else if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        format!("{}", n)
    }
}

pub use super::format_cost;

pub fn format_cost_cents(cents: f64) -> String {
    format_cost(cents / 100.0)
}

fn cmd_stats_tags(
    client: &DaemonClient,
    period: StatsPeriod,
    tag_filter: &str,
    json_output: bool,
) -> Result<()> {
    let (since, until) = period_date_range(period);

    let data = client.tags(Some(tag_filter), since.as_deref(), until.as_deref(), 30)?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&data)?);
        return Ok(());
    }

    if data.is_empty() {
        println!(
            "No tag data for '{}' ({})",
            tag_filter,
            period_label(period)
        );
        return Ok(());
    }

    let bold = ansi("\x1b[1m");
    let yellow = ansi("\x1b[33m");
    let reset = ansi("\x1b[0m");

    let dim = ansi("\x1b[90m");

    println!(
        "\n{bold}  Tag: {} — {}{reset}\n",
        tag_filter,
        period_label(period)
    );

    println!("  {dim}{:<40} {:>38}{reset}", "VALUE", "COST");
    println!("  {dim}{}{reset}", "─".repeat(78));

    // Find max cost for bar scaling
    let max_cost = data.iter().map(|t| t.cost_cents).fold(0.0f64, f64::max);
    let bar_width: usize = 30;

    for tag in &data {
        let bar_len = if max_cost > 0.0 {
            ((tag.cost_cents / max_cost) * bar_width as f64) as usize
        } else {
            0
        };
        let bar = "█".repeat(bar_len);
        let pad_bar = " ".repeat(bar_width.saturating_sub(bar_len));
        println!(
            "  {:<40} {yellow}{}{reset}{} {:>8}",
            tag.value,
            bar,
            pad_bar,
            format_cost_cents(tag.cost_cents),
        );
    }
    println!();
    Ok(())
}
