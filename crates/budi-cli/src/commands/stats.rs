use anyhow::Result;
use budi_core::analytics;
use chrono::{Datelike, Local, NaiveDate};

use crate::StatsPeriod;
use crate::client::DaemonClient;

fn use_color() -> bool {
    std::env::var("NO_COLOR").is_err()
}

fn ansi(code: &str) -> &str {
    if use_color() { code } else { "" }
}

pub fn period_label(period: StatsPeriod) -> &'static str {
    match period {
        StatsPeriod::Today => "Today",
        StatsPeriod::Week => "This week",
        StatsPeriod::Month => "This month",
        StatsPeriod::All => "All time",
    }
}

pub fn period_date_range(period: StatsPeriod) -> (Option<String>, Option<String>) {
    let today = Local::now().date_naive();
    match period {
        StatsPeriod::Today => {
            let since = today.format("%Y-%m-%dT00:00:00").to_string();
            (Some(since), None)
        }
        StatsPeriod::Week => {
            let weekday = today.weekday().num_days_from_monday();
            let monday = today - chrono::Duration::days(weekday as i64);
            let since = monday.format("%Y-%m-%dT00:00:00").to_string();
            (Some(since), None)
        }
        StatsPeriod::Month => {
            let first = NaiveDate::from_ymd_opt(today.year(), today.month(), 1).unwrap();
            let since = first.format("%Y-%m-%dT00:00:00").to_string();
            (Some(since), None)
        }
        StatsPeriod::All => (None, None),
    }
}

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
    let exclusive_count = [projects, branches, branch.is_some(), models, tag.is_some()]
        .iter()
        .filter(|&&x| x)
        .count();
    if exclusive_count > 1 {
        eprintln!("Error: --projects, --branches, --branch, --models, and --tag are mutually exclusive.");
        std::process::exit(1);
    }

    let client = DaemonClient::connect()?;

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
        println!("{}", serde_json::to_string_pretty(&summary)?);
        return Ok(());
    }

    // When no provider filter and multiple agents detected, show breakdown
    if provider.is_none() && client.provider_count()? > 1 {
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

    let total_input = summary.total_input_tokens
        + summary.total_cache_creation_tokens
        + summary.total_cache_read_tokens;
    println!(
        "  {bold}Input tokens{reset}  {}",
        format_tokens(total_input)
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

    let total_input = summary.total_input_tokens
        + summary.total_cache_creation_tokens
        + summary.total_cache_read_tokens;
    println!(
        "  {bold}Tokens{reset}       {} in, {} out",
        format_tokens(total_input),
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

fn cmd_stats_branches(
    client: &DaemonClient,
    period: StatsPeriod,
    json_output: bool,
) -> Result<()> {
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
            let total_input = b.input_tokens + b.cache_creation_tokens + b.cache_read_tokens;
            println!("  {bold}Input{reset}      {}", format_tokens(total_input));
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
            println!(
                "  No data found for branch '{}'. Run `budi stats --branches` to see available branches.",
                branch
            );
        }
    }

    println!();
    Ok(())
}

fn cmd_stats_models(
    client: &DaemonClient,
    period: StatsPeriod,
    json_output: bool,
) -> Result<()> {
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

    let max_msgs = models.first().map(|m| m.message_count).unwrap_or(1);
    for m in &models {
        let bar_len = ((m.message_count as f64 / max_msgs as f64) * 16.0) as usize;
        let bar: String = "█".repeat(bar_len);
        let total_tok =
            m.input_tokens + m.output_tokens + m.cache_read_tokens + m.cache_creation_tokens;
        println!(
            "    {bold}{:<30}{reset} {:>5} msgs  {:>8} tok  {cyan}{}{reset}",
            m.model,
            m.message_count,
            format_tokens(total_tok),
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

pub fn format_cost(dollars: f64) -> String {
    if dollars >= 1000.0 {
        format!("${:.1}K", dollars / 1000.0)
    } else if dollars >= 100.0 {
        format!("${:.0}", dollars)
    } else if dollars >= 1.0 {
        format!("${:.2}", dollars)
    } else if dollars > 0.0 {
        format!("${:.2}", dollars)
    } else {
        "$0.00".to_string()
    }
}

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

    println!(
        "\n{bold}  Tag: {} — {}{reset}\n",
        tag_filter,
        period_label(period)
    );

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

