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
    tickets: bool,
    ticket: Option<String>,
    activities: bool,
    activity: Option<String>,
    files: bool,
    file: Option<String>,
    repo: Option<String>,
    models: bool,
    provider: Option<String>,
    tag: Option<String>,
    json_output: bool,
) -> Result<()> {
    // Normalize and validate --provider early with a helpful error message.
    // Canonical names match the `provider` column in SQLite; aliases are
    // user-friendly shortcuts that resolve to their canonical form.
    let provider = provider.map(|p| normalize_provider(&p)).transpose()?;

    // `--repo` is a filter for `--branch`, `--ticket`, `--activity`, or
    // `--file` — surface the misuse early instead of silently ignoring it.
    // Clap's `requires` only takes a single arg, so the cross-flag check
    // lives here.
    if repo.is_some()
        && branch.is_none()
        && ticket.is_none()
        && activity.is_none()
        && file.is_none()
    {
        anyhow::bail!(
            "--repo requires --branch <NAME>, --ticket <ID>, --activity <NAME>, or --file <PATH> to scope the filter"
        );
    }

    // `--file <PATH>` must be a repo-relative forward-slashed path. The
    // pipeline never stores absolute / traversal paths (see ADR-0083 and
    // `file_attribution::normalize_one`), so validate early with a clear
    // error rather than silently returning "file not found".
    if let Some(ref f) = file {
        validate_file_path_arg(f)?;
    }

    let client = DaemonClient::connect().context(
        "Could not reach budi daemon. Run `budi init` to set up, or `budi doctor` to diagnose.",
    )?;

    if let Some(ref tag_filter) = tag {
        return cmd_stats_tags(&client, period, tag_filter, json_output);
    }

    if let Some(ref f) = file {
        return cmd_stats_file_detail(&client, period, f, repo.as_deref(), json_output);
    }

    if files {
        return cmd_stats_files(&client, period, json_output);
    }

    if let Some(ref ac) = activity {
        return cmd_stats_activity_detail(&client, period, ac, repo.as_deref(), json_output);
    }

    if activities {
        return cmd_stats_activities(&client, period, json_output);
    }

    if let Some(ref tk) = ticket {
        return cmd_stats_ticket_detail(&client, period, tk, repo.as_deref(), json_output);
    }

    if tickets {
        return cmd_stats_tickets(&client, period, json_output);
    }

    if let Some(ref br) = branch {
        return cmd_stats_branch_detail(&client, period, br, repo.as_deref(), json_output);
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
            map.insert("total_cost".to_string(), serde_json::json!(cost.total_cost));
            map.insert("input_cost".to_string(), serde_json::json!(cost.input_cost));
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
    let green = ansi("\x1b[32m");
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

    let est = client.cost(since.as_deref(), until.as_deref(), None)?;
    println!();
    println!(
        "  {bold}Est. cost{reset}    {yellow}{}{reset}",
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
    repo: Option<&str>,
    json_output: bool,
) -> Result<()> {
    let (since, until) = period_date_range(period);
    let result = client.branch_detail(branch, repo, since.as_deref(), until.as_deref())?;

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
    if let Some(repo_id) = repo {
        println!("  {bold}Repo filter{reset} {}", repo_id);
    }
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
            println!("  Tip: run `budi import` first if you haven't imported data yet.");
            println!("  Run `budi stats --branches` to see available branches.");
        }
    }

    println!();
    Ok(())
}

/// `--tickets` view: tickets ranked by cost. Mirrors `cmd_stats_branches`.
///
/// The list always carries an `(untagged)` row so users can see how much
/// activity is *not* attributed to a ticket — that bucket should shrink as
/// teams adopt ticket-bearing branch names.
fn cmd_stats_tickets(client: &DaemonClient, period: StatsPeriod, json_output: bool) -> Result<()> {
    let (since, until) = period_date_range(period);
    let tickets = client.tickets(since.as_deref(), until.as_deref(), 30)?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&tickets)?);
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
        "  {bold_cyan} Tickets{reset} — {bold}{}{reset}",
        period_label
    );
    println!("  {dim}{}{reset}", "─".repeat(60));

    if tickets.is_empty() {
        println!("  No ticket data for this period.");
        println!("  Tip: branch names need to contain a ticket id (e.g. PAVA-123).");
        println!();
        return Ok(());
    }

    let max_cost = tickets
        .iter()
        .map(|t| t.cost_cents)
        .fold(0.0_f64, f64::max)
        .max(0.01);
    for t in &tickets {
        let bar_len = ((t.cost_cents / max_cost) * 16.0) as usize;
        let bar: String = "\u{2588}".repeat(bar_len);
        let branch_label = if t.top_branch.is_empty() {
            "--".to_string()
        } else {
            t.top_branch.clone()
        };
        let source_label = if t.source.is_empty() {
            "--".to_string()
        } else {
            t.source.clone()
        };
        println!(
            "    {bold}{:<24}{reset} {yellow}{:>8}{reset}  {dim}src={:<15}{reset}  {dim}{:<24}{reset}  {cyan}{}{reset}",
            t.ticket_id,
            format_cost_cents(t.cost_cents),
            source_label,
            branch_label,
            bar
        );
    }

    println!();
    Ok(())
}

/// `--ticket <ID>` detail view. Mirrors `cmd_stats_branch_detail`, plus a
/// per-branch breakdown so the user can see which branches charged cost to
/// the ticket (handy for stacked PRs / multi-branch work).
fn cmd_stats_ticket_detail(
    client: &DaemonClient,
    period: StatsPeriod,
    ticket: &str,
    repo: Option<&str>,
    json_output: bool,
) -> Result<()> {
    let (since, until) = period_date_range(period);
    let result = client.ticket_detail(ticket, repo, since.as_deref(), until.as_deref())?;

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
        "  {bold_cyan} Ticket{reset} {bold}{}{reset} — {dim}{}{reset}",
        ticket, period_label
    );
    if let Some(repo_id) = repo {
        println!("  {bold}Repo filter{reset} {}", repo_id);
    }
    println!("  {dim}{}{reset}", "─".repeat(50));

    match result {
        Some(t) => {
            if !t.repo_id.is_empty() {
                println!("  {bold}Repo{reset}       {}", t.repo_id);
            }
            if !t.ticket_prefix.is_empty() {
                println!("  {bold}Prefix{reset}     {}", t.ticket_prefix);
            }
            if !t.source.is_empty() {
                println!("  {bold}Source{reset}     {}", t.source);
            }
            println!("  {bold}Sessions{reset}   {}", t.session_count);
            println!("  {bold}Messages{reset}   {}", t.message_count);
            println!(
                "  {bold}Input{reset}      {}",
                format_tokens(t.input_tokens)
            );
            println!(
                "  {bold}Output{reset}     {}",
                format_tokens(t.output_tokens)
            );
            println!(
                "  {bold}Est. cost{reset}  {yellow}{}{reset}",
                format_cost_cents(t.cost_cents)
            );

            if !t.branches.is_empty() {
                println!();
                println!("  {bold}Branches{reset}");
                for br in &t.branches {
                    let repo_label = if br.repo_id.is_empty() {
                        "--".to_string()
                    } else {
                        br.repo_id
                            .rsplit('/')
                            .next()
                            .unwrap_or(&br.repo_id)
                            .to_string()
                    };
                    println!(
                        "    {bold}{:<28}{reset} {yellow}{:>8}{reset}  {dim}{}{reset}",
                        br.git_branch,
                        format_cost_cents(br.cost_cents),
                        repo_label
                    );
                }
            }
        }
        None => {
            println!("  No data found for ticket '{}'.", ticket);
            println!("  Tip: run `budi import` first if you haven't imported data yet.");
            println!("  Run `budi stats --tickets` to see available tickets.");
        }
    }

    println!();
    Ok(())
}

/// `--activities` list view. Mirrors `cmd_stats_tickets`: activities come
/// from the `activity` tag emitted by the prompt classifier
/// (`hooks::classify_prompt`) and propagated across the session by the
/// pipeline. The output always carries an `(untagged)` row so users can see
/// how much work isn't yet classified — that bucket should shrink as the
/// classifier improves in R1.2 (#222).
fn cmd_stats_activities(
    client: &DaemonClient,
    period: StatsPeriod,
    json_output: bool,
) -> Result<()> {
    let (since, until) = period_date_range(period);
    let activities = client.activities(since.as_deref(), until.as_deref(), 30)?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&activities)?);
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
        "  {bold_cyan} Activities{reset} — {bold}{}{reset}",
        period_label
    );
    println!("  {dim}{}{reset}", "─".repeat(66));

    if activities.is_empty() {
        println!("  No activity data for this period.");
        println!(
            "  Tip: activity is classified from the user's prompt; run `budi doctor` to check the signal."
        );
        println!();
        return Ok(());
    }

    let max_cost = activities
        .iter()
        .map(|a| a.cost_cents)
        .fold(0.0_f64, f64::max)
        .max(0.01);
    for a in &activities {
        let bar_len = ((a.cost_cents / max_cost) * 16.0) as usize;
        let bar: String = "\u{2588}".repeat(bar_len);
        let branch_label = if a.top_branch.is_empty() {
            "--".to_string()
        } else {
            a.top_branch.clone()
        };
        let confidence_label = if a.confidence.is_empty() {
            "--".to_string()
        } else {
            a.confidence.clone()
        };
        println!(
            "    {bold}{:<18}{reset} {yellow}{:>8}{reset}  {dim}conf={:<6}{reset}  {dim}{:<22}{reset}  {cyan}{}{reset}",
            a.activity,
            format_cost_cents(a.cost_cents),
            confidence_label,
            branch_label,
            bar
        );
    }

    println!();
    Ok(())
}

/// `--activity <NAME>` detail view. Mirrors `cmd_stats_ticket_detail`, plus
/// the classification `source`/`confidence` contract so the user can tell
/// at a glance whether the label comes from rules (R1.0) or a richer
/// classifier landing in R1.2 (#222).
fn cmd_stats_activity_detail(
    client: &DaemonClient,
    period: StatsPeriod,
    activity: &str,
    repo: Option<&str>,
    json_output: bool,
) -> Result<()> {
    let (since, until) = period_date_range(period);
    let result = client.activity_detail(activity, repo, since.as_deref(), until.as_deref())?;

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
        "  {bold_cyan} Activity{reset} {bold}{}{reset} — {dim}{}{reset}",
        activity, period_label
    );
    if let Some(repo_id) = repo {
        println!("  {bold}Repo filter{reset} {}", repo_id);
    }
    println!("  {dim}{}{reset}", "─".repeat(50));

    match result {
        Some(a) => {
            if !a.repo_id.is_empty() {
                println!("  {bold}Repo{reset}       {}", a.repo_id);
            }
            if !a.source.is_empty() {
                println!(
                    "  {bold}Source{reset}     {} {dim}(confidence: {}){reset}",
                    a.source,
                    if a.confidence.is_empty() {
                        "--"
                    } else {
                        &a.confidence
                    }
                );
            }
            println!("  {bold}Sessions{reset}   {}", a.session_count);
            println!("  {bold}Messages{reset}   {}", a.message_count);
            println!(
                "  {bold}Input{reset}      {}",
                format_tokens(a.input_tokens)
            );
            println!(
                "  {bold}Output{reset}     {}",
                format_tokens(a.output_tokens)
            );
            println!(
                "  {bold}Est. cost{reset}  {yellow}{}{reset}",
                format_cost_cents(a.cost_cents)
            );

            if !a.branches.is_empty() {
                println!();
                println!("  {bold}Branches{reset}");
                for br in &a.branches {
                    let repo_label = if br.repo_id.is_empty() {
                        "--".to_string()
                    } else {
                        br.repo_id
                            .rsplit('/')
                            .next()
                            .unwrap_or(&br.repo_id)
                            .to_string()
                    };
                    println!(
                        "    {bold}{:<28}{reset} {yellow}{:>8}{reset}  {dim}{}{reset}",
                        br.git_branch,
                        format_cost_cents(br.cost_cents),
                        repo_label
                    );
                }
            }
        }
        None => {
            println!("  No data found for activity '{}'.", activity);
            println!("  Tip: run `budi import` first if you haven't imported data yet.");
            println!("  Run `budi stats --activities` to see available activities.");
        }
    }

    println!();
    Ok(())
}

/// Validate a `--file <PATH>` argument. Rejects absolute paths, paths
/// with `..` traversal, Windows separators, and URL schemes. The
/// pipeline would never emit such a value as a `file_path` tag
/// (`file_attribution::normalize_one` strips them), so surfacing a clear
/// error here is more useful than a silent 404 from the daemon.
fn validate_file_path_arg(path: &str) -> Result<()> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        anyhow::bail!("--file path must not be empty");
    }
    if trimmed.starts_with('/') {
        anyhow::bail!(
            "--file must be a repo-relative path (no leading '/'); try a path like src/main.rs"
        );
    }
    if trimmed.contains('\\') {
        anyhow::bail!("--file path uses forward slashes only, even on Windows");
    }
    if trimmed.contains("..") {
        anyhow::bail!("--file path must not contain '..' (repo-relative only)");
    }
    if trimmed.contains("://") {
        anyhow::bail!("--file path must not contain a URL scheme");
    }
    Ok(())
}

/// `--files` list view. Mirrors `cmd_stats_tickets` / `cmd_stats_activities`:
/// files come from the `file_path` tag emitted by `FileEnricher` when an
/// assistant message's tool-call arguments point inside the repo root. The
/// output always carries an `(untagged)` row so users can see how much
/// activity isn't attributed to a file — that bucket should shrink as
/// tool-arg coverage improves. Added in R1.4 (#292).
fn cmd_stats_files(client: &DaemonClient, period: StatsPeriod, json_output: bool) -> Result<()> {
    let (since, until) = period_date_range(period);
    let files = client.files(since.as_deref(), until.as_deref(), 30)?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&files)?);
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
    println!("  {bold_cyan} Files{reset} — {bold}{}{reset}", period_label);
    println!("  {dim}{}{reset}", "─".repeat(72));

    if files.is_empty() {
        println!("  No file data for this period.");
        println!(
            "  Tip: file paths are extracted from tool-call arguments (Read/Write/Edit, etc)."
        );
        println!();
        return Ok(());
    }

    let max_cost = files
        .iter()
        .map(|f| f.cost_cents)
        .fold(0.0_f64, f64::max)
        .max(0.01);
    for f in &files {
        let bar_len = ((f.cost_cents / max_cost) * 16.0) as usize;
        let bar: String = "\u{2588}".repeat(bar_len);
        let ticket_label = if f.top_ticket_id.is_empty() {
            "--".to_string()
        } else {
            f.top_ticket_id.clone()
        };
        let source_label = if f.source.is_empty() {
            "--".to_string()
        } else {
            f.source.clone()
        };
        // Truncate very long paths so the row stays readable in narrow
        // terminals. Full paths remain visible via `--file <PATH>` and
        // `--format json`.
        let path_label = if f.file_path.chars().count() > 40 {
            let tail: String = f.file_path.chars().rev().take(37).collect();
            let tail: String = tail.chars().rev().collect();
            format!("…{tail}")
        } else {
            f.file_path.clone()
        };
        println!(
            "    {bold}{:<40}{reset} {yellow}{:>8}{reset}  {dim}src={:<12}{reset}  {dim}{:<14}{reset}  {cyan}{}{reset}",
            path_label,
            format_cost_cents(f.cost_cents),
            source_label,
            ticket_label,
            bar
        );
    }

    println!();
    Ok(())
}

/// `--file <PATH>` detail view. Mirrors `cmd_stats_ticket_detail`, plus a
/// per-ticket breakdown so users can see which tickets drove cost on a
/// particular file (complements the per-branch view).
fn cmd_stats_file_detail(
    client: &DaemonClient,
    period: StatsPeriod,
    file_path: &str,
    repo: Option<&str>,
    json_output: bool,
) -> Result<()> {
    let (since, until) = period_date_range(period);
    let result = client.file_detail(file_path, repo, since.as_deref(), until.as_deref())?;

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
        "  {bold_cyan} File{reset} {bold}{}{reset} — {dim}{}{reset}",
        file_path, period_label
    );
    if let Some(repo_id) = repo {
        println!("  {bold}Repo filter{reset} {}", repo_id);
    }
    println!("  {dim}{}{reset}", "─".repeat(50));

    match result {
        Some(f) => {
            if !f.repo_id.is_empty() {
                println!("  {bold}Repo{reset}       {}", f.repo_id);
            }
            if !f.source.is_empty() {
                println!(
                    "  {bold}Source{reset}     {} {dim}(confidence: {}){reset}",
                    f.source,
                    if f.confidence.is_empty() {
                        "--"
                    } else {
                        &f.confidence
                    }
                );
            }
            println!("  {bold}Sessions{reset}   {}", f.session_count);
            println!("  {bold}Messages{reset}   {}", f.message_count);
            println!(
                "  {bold}Input{reset}      {}",
                format_tokens(f.input_tokens)
            );
            println!(
                "  {bold}Output{reset}     {}",
                format_tokens(f.output_tokens)
            );
            println!(
                "  {bold}Est. cost{reset}  {yellow}{}{reset}",
                format_cost_cents(f.cost_cents)
            );

            if !f.branches.is_empty() {
                println!();
                println!("  {bold}Branches{reset}");
                for br in &f.branches {
                    let repo_label = if br.repo_id.is_empty() {
                        "--".to_string()
                    } else {
                        br.repo_id
                            .rsplit('/')
                            .next()
                            .unwrap_or(&br.repo_id)
                            .to_string()
                    };
                    println!(
                        "    {bold}{:<28}{reset} {yellow}{:>8}{reset}  {dim}{}{reset}",
                        br.git_branch,
                        format_cost_cents(br.cost_cents),
                        repo_label
                    );
                }
            }

            if !f.tickets.is_empty() {
                println!();
                println!("  {bold}Tickets{reset}");
                for tk in &f.tickets {
                    println!(
                        "    {bold}{:<28}{reset} {yellow}{:>8}{reset}  {dim}{} msgs{reset}",
                        tk.ticket_id,
                        format_cost_cents(tk.cost_cents),
                        tk.message_count
                    );
                }
            }
        }
        None => {
            println!("  No data found for file '{}'.", file_path);
            println!("  Tip: run `budi import` first if you haven't imported data yet.");
            println!("  Run `budi stats --files` to see available files.");
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

    let has_duplicate_models = {
        let mut seen = std::collections::HashSet::new();
        models.iter().any(|m| !seen.insert(&m.model))
    };

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
        let label = if has_duplicate_models {
            format!("{} ({})", m.model, m.provider)
        } else {
            m.model.clone()
        };
        println!(
            "    {bold}{:<40}{reset} {:>5} msgs  {:>8} tok  {yellow}{:>8}{reset}  {cyan}{}{reset}",
            label,
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

/// Resolve a user-supplied provider name to its canonical DB value.
///
/// Canonical names: `claude_code`, `cursor`, `codex`, `copilot_cli`, `openai`.
/// Accepted aliases: `copilot` → `copilot_cli`, `anthropic` → `claude_code`.
fn normalize_provider(input: &str) -> Result<String> {
    const KNOWN_PROVIDERS: &[&str] = &["claude_code", "cursor", "codex", "copilot_cli", "openai"];

    if KNOWN_PROVIDERS.contains(&input) {
        return Ok(input.to_string());
    }

    match input {
        "copilot" => Ok("copilot_cli".to_string()),
        "anthropic" => Ok("claude_code".to_string()),
        _ => {
            let all: Vec<&str> = KNOWN_PROVIDERS
                .iter()
                .copied()
                .chain(["copilot", "anthropic"])
                .collect();
            anyhow::bail!(
                "Unknown provider '{}'. Available providers: {}",
                input,
                all.join(", ")
            );
        }
    }
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
