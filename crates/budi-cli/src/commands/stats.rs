use anyhow::Result;
use budi_core::analytics;
use chrono::{Datelike, Local, NaiveDate};

use crate::StatsPeriod;
use crate::client::DaemonClient;

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

    println!();
    if provider.is_some() {
        println!(
            "  \x1b[1;36m budi stats\x1b[0m — \x1b[1m{}\x1b[0m \x1b[90m({})\x1b[0m",
            period_label, provider_label
        );
    } else {
        println!(
            "  \x1b[1;36m budi stats\x1b[0m — \x1b[1m{}\x1b[0m",
            period_label
        );
    }
    println!("  \x1b[90m{}\x1b[0m", "─".repeat(40));

    if summary.total_messages == 0 {
        println!("  No data for this period.");
        println!();
        return Ok(());
    }

    println!(
        "  \x1b[1mMessages\x1b[0m     {} \x1b[90m({} user, {} assistant)\x1b[0m",
        summary.total_messages, summary.total_user_messages, summary.total_assistant_messages
    );
    println!();

    let total_input = summary.total_input_tokens
        + summary.total_cache_creation_tokens
        + summary.total_cache_read_tokens;
    println!(
        "  \x1b[1mInput tokens\x1b[0m  {}",
        format_tokens(total_input)
    );
    println!(
        "  \x1b[1mOutput tokens\x1b[0m {}",
        format_tokens(summary.total_output_tokens)
    );

    // Cost breakdown
    let est = client.cost(since.as_deref(), until.as_deref(), provider)?;
    println!();
    println!(
        "  \x1b[1mEst. cost\x1b[0m     \x1b[33m{}\x1b[0m",
        format_cost(est.total_cost)
    );
    println!(
        "  \x1b[90m  input {}  output {}  cache write {}  cache read {}\x1b[0m",
        format_cost(est.input_cost),
        format_cost(est.output_cost),
        format_cost(est.cache_write_cost),
        format_cost(est.cache_read_cost)
    );
    if est.cache_savings > 0.0 {
        println!(
            "  \x1b[32m  cache savings {}\x1b[0m",
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

    println!();
    println!(
        "  \x1b[1;36m budi stats\x1b[0m — \x1b[1m{}\x1b[0m",
        period_label
    );
    println!("  \x1b[90m{}\x1b[0m", "─".repeat(40));

    // Per-agent breakdown
    println!("  \x1b[1mAgents\x1b[0m");
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
            "    \x1b[36m{:<14}\x1b[0m {:>5} msgs  {}  \x1b[33m{}\x1b[0m",
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
        "  \x1b[1mTotal\x1b[0m        {} messages",
        summary.total_messages
    );

    let total_input = summary.total_input_tokens
        + summary.total_cache_creation_tokens
        + summary.total_cache_read_tokens;
    println!(
        "  \x1b[1mTokens\x1b[0m       {} in, {} out",
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

    println!();
    println!(
        "  \x1b[1;36m Repositories\x1b[0m — \x1b[1m{}\x1b[0m",
        period_label
    );
    println!("  \x1b[90m{}\x1b[0m", "─".repeat(50));

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
            "    \x1b[1m{:<28}\x1b[0m \x1b[33m{:>8}\x1b[0m  \x1b[36m{}\x1b[0m",
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
    println!();
    println!(
        "  \x1b[1;36m Branches\x1b[0m — \x1b[1m{}\x1b[0m",
        period_label
    );
    println!("  \x1b[90m{}\x1b[0m", "─".repeat(50));

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
            "    \x1b[1m{:<28}\x1b[0m \x1b[33m{:>8}\x1b[0m  \x1b[90m{}\x1b[0m  \x1b[36m{}\x1b[0m",
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

    println!();
    println!(
        "  \x1b[1;36m Branch\x1b[0m \x1b[1m{}\x1b[0m — \x1b[90m{}\x1b[0m",
        branch, period_label
    );
    println!("  \x1b[90m{}\x1b[0m", "─".repeat(40));

    match result {
        Some(b) => {
            if !b.repo_id.is_empty() {
                println!("  \x1b[1mRepo\x1b[0m       {}", b.repo_id);
            }
            println!("  \x1b[1mSessions\x1b[0m   {}", b.session_count);
            println!("  \x1b[1mMessages\x1b[0m   {}", b.message_count);
            let total_input = b.input_tokens + b.cache_creation_tokens + b.cache_read_tokens;
            println!("  \x1b[1mInput\x1b[0m      {}", format_tokens(total_input));
            println!(
                "  \x1b[1mOutput\x1b[0m     {}",
                format_tokens(b.output_tokens)
            );
            println!(
                "  \x1b[1mEst. cost\x1b[0m  \x1b[33m{}\x1b[0m",
                format_cost_cents(b.cost_cents)
            );
        }
        None => {
            println!("  No data found for branch '{}'.", branch);
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
    println!();
    println!(
        "  \x1b[1;36m🤖 Model usage\x1b[0m — \x1b[1m{}\x1b[0m",
        period_label
    );
    println!("  \x1b[90m{}\x1b[0m", "─".repeat(50));

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
            "    \x1b[1m{:<30}\x1b[0m {:>5} msgs  {:>8} tok  \x1b[36m{}\x1b[0m",
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

    // Parse "key=value" or just "key"
    let (tag_key, _tag_value) = if let Some(eq) = tag_filter.find('=') {
        (Some(&tag_filter[..eq]), Some(&tag_filter[eq + 1..]))
    } else {
        (Some(tag_filter), None)
    };

    let data = client.tags(tag_key, since.as_deref(), until.as_deref(), 30)?;

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

    println!(
        "\n\x1b[1m  Tag: {} — {}\x1b[0m\n",
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
            "  {:<40} \x1b[33m{}\x1b[0m{} {:>8}",
            tag.value,
            bar,
            pad_bar,
            format_cost_cents(tag.cost_cents),
        );
    }
    println!();
    Ok(())
}

