use anyhow::{Context, Result};
use budi_core::analytics::{self, BreakdownPage, BreakdownRowCost};
use chrono::{Local, Months, NaiveDate, TimeZone};

use crate::StatsPeriod;
use crate::client::DaemonClient;

use super::ansi;

// ─── Shared Breakdown Rendering (#449) ───────────────────────────────────────
//
// Every `budi stats` breakdown view (`--projects / --branches / --tickets /
// --activities / --files / --models / --tag`) ships through the same bar
// renderer, header shape, and footer so the visual signal never drifts. The
// contract, nailed down by #449:
//
//   1. Bar length is proportional to the row's cost (the column the bar sits
//      next to), normalized against the max cost in the visible rows.
//   2. Zero-cost rows render a blank bar slot, never a tiny sliver.
//   3. Every view prints a header row so `src=`, `conf=`, dominant-branch,
//      etc. have in-place labels.
//   4. The bar column sits immediately before the cost column — the eye
//      scans bar → cost without crossing any other field.

/// Width of the bar column, in monospace cells, for every breakdown view.
/// Chosen to stay readable at 80-column terminals while leaving room for
/// the per-view extra columns.
const BREAKDOWN_BAR_WIDTH: usize = 20;

/// Cost column width for the right-aligned `$X` cell printed next to the bar.
const BREAKDOWN_COST_WIDTH: usize = 10;

/// Render a bar cell of exactly [`BREAKDOWN_BAR_WIDTH`] characters wide.
///
/// Returns a string of blocks + right-padding so adjacent columns always
/// align. Zero-cost rows produce an all-spaces cell — the #449 "bar on $0
/// row" regression class. Rows with non-zero cost always render at least
/// one block so the reader can distinguish "tiny but real" from "exactly
/// $0".
fn render_bar(cost_cents: f64, max_cost: f64) -> String {
    if cost_cents <= 0.0 || max_cost <= 0.0 {
        return " ".repeat(BREAKDOWN_BAR_WIDTH);
    }
    let ratio = (cost_cents / max_cost).clamp(0.0, 1.0);
    let raw_len = (ratio * BREAKDOWN_BAR_WIDTH as f64).round() as usize;
    let bar_len = raw_len.clamp(1, BREAKDOWN_BAR_WIDTH);
    let mut out = String::with_capacity(BREAKDOWN_BAR_WIDTH * 3);
    for _ in 0..bar_len {
        out.push('\u{2588}');
    }
    for _ in 0..(BREAKDOWN_BAR_WIDTH - bar_len) {
        out.push(' ');
    }
    out
}

/// Max cost across the visible rows on a breakdown page, used to normalize
/// the bar length. Zero when every row is $0 (in which case `render_bar`
/// returns blank cells for every row).
fn max_cost_for_rows<T: BreakdownRowCost>(rows: &[T]) -> f64 {
    rows.iter()
        .map(BreakdownRowCost::cost_cents)
        .fold(0.0_f64, f64::max)
}

/// Truncate a display label to at most `max_chars` characters, prefixing
/// the kept suffix with `…` when truncation happens. Operates on Unicode
/// scalars (not bytes) to stay safe on multi-byte file paths / ticket
/// ids — see #389 / #383 / #404 / #445 for the precedent bug class.
fn truncate_label(label: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let char_count = label.chars().count();
    if char_count <= max_chars {
        return label.to_string();
    }
    let tail_len = max_chars.saturating_sub(1);
    let skip = char_count - tail_len;
    let mut out = String::with_capacity(max_chars * 4);
    out.push('…');
    out.extend(label.chars().skip(skip));
    out
}

/// Format the shared `LABEL   [bar gap]   COST   EXTRAS` header row that
/// precedes every breakdown table. The bar column itself has no header —
/// it's a scale cue, not a data column — but the `COST` title sits at the
/// right edge of the bar+cost combined width so cost values line up
/// under it.
///
/// Returns a plain-text (ANSI-free) string so tests can snapshot the
/// layout deterministically. ANSI wrapping is applied by the caller at
/// print time.
fn format_breakdown_header_text(
    label_header: &str,
    label_width: usize,
    extra_header: &str,
) -> String {
    // Bar + single space gap + right-aligned COST column width. The `+1`
    // accounts for the blank column between the bar and the cost cell in
    // the row formatter.
    let cost_header_width = BREAKDOWN_BAR_WIDTH + 1 + BREAKDOWN_COST_WIDTH;
    if extra_header.is_empty() {
        format!(
            "  {:<label_w$} {:>cost_w$}",
            label_header,
            "COST",
            label_w = label_width,
            cost_w = cost_header_width,
        )
    } else {
        format!(
            "  {:<label_w$} {:>cost_w$}  {}",
            label_header,
            "COST",
            extra_header,
            label_w = label_width,
            cost_w = cost_header_width,
        )
    }
}

/// Print the shared breakdown header to stdout, wrapped in the `dim`
/// ANSI cue. See [`format_breakdown_header_text`] for the underlying
/// layout.
fn print_breakdown_header(label_header: &str, label_width: usize, extra_header: &str) {
    let dim = ansi("\x1b[90m");
    let reset = ansi("\x1b[0m");
    let text = format_breakdown_header_text(label_header, label_width, extra_header);
    println!("{dim}{}{reset}", text);
}

/// Format one breakdown row as plain text (no ANSI) in the canonical
/// shared layout: `  LABEL  BAR  COST  EXTRAS`.
///
/// Kept alongside the production view code so snapshot tests can anchor
/// the layout without simulating terminal ANSI state. Production views
/// print the same layout with per-field colors applied; if they drift
/// apart, the rendering helpers ([`render_bar`], [`BREAKDOWN_BAR_WIDTH`],
/// [`BREAKDOWN_COST_WIDTH`]) are the single source of truth for widths
/// and bar scaling.
#[cfg(test)]
fn format_breakdown_row_text(
    label: &str,
    label_width: usize,
    cost_cents: f64,
    max_cost: f64,
    extra: &str,
) -> String {
    let bar = render_bar(cost_cents, max_cost);
    let cost_cell = format_cost_cents(cost_cents);
    if extra.is_empty() {
        format!(
            "  {:<label_w$} {} {:>cost_w$}",
            label,
            bar,
            cost_cell,
            label_w = label_width,
            cost_w = BREAKDOWN_COST_WIDTH,
        )
    } else {
        format!(
            "  {:<label_w$} {} {:>cost_w$}  {}",
            label,
            bar,
            cost_cell,
            extra,
            label_w = label_width,
            cost_w = BREAKDOWN_COST_WIDTH,
        )
    }
}

pub fn period_label(period: StatsPeriod) -> String {
    match period {
        StatsPeriod::Today => "Today".to_string(),
        StatsPeriod::Week => "Last 7 days".to_string(),
        StatsPeriod::Month => "Last 30 days".to_string(),
        StatsPeriod::All => "All time".to_string(),
        StatsPeriod::Days(1) => "Last 1 day".to_string(),
        StatsPeriod::Days(n) => format!("Last {} days", n),
        StatsPeriod::Weeks(1) => "Last 1 week".to_string(),
        StatsPeriod::Weeks(n) => format!("Last {} weeks", n),
        StatsPeriod::Months(1) => "Last 1 month".to_string(),
        StatsPeriod::Months(n) => format!("Last {} months", n),
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

/// Resolve the `since` anchor (as a local calendar date at midnight) for a
/// given period, relative to `today`. Extracted from `period_date_range` so
/// tests can parameterize `today` without mocking `Local::now()`.
///
/// Returns `None` when the period has no lower bound (e.g. `All`).
///
/// `Week` resolves to `today − 7 days` (same as `Days(7)` / `Weeks(1)`), and
/// `Month` resolves to `today − 30 days` (same as `Days(30)`). This is the
/// rolling semantic documented in README §"Windows: rolling vs calendar":
/// "week / month = the last 7 / 30 calendar days including today." The
/// previous implementation resolved `Week` to the Monday-of-this-week and
/// `Month` to the first-of-this-month, which collapsed to one day of data on
/// Mondays and on the first of the month (#447).
pub(crate) fn period_since_date(today: NaiveDate, period: StatsPeriod) -> Option<NaiveDate> {
    match period {
        StatsPeriod::Today => Some(today),
        StatsPeriod::Week => Some(today - chrono::Duration::days(7)),
        StatsPeriod::Month => Some(today - chrono::Duration::days(30)),
        StatsPeriod::All => None,
        StatsPeriod::Days(n) => Some(today - chrono::Duration::days(n as i64)),
        StatsPeriod::Weeks(n) => Some(today - chrono::Duration::weeks(n as i64)),
        StatsPeriod::Months(n) => {
            // Use calendar months (chrono clamps to the end of the
            // target month if the current day-of-month doesn't exist
            // there, e.g. 2026-03-31 - 1 month = 2026-02-28). Falls
            // back to a 30-day-per-month approximation only for the
            // unreachable overflow case so we never panic on the
            // `--period` axis.
            let past = today
                .checked_sub_months(Months::new(n))
                .unwrap_or_else(|| today - chrono::Duration::days((n as i64) * 30));
            Some(past)
        }
    }
}

pub fn period_date_range(period: StatsPeriod) -> (Option<String>, Option<String>) {
    let today = Local::now().date_naive();
    let since = period_since_date(today, period).map(local_midnight_to_utc);
    (since, None)
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
    limit: usize,
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
        return cmd_stats_tags(&client, period, tag_filter, limit, json_output);
    }

    if let Some(ref f) = file {
        return cmd_stats_file_detail(&client, period, f, repo.as_deref(), json_output);
    }

    if files {
        return cmd_stats_files(&client, period, limit, json_output);
    }

    if let Some(ref ac) = activity {
        return cmd_stats_activity_detail(&client, period, ac, repo.as_deref(), json_output);
    }

    if activities {
        return cmd_stats_activities(&client, period, limit, json_output);
    }

    if let Some(ref tk) = ticket {
        return cmd_stats_ticket_detail(&client, period, tk, repo.as_deref(), json_output);
    }

    if tickets {
        return cmd_stats_tickets(&client, period, limit, json_output);
    }

    if let Some(ref br) = branch {
        return cmd_stats_branch_detail(&client, period, br, repo.as_deref(), json_output);
    }

    if branches {
        return cmd_stats_branches(&client, period, limit, json_output);
    }

    if models {
        return cmd_stats_models(&client, period, limit, json_output);
    }

    if projects {
        return cmd_stats_projects(&client, period, limit, json_output);
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
            // Expose the resolved window so scripting users can verify
            // which period `--period today|week|month|Nd|Nw|Nm|all`
            // actually mapped to (#447 acceptance: `budi stats -p week`
            // and `-p 7d` must resolve to the same window on every
            // weekday).
            map.insert("window_start".to_string(), serde_json::json!(since));
            map.insert("window_end".to_string(), serde_json::json!(until));
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

    if provider == Some("cursor") {
        println!("  {dim}* {}{reset}", budi_core::analytics::CURSOR_LAG_HINT);
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

    if providers.iter().any(|p| p.provider == "cursor") {
        println!("  {dim}* {}{reset}", budi_core::analytics::CURSOR_LAG_HINT);
    }

    println!();
    Ok(())
}

fn cmd_stats_projects(
    client: &DaemonClient,
    period: StatsPeriod,
    limit: usize,
    json_output: bool,
) -> Result<()> {
    let (since, until) = period_date_range(period);
    let page = client.projects(since.as_deref(), until.as_deref(), limit)?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&page)?);
        return Ok(());
    }

    let period_label = period_label(period);

    let bold_cyan = ansi("\x1b[1;36m");
    let bold = ansi("\x1b[1m");
    let dim = ansi("\x1b[90m");
    let cyan = ansi("\x1b[36m");
    let yellow = ansi("\x1b[33m");
    let reset = ansi("\x1b[0m");

    const LABEL_WIDTH: usize = 28;
    let rule_width = LABEL_WIDTH + 1 + BREAKDOWN_BAR_WIDTH + 1 + BREAKDOWN_COST_WIDTH;
    println!();
    println!(
        "  {bold_cyan} Repositories{reset} — {bold}{}{reset}",
        period_label
    );
    println!("  {dim}{}{reset}", "─".repeat(rule_width));

    if page.rows.is_empty() && page.other.is_none() {
        println!("  No data for this period.");
        println!();
        return Ok(());
    }

    print_breakdown_header("REPOSITORY", LABEL_WIDTH, "");

    let max_cost = max_cost_for_rows(&page.rows);
    for r in &page.rows {
        let bar = render_bar(r.cost_cents, max_cost);
        println!(
            "  {bold}{:<LABEL_WIDTH$}{reset} {cyan}{}{reset} {yellow}{:>cost_w$}{reset}",
            r.repo_id,
            bar,
            format_cost_cents(r.cost_cents),
            cost_w = BREAKDOWN_COST_WIDTH,
        );
    }

    render_breakdown_footer(&page, LABEL_WIDTH, rule_width);
    Ok(())
}

fn cmd_stats_branches(
    client: &DaemonClient,
    period: StatsPeriod,
    limit: usize,
    json_output: bool,
) -> Result<()> {
    let (since, until) = period_date_range(period);
    let page = client.branches(since.as_deref(), until.as_deref(), limit)?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&page)?);
        return Ok(());
    }

    let period_label = period_label(period);

    let bold_cyan = ansi("\x1b[1;36m");
    let bold = ansi("\x1b[1m");
    let dim = ansi("\x1b[90m");
    let cyan = ansi("\x1b[36m");
    let yellow = ansi("\x1b[33m");
    let reset = ansi("\x1b[0m");

    const LABEL_WIDTH: usize = 28;
    const REPO_WIDTH: usize = 16;
    let rule_width =
        LABEL_WIDTH + 1 + BREAKDOWN_BAR_WIDTH + 1 + BREAKDOWN_COST_WIDTH + 2 + REPO_WIDTH;
    println!();
    println!(
        "  {bold_cyan} Branches{reset} — {bold}{}{reset}",
        period_label
    );
    println!("  {dim}{}{reset}", "─".repeat(rule_width));

    if page.rows.is_empty() && page.other.is_none() {
        println!("  No branch data for this period.");
        println!();
        return Ok(());
    }

    print_breakdown_header("BRANCH", LABEL_WIDTH, &format!("{:<REPO_WIDTH$}", "REPO"));

    let max_cost = max_cost_for_rows(&page.rows);
    for b in &page.rows {
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
        let bar = render_bar(b.cost_cents, max_cost);
        println!(
            "  {bold}{:<LABEL_WIDTH$}{reset} {cyan}{}{reset} {yellow}{:>cost_w$}{reset}  {dim}{:<REPO_WIDTH$}{reset}",
            branch_name,
            bar,
            format_cost_cents(b.cost_cents),
            repo,
            cost_w = BREAKDOWN_COST_WIDTH,
        );
    }

    render_breakdown_footer(&page, LABEL_WIDTH, rule_width);
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
            println!("  Tip: run `budi db import` first if you haven't imported data yet.");
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
fn cmd_stats_tickets(
    client: &DaemonClient,
    period: StatsPeriod,
    limit: usize,
    json_output: bool,
) -> Result<()> {
    let (since, until) = period_date_range(period);
    let page = client.tickets(since.as_deref(), until.as_deref(), limit)?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&page)?);
        return Ok(());
    }

    let period_label = period_label(period);

    let bold_cyan = ansi("\x1b[1;36m");
    let bold = ansi("\x1b[1m");
    let dim = ansi("\x1b[90m");
    let cyan = ansi("\x1b[36m");
    let yellow = ansi("\x1b[33m");
    let reset = ansi("\x1b[0m");

    const LABEL_WIDTH: usize = 24;
    const SOURCE_WIDTH: usize = 18;
    const BRANCH_WIDTH: usize = 24;
    let rule_width = LABEL_WIDTH
        + 1
        + BREAKDOWN_BAR_WIDTH
        + 1
        + BREAKDOWN_COST_WIDTH
        + 2
        + SOURCE_WIDTH
        + 2
        + BRANCH_WIDTH;
    println!();
    println!(
        "  {bold_cyan} Tickets{reset} — {bold}{}{reset}",
        period_label
    );
    println!("  {dim}{}{reset}", "─".repeat(rule_width));

    if page.rows.is_empty() && page.other.is_none() {
        println!("  No ticket data for this period.");
        println!("  Tip: branch names need to contain a ticket id (e.g. PAVA-123).");
        println!();
        return Ok(());
    }

    print_breakdown_header(
        "TICKET",
        LABEL_WIDTH,
        &format!(
            "{:<SOURCE_WIDTH$}  {:<BRANCH_WIDTH$}",
            "SOURCE", "TOP_BRANCH"
        ),
    );

    let max_cost = max_cost_for_rows(&page.rows);
    for t in &page.rows {
        let bar = render_bar(t.cost_cents, max_cost);
        let branch_label = if t.top_branch.is_empty() {
            "--".to_string()
        } else {
            t.top_branch.clone()
        };
        let source_label = if t.source.is_empty() {
            "--".to_string()
        } else {
            format!("src={}", t.source)
        };
        println!(
            "  {bold}{:<LABEL_WIDTH$}{reset} {cyan}{}{reset} {yellow}{:>cost_w$}{reset}  {dim}{:<SOURCE_WIDTH$}{reset}  {dim}{:<BRANCH_WIDTH$}{reset}",
            t.ticket_id,
            bar,
            format_cost_cents(t.cost_cents),
            source_label,
            branch_label,
            cost_w = BREAKDOWN_COST_WIDTH,
        );
    }

    render_breakdown_footer(&page, LABEL_WIDTH, rule_width);
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
            println!("  Tip: run `budi db import` first if you haven't imported data yet.");
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
    limit: usize,
    json_output: bool,
) -> Result<()> {
    let (since, until) = period_date_range(period);
    let page = client.activities(since.as_deref(), until.as_deref(), limit)?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&page)?);
        return Ok(());
    }

    let period_label = period_label(period);

    let bold_cyan = ansi("\x1b[1;36m");
    let bold = ansi("\x1b[1m");
    let dim = ansi("\x1b[90m");
    let cyan = ansi("\x1b[36m");
    let yellow = ansi("\x1b[33m");
    let reset = ansi("\x1b[0m");

    const LABEL_WIDTH: usize = 18;
    const CONF_WIDTH: usize = 11;
    const BRANCH_WIDTH: usize = 22;
    let rule_width = LABEL_WIDTH
        + 1
        + BREAKDOWN_BAR_WIDTH
        + 1
        + BREAKDOWN_COST_WIDTH
        + 2
        + CONF_WIDTH
        + 2
        + BRANCH_WIDTH;
    println!();
    println!(
        "  {bold_cyan} Activities{reset} — {bold}{}{reset}",
        period_label
    );
    println!("  {dim}{}{reset}", "─".repeat(rule_width));

    if page.rows.is_empty() && page.other.is_none() {
        println!("  No activity data for this period.");
        println!(
            "  Tip: activity is classified from the user's prompt; run `budi doctor` to check the signal."
        );
        println!();
        return Ok(());
    }

    print_breakdown_header(
        "ACTIVITY",
        LABEL_WIDTH,
        &format!(
            "{:<CONF_WIDTH$}  {:<BRANCH_WIDTH$}",
            "CONFIDENCE", "TOP_BRANCH"
        ),
    );

    let max_cost = max_cost_for_rows(&page.rows);
    for a in &page.rows {
        let bar = render_bar(a.cost_cents, max_cost);
        let branch_label = if a.top_branch.is_empty() {
            "--".to_string()
        } else {
            a.top_branch.clone()
        };
        let confidence_label = if a.confidence.is_empty() {
            "--".to_string()
        } else {
            format!("conf={}", a.confidence)
        };
        println!(
            "  {bold}{:<LABEL_WIDTH$}{reset} {cyan}{}{reset} {yellow}{:>cost_w$}{reset}  {dim}{:<CONF_WIDTH$}{reset}  {dim}{:<BRANCH_WIDTH$}{reset}",
            a.activity,
            bar,
            format_cost_cents(a.cost_cents),
            confidence_label,
            branch_label,
            cost_w = BREAKDOWN_COST_WIDTH,
        );
    }

    render_breakdown_footer(&page, LABEL_WIDTH, rule_width);
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
            println!("  Tip: run `budi db import` first if you haven't imported data yet.");
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
fn cmd_stats_files(
    client: &DaemonClient,
    period: StatsPeriod,
    limit: usize,
    json_output: bool,
) -> Result<()> {
    let (since, until) = period_date_range(period);
    let page = client.files(since.as_deref(), until.as_deref(), limit)?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&page)?);
        return Ok(());
    }

    let period_label = period_label(period);

    let bold_cyan = ansi("\x1b[1;36m");
    let bold = ansi("\x1b[1m");
    let dim = ansi("\x1b[90m");
    let cyan = ansi("\x1b[36m");
    let yellow = ansi("\x1b[33m");
    let reset = ansi("\x1b[0m");

    const LABEL_WIDTH: usize = 40;
    const SOURCE_WIDTH: usize = 16;
    const TICKET_WIDTH: usize = 14;
    let rule_width = LABEL_WIDTH
        + 1
        + BREAKDOWN_BAR_WIDTH
        + 1
        + BREAKDOWN_COST_WIDTH
        + 2
        + SOURCE_WIDTH
        + 2
        + TICKET_WIDTH;
    println!();
    println!("  {bold_cyan} Files{reset} — {bold}{}{reset}", period_label);
    println!("  {dim}{}{reset}", "─".repeat(rule_width));

    if page.rows.is_empty() && page.other.is_none() {
        println!("  No file data for this period.");
        println!(
            "  Tip: file paths are extracted from tool-call arguments (Read/Write/Edit, etc)."
        );
        println!();
        return Ok(());
    }

    print_breakdown_header(
        "FILE",
        LABEL_WIDTH,
        &format!(
            "{:<SOURCE_WIDTH$}  {:<TICKET_WIDTH$}",
            "SOURCE", "TOP_TICKET"
        ),
    );

    let max_cost = max_cost_for_rows(&page.rows);
    for f in &page.rows {
        let bar = render_bar(f.cost_cents, max_cost);
        let ticket_label = if f.top_ticket_id.is_empty() {
            "--".to_string()
        } else {
            f.top_ticket_id.clone()
        };
        let source_label = if f.source.is_empty() {
            "--".to_string()
        } else {
            format!("src={}", f.source)
        };
        // Truncate very long paths so the row stays readable in narrow
        // terminals. Full paths remain visible via `--file <PATH>` and
        // `--format json`.
        let path_label = truncate_label(&f.file_path, LABEL_WIDTH);
        println!(
            "  {bold}{:<LABEL_WIDTH$}{reset} {cyan}{}{reset} {yellow}{:>cost_w$}{reset}  {dim}{:<SOURCE_WIDTH$}{reset}  {dim}{:<TICKET_WIDTH$}{reset}",
            path_label,
            bar,
            format_cost_cents(f.cost_cents),
            source_label,
            ticket_label,
            cost_w = BREAKDOWN_COST_WIDTH,
        );
    }

    render_breakdown_footer(&page, LABEL_WIDTH, rule_width);
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
            println!("  Tip: run `budi db import` first if you haven't imported data yet.");
            println!("  Run `budi stats --files` to see available files.");
        }
    }

    println!();
    Ok(())
}

fn cmd_stats_models(
    client: &DaemonClient,
    period: StatsPeriod,
    limit: usize,
    json_output: bool,
) -> Result<()> {
    let (since, until) = period_date_range(period);
    let page = client.models(since.as_deref(), until.as_deref(), limit)?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&page)?);
        return Ok(());
    }

    let period_label = period_label(period);

    let bold_cyan = ansi("\x1b[1;36m");
    let bold = ansi("\x1b[1m");
    let dim = ansi("\x1b[90m");
    let cyan = ansi("\x1b[36m");
    let yellow = ansi("\x1b[33m");
    let reset = ansi("\x1b[0m");

    const LABEL_WIDTH: usize = 40;
    const MSGS_WIDTH: usize = 10;
    const TOK_WIDTH: usize = 10;
    let rule_width = LABEL_WIDTH
        + 1
        + BREAKDOWN_BAR_WIDTH
        + 1
        + BREAKDOWN_COST_WIDTH
        + 2
        + MSGS_WIDTH
        + 2
        + TOK_WIDTH;
    println!();
    println!(
        "  {bold_cyan} Model usage{reset} — {bold}{}{reset}",
        period_label
    );
    println!("  {dim}{}{reset}", "─".repeat(rule_width));

    if page.rows.is_empty() && page.other.is_none() {
        println!("  No data for this period.");
        println!();
        return Ok(());
    }

    print_breakdown_header(
        "MODEL",
        LABEL_WIDTH,
        &format!("{:>MSGS_WIDTH$}  {:>TOK_WIDTH$}", "MSGS", "TOKENS"),
    );

    let has_duplicate_models = {
        let mut seen = std::collections::HashSet::new();
        page.rows.iter().any(|m| !seen.insert(&m.model))
    };

    // #449 fix: bars scale by cost (the column they sit next to), not by
    // message count. A $66 row no longer renders with more blocks than a
    // $548 row just because it crossed a provider-specific high-volume
    // threshold.
    let max_cost = max_cost_for_rows(&page.rows);
    for m in &page.rows {
        let bar = render_bar(m.cost_cents, max_cost);
        let total_tok =
            m.input_tokens + m.output_tokens + m.cache_read_tokens + m.cache_creation_tokens;
        let raw_label = if has_duplicate_models {
            format!("{} ({})", m.model, m.provider)
        } else {
            m.model.clone()
        };
        let label = truncate_label(&raw_label, LABEL_WIDTH);
        let msgs_cell = format!("{} msgs", m.message_count);
        let tok_cell = format!("{} tok", format_tokens(total_tok));
        println!(
            "  {bold}{:<LABEL_WIDTH$}{reset} {cyan}{}{reset} {yellow}{:>cost_w$}{reset}  {dim}{:>MSGS_WIDTH$}{reset}  {dim}{:>TOK_WIDTH$}{reset}",
            label,
            bar,
            format_cost_cents(m.cost_cents),
            msgs_cell,
            tok_cell,
            cost_w = BREAKDOWN_COST_WIDTH,
        );
    }

    render_breakdown_footer(&page, LABEL_WIDTH, rule_width);
    Ok(())
}

// ─── Breakdown Footer (#448) ─────────────────────────────────────────────────
//
// Every text-mode breakdown view ends in a `Total` footer that reconciles
// to the cent. When rows are truncated, a sibling `(other N: $X)` row is
// rendered just above the total so sum(rendered) + other == total. This is
// the contract the #448 release-blocker nails down.

/// Render the `(other N rows)` line and trailing `Total $X (M of N rows shown)`
/// footer that wraps every breakdown view. No-ops when the page is empty
/// (caller prints its own "no data" message).
///
/// `_name_col_width` is kept as a signature parameter for views that later
/// want tighter per-column alignment (see #450); the current footer uses the
/// `rule_width` anchor so it reconciles visually on every view without
/// per-layout tuning.
fn render_breakdown_footer<T>(page: &BreakdownPage<T>, _name_col_width: usize, rule_width: usize) {
    if page.shown_rows == 0 && page.other.is_none() {
        return;
    }

    let dim = ansi("\x1b[90m");
    let bold = ansi("\x1b[1m");
    let yellow = ansi("\x1b[33m");
    let reset = ansi("\x1b[0m");

    // The footer sits under a rule `rule_width` wide; we right-align the
    // cost to the last column of the rule and pad the label with spaces
    // in front. This reconciles cleanly on every view without needing
    // per-view column tables (which #450 will introduce in the polish
    // pass).
    const COST_COL_WIDTH: usize = 10;
    let rule_len = rule_width.max(20);
    let label_pad = rule_len.saturating_sub(COST_COL_WIDTH);

    if let Some(other) = &page.other {
        let plural = if other.row_count == 1 { "" } else { "s" };
        let label = format!(
            "{} — {} more row{}",
            analytics::BREAKDOWN_OTHER_LABEL,
            other.row_count,
            plural,
        );
        println!(
            "  {dim}{:<label_pad$}{reset}{yellow}{:>width$}{reset}",
            label,
            format_cost_cents(other.cost_cents),
            width = COST_COL_WIDTH,
        );
    }

    println!("  {dim}{}{reset}", "─".repeat(rule_len));

    let shown_note = if page.other.is_some() {
        format!(
            "{dim}({} of {} rows shown — pass --limit 0 for all){reset}",
            page.shown_rows, page.total_rows
        )
    } else if page.total_rows == 0 {
        String::new()
    } else {
        let plural = if page.total_rows == 1 { "" } else { "s" };
        format!("{dim}({} row{} shown){reset}", page.total_rows, plural)
    };

    let total_label_pad = label_pad.saturating_sub(5); // "Total" prefix width
    println!(
        "  {bold}Total{reset}{:<total_label_pad$}{yellow}{:>width$}{reset}  {}",
        "",
        format_cost_cents(page.total_cost_cents),
        shown_note,
        width = COST_COL_WIDTH,
    );
    println!();
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
    limit: usize,
    json_output: bool,
) -> Result<()> {
    let (since, until) = period_date_range(period);

    let page = client.tags(Some(tag_filter), since.as_deref(), until.as_deref(), limit)?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&page)?);
        return Ok(());
    }

    if page.rows.is_empty() && page.other.is_none() {
        println!(
            "No tag data for '{}' ({})",
            tag_filter,
            period_label(period)
        );
        return Ok(());
    }

    let bold = ansi("\x1b[1m");
    let bold_cyan = ansi("\x1b[1;36m");
    let cyan = ansi("\x1b[36m");
    let yellow = ansi("\x1b[33m");
    let reset = ansi("\x1b[0m");

    let dim = ansi("\x1b[90m");

    const LABEL_WIDTH: usize = 40;
    let rule_width = LABEL_WIDTH + 1 + BREAKDOWN_BAR_WIDTH + 1 + BREAKDOWN_COST_WIDTH;
    println!();
    println!(
        "  {bold_cyan} Tag: {}{reset} — {bold}{}{reset}",
        tag_filter,
        period_label(period)
    );
    println!("  {dim}{}{reset}", "─".repeat(rule_width));

    print_breakdown_header("VALUE", LABEL_WIDTH, "");

    let max_cost = max_cost_for_rows(&page.rows);
    for tag in &page.rows {
        let bar = render_bar(tag.cost_cents, max_cost);
        let label = truncate_label(&tag.value, LABEL_WIDTH);
        println!(
            "  {bold}{:<LABEL_WIDTH$}{reset} {cyan}{}{reset} {yellow}{:>cost_w$}{reset}",
            label,
            bar,
            format_cost_cents(tag.cost_cents),
            cost_w = BREAKDOWN_COST_WIDTH,
        );
    }
    render_breakdown_footer(&page, LABEL_WIDTH, rule_width);
    Ok(())
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn period_label_covers_relative_windows() {
        assert_eq!(period_label(StatsPeriod::Today), "Today");
        // `Week` and `Month` now describe the rolling window they
        // resolve to so the label cannot drift from the numeric total
        // again (#447).
        assert_eq!(period_label(StatsPeriod::Week), "Last 7 days");
        assert_eq!(period_label(StatsPeriod::Month), "Last 30 days");
        assert_eq!(period_label(StatsPeriod::All), "All time");

        // Singular forms avoid the "Last 1 days" infelicity.
        assert_eq!(period_label(StatsPeriod::Days(1)), "Last 1 day");
        assert_eq!(period_label(StatsPeriod::Weeks(1)), "Last 1 week");
        assert_eq!(period_label(StatsPeriod::Months(1)), "Last 1 month");

        assert_eq!(period_label(StatsPeriod::Days(7)), "Last 7 days");
        assert_eq!(period_label(StatsPeriod::Weeks(2)), "Last 2 weeks");
        assert_eq!(period_label(StatsPeriod::Months(3)), "Last 3 months");
    }

    #[test]
    fn period_date_range_all_has_no_since() {
        let (since, until) = period_date_range(StatsPeriod::All);
        assert!(since.is_none(), "`all` must not clamp the since bound");
        assert!(until.is_none());
    }

    #[test]
    fn period_date_range_today_pins_local_midnight() {
        let (since, until) = period_date_range(StatsPeriod::Today);
        assert!(since.is_some(), "`today` must anchor a since bound");
        assert!(until.is_none());
    }

    #[test]
    fn period_date_range_relative_windows_go_backwards() {
        // Rolling windows must produce a `since` strictly earlier than
        // `today`'s `since` for any N >= 1. This is the behavior the
        // statusline and cloud dashboard rely on for rolling 1d/7d/30d
        // semantics (#404, ADR-0088 §4).
        let (today_since, _) = period_date_range(StatsPeriod::Today);
        let today_since = today_since.expect("today has a since bound");

        for period in [
            StatsPeriod::Days(1),
            StatsPeriod::Days(7),
            StatsPeriod::Weeks(1),
            StatsPeriod::Weeks(2),
            StatsPeriod::Months(1),
            StatsPeriod::Months(3),
        ] {
            let (since, until) = period_date_range(period);
            let since = since.unwrap_or_else(|| panic!("{:?} must produce a since bound", period));
            assert!(until.is_none(), "{:?} must not clamp until", period);
            assert!(
                since < today_since,
                "relative window {:?} must start before today ({} vs {})",
                period,
                since,
                today_since
            );
        }
    }

    #[test]
    fn week_resolves_to_rolling_seven_days_on_every_weekday() {
        // #447 regression: `-p week` previously used
        // calendar-week-starting-Monday, which collapsed to today on
        // Mondays and to two days on Tuesdays, contradicting the
        // README contract that week == rolling 7 days including today.
        //
        // Parameterize over a full calendar week so every weekday is
        // covered, including the Monday that triggered the original
        // report.
        let base = NaiveDate::from_ymd_opt(2026, 4, 20).unwrap(); // Monday
        for offset in 0..7 {
            let today = base + chrono::Duration::days(offset);
            let week = period_since_date(today, StatsPeriod::Week).expect("week has since");
            let seven_d = period_since_date(today, StatsPeriod::Days(7)).expect("7d has since");
            let today_since =
                period_since_date(today, StatsPeriod::Today).expect("today has since");

            assert_eq!(
                week,
                seven_d,
                "on {today} (weekday={:?}) `-p week` must resolve to the same date as `-p 7d`",
                today.format("%A").to_string()
            );
            assert_ne!(
                week,
                today_since,
                "on {today} (weekday={:?}) `-p week` must NOT collapse to today's since",
                today.format("%A").to_string()
            );
            assert_eq!(
                today - week,
                chrono::Duration::days(7),
                "on {today} `-p week` must anchor exactly 7 days before today"
            );
        }
    }

    #[test]
    fn month_resolves_to_rolling_thirty_days_on_every_day_of_month() {
        // #447 regression: `-p month` previously used
        // first-of-calendar-month, which collapsed to one day on the
        // 1st of the month, contradicting the README contract that
        // month == rolling 30 days including today.
        //
        // Parameterize over a full month so the 1st is covered.
        let base = NaiveDate::from_ymd_opt(2026, 3, 1).unwrap();
        for offset in 0..31 {
            let today = base + chrono::Duration::days(offset);
            let month = period_since_date(today, StatsPeriod::Month).expect("month has since");
            let thirty_d = period_since_date(today, StatsPeriod::Days(30)).expect("30d has since");
            let today_since =
                period_since_date(today, StatsPeriod::Today).expect("today has since");

            assert_eq!(
                month, thirty_d,
                "on {today} `-p month` must resolve to the same date as `-p 30d`"
            );
            assert_ne!(
                month, today_since,
                "on {today} `-p month` must NOT collapse to today's since"
            );
            assert_eq!(
                today - month,
                chrono::Duration::days(30),
                "on {today} `-p month` must anchor exactly 30 days before today"
            );
        }
    }

    #[test]
    fn period_date_range_months_uses_calendar_subtraction() {
        // `StatsPeriod::Months(12)` should land roughly 12 calendar
        // months before today — i.e. at least 360 days back — rather
        // than the 30-day-per-month approximation used before #404.
        // We assert a conservative lower bound so the test is stable
        // regardless of which months are currently in view.
        let today = Local::now().date_naive();
        let (since_rfc, _) = period_date_range(StatsPeriod::Months(12));
        let since_rfc = since_rfc.expect("months(12) has a since bound");
        let since_dt = chrono::DateTime::parse_from_rfc3339(&since_rfc)
            .expect("since is a valid RFC3339 timestamp");
        let since_local = since_dt.with_timezone(&Local).date_naive();
        let delta_days = (today - since_local).num_days();
        assert!(
            delta_days >= 360,
            "Months(12) should span at least 360 days (got {delta_days})"
        );
    }

    // ─── #449 breakdown bar / layout tests ────────────────────────────

    #[test]
    fn render_bar_is_blank_for_zero_cost_rows() {
        // The #449 "bar on $0 row" regression: a `$0.00` row would
        // render with visible blocks because the old code ran the ratio
        // multiplier without a zero guard and relied on a 0.01 floor
        // on `max_cost`.
        let blank = render_bar(0.0, 1_000.0);
        assert_eq!(blank.chars().count(), BREAKDOWN_BAR_WIDTH);
        assert!(
            blank.chars().all(|c| c == ' '),
            "zero-cost row must render an all-spaces bar cell, got {:?}",
            blank
        );

        // Negative cost (should not happen in practice but is the same
        // "no bar" case; guards against sign bugs).
        let neg = render_bar(-1.0, 100.0);
        assert!(neg.chars().all(|c| c == ' '));
    }

    #[test]
    fn render_bar_is_blank_when_every_row_is_zero() {
        // All-$0 window: `max_cost_for_rows` returns 0.0 and the bar
        // renderer must not divide-by-zero or paint phantom blocks.
        let blank = render_bar(0.0, 0.0);
        assert_eq!(blank.chars().count(), BREAKDOWN_BAR_WIDTH);
        assert!(blank.chars().all(|c| c == ' '));
    }

    #[test]
    fn render_bar_is_proportional_to_cost_not_message_count() {
        // #449 primary complaint: bars in `--models` scaled by
        // `message_count`, so a $66 / 5814-msg row out-drew a $548 / 381-msg
        // row. The shared renderer scales by the caller's `max_cost`
        // value — message count is no longer in the input.
        let max = 1_000.0_f64;

        // A row at 100% of max fills the bar.
        let full = render_bar(1_000.0, max);
        let full_blocks = full.chars().filter(|c| *c == '\u{2588}').count();
        assert_eq!(full_blocks, BREAKDOWN_BAR_WIDTH);

        // A row at 50% fills half.
        let half = render_bar(500.0, max);
        let half_blocks = half.chars().filter(|c| *c == '\u{2588}').count();
        assert_eq!(half_blocks, BREAKDOWN_BAR_WIDTH / 2);

        // A row at 0.4% still renders at least one block so the reader
        // can distinguish it from a $0 row.
        let tiny = render_bar(4.0, max);
        let tiny_blocks = tiny.chars().filter(|c| *c == '\u{2588}').count();
        assert!((1..BREAKDOWN_BAR_WIDTH).contains(&tiny_blocks));
    }

    #[test]
    fn render_bar_always_pads_to_fixed_width() {
        // Width invariant: every bar string is exactly
        // `BREAKDOWN_BAR_WIDTH` cells (blocks + spaces) so adjacent
        // columns line up regardless of the row's cost.
        for pct in [0, 1, 25, 50, 75, 100, 150] {
            let cost = pct as f64 * 10.0;
            let bar = render_bar(cost, 1_000.0);
            assert_eq!(
                bar.chars().count(),
                BREAKDOWN_BAR_WIDTH,
                "bar at {pct}% of max must be exactly {BREAKDOWN_BAR_WIDTH} cells"
            );
        }
    }

    #[test]
    fn truncate_label_is_utf8_boundary_safe() {
        // #389 / #383 / #404 / #445 precedent: byte-indexed truncation
        // on user strings panics on multi-byte codepoints. The label
        // truncator operates on Unicode scalars so tickets / file paths
        // carrying non-ASCII characters never crash.
        assert_eq!(truncate_label("short", 40), "short");
        assert_eq!(truncate_label("", 40), "");

        let long_ascii = "a".repeat(60);
        let truncated = truncate_label(&long_ascii, 20);
        assert_eq!(truncated.chars().count(), 20);
        assert!(truncated.starts_with('…'));

        // Multi-byte: an emoji is 4 bytes but 1 char. The truncator
        // must count chars, not bytes, so we never split mid-codepoint.
        let emoji_path = "src/🚀/main.rs".repeat(5);
        let truncated = truncate_label(&emoji_path, 12);
        assert_eq!(truncated.chars().count(), 12);
        assert!(truncated.starts_with('…'));
    }

    #[test]
    fn max_cost_for_rows_handles_empty_and_all_zero() {
        // Empty page: no division-by-zero hazard.
        let empty: Vec<budi_core::analytics::RepoUsage> = vec![];
        assert_eq!(max_cost_for_rows(&empty), 0.0);

        // All-$0 page (a real case in `--files today` when no tool-arg
        // attribution landed). `render_bar` takes this 0.0 and paints
        // blank cells, which is what the reader expects.
        let all_zero = vec![
            budi_core::analytics::RepoUsage {
                repo_id: "a".into(),
                display_path: "a".into(),
                message_count: 10,
                input_tokens: 0,
                output_tokens: 0,
                cost_cents: 0.0,
            },
            budi_core::analytics::RepoUsage {
                repo_id: "b".into(),
                display_path: "b".into(),
                message_count: 10,
                input_tokens: 0,
                output_tokens: 0,
                cost_cents: 0.0,
            },
        ];
        assert_eq!(max_cost_for_rows(&all_zero), 0.0);

        // Sanity: the max is taken from `cost_cents`, not any other
        // field. Keeps `--models` from regressing to message-count
        // scaling (#449).
        let mixed = vec![
            budi_core::analytics::RepoUsage {
                repo_id: "big-msgs-cheap".into(),
                display_path: "".into(),
                message_count: 100_000,
                input_tokens: 0,
                output_tokens: 0,
                cost_cents: 10.0,
            },
            budi_core::analytics::RepoUsage {
                repo_id: "few-msgs-dear".into(),
                display_path: "".into(),
                message_count: 10,
                input_tokens: 0,
                output_tokens: 0,
                cost_cents: 500.0,
            },
        ];
        assert_eq!(max_cost_for_rows(&mixed), 500.0);
    }

    #[test]
    fn breakdown_header_has_cost_column_in_canonical_position() {
        // #449 acceptance: every breakdown shows a header row, and the
        // `COST` label sits at the right edge of the bar+cost combined
        // width so rendered cost values align under it.
        let header = format_breakdown_header_text("TICKET", 24, "");
        assert!(
            header.starts_with("  TICKET"),
            "header must start with indented label title, got {:?}",
            header
        );
        assert!(
            header.trim_end().ends_with("COST"),
            "header must end with the COST title, got {:?}",
            header
        );

        // With extras, the extra headers follow COST separated by two
        // spaces (same gap the row formatter uses).
        let with_extras =
            format_breakdown_header_text("ACTIVITY", 18, "CONFIDENCE   TOP_BRANCH             ");
        assert!(with_extras.contains("COST  CONFIDENCE"));
    }

    #[test]
    fn breakdown_row_layout_matches_tag_view_template() {
        // #449 acceptance: all views line up as `LABEL  BAR  COST  EXTRAS`.
        // Baseline: the `--tag` shape (the layout the ticket picks as
        // canonical) produces exactly this string for a half-cost row.
        let line = format_breakdown_row_text("activity/coding", 40, 500.0, 1_000.0, "");
        let expected = format!(
            "  {:<40} {} {:>10}",
            "activity/coding",
            render_bar(500.0, 1_000.0),
            format_cost_cents(500.0),
        );
        assert_eq!(line, expected);
    }

    /// Helper: canonical right-edge column that every view's `COST`
    /// title must line up against, given the shared bar + cost widths.
    fn cost_right_edge(label_width: usize) -> usize {
        // Matches `format_breakdown_header_text` / `format_breakdown_row_text`:
        // "  " + label + " " + (bar + " " + cost right-aligned).
        2 + label_width + 1 + BREAKDOWN_BAR_WIDTH + 1 + BREAKDOWN_COST_WIDTH
    }

    /// Helper: char-based position of `needle` inside `haystack`. Plain
    /// `str::find` returns byte offsets, which over-counts every bar
    /// block (3 bytes per `█`) — we want character columns here so the
    /// layout assertions line up with visual widths.
    fn char_pos_of(haystack: &str, needle: &str) -> Option<usize> {
        let byte_pos = haystack.find(needle)?;
        Some(haystack[..byte_pos].chars().count())
    }

    #[test]
    fn breakdown_row_snapshot_models_view() {
        // Snapshot baseline for `budi stats --models` row layout
        // (#449 acceptance). One row at the top of the scale, one
        // tiny but non-zero row, one $0 row — the three cases the
        // ticket's Reproduction section calls out as bug triggers.
        let label_w = 40usize;
        let extras_header = format!("{:>10}  {:>10}", "MSGS", "TOKENS");
        let header = format_breakdown_header_text("MODEL", label_w, &extras_header);

        let heavy_extras = format!("{:>10}  {:>10}", "24114 msgs", "120M tok");
        let heavy = format_breakdown_row_text(
            "claude-opus-4-6 (claude_code)",
            label_w,
            210_000.0,
            210_000.0,
            &heavy_extras,
        );
        let tiny = format_breakdown_row_text(
            "claude-haiku-4-5 (claude_code)",
            label_w,
            65.74,
            210_000.0,
            &format!("{:>10}  {:>10}", "5814 msgs", "30M tok"),
        );
        let pending = format_breakdown_row_text(
            "(untagged)",
            label_w,
            0.0,
            210_000.0,
            &format!("{:>10}  {:>10}", "162 msgs", "0 tok"),
        );

        // Header `COST` title right-aligns to the canonical edge so it
        // sits above every row's cost cell.
        let cost_title_right = header.find("COST").unwrap() + "COST".len();
        assert_eq!(
            cost_title_right,
            cost_right_edge(label_w),
            "MODEL view: COST title must align to the shared cost-cell right edge\nheader: {header:?}"
        );

        // Extras header and row extras share the same right edge.
        assert!(header.trim_end().ends_with("TOKENS"));
        assert!(heavy.trim_end().ends_with("tok"));

        // The heavy row renders a full bar; the tiny row renders a
        // visible but non-full bar; the `(untagged)` $0 row renders
        // a blank bar. This is the #449 bug's 1:1 repro checked.
        let heavy_blocks = heavy.chars().filter(|c| *c == '\u{2588}').count();
        let tiny_blocks = tiny.chars().filter(|c| *c == '\u{2588}').count();
        let pending_blocks = pending.chars().filter(|c| *c == '\u{2588}').count();
        assert_eq!(heavy_blocks, BREAKDOWN_BAR_WIDTH);
        assert!((1..BREAKDOWN_BAR_WIDTH).contains(&tiny_blocks));
        assert_eq!(pending_blocks, 0);
    }

    #[test]
    fn breakdown_row_snapshot_tickets_view() {
        // Golden snapshot for `budi stats --tickets` — pinned so the
        // bar/cost order cannot silently flip back to the pre-#449
        // layout (cost-before-bar).
        let label_w = 24usize;
        let extras = format!("{:<18}  {:<24}", "SOURCE", "TOP_BRANCH");
        let header = format_breakdown_header_text("TICKET", label_w, &extras);

        assert!(header.starts_with("  TICKET"));
        let cost_title_right = header.find("COST").unwrap() + "COST".len();
        assert_eq!(
            cost_title_right,
            cost_right_edge(label_w),
            "TICKET view: COST title must align to the shared cost-cell right edge"
        );
        assert!(header.contains("SOURCE"));
        assert!(header.contains("TOP_BRANCH"));

        // Canonical row: label, full bar (row cost == max), cost cell,
        // then extras. The dollar character marks the right edge of the
        // cost cell.
        let row_extras = format!("{:<18}  {:<24}", "src=branch", "04-20-pava-1669");
        let row = format_breakdown_row_text("PAVA-1669", label_w, 2_265.0, 2_265.0, &row_extras);
        assert!(row.starts_with("  PAVA-1669"));
        assert!(
            row.chars().filter(|c| *c == '\u{2588}').count() == BREAKDOWN_BAR_WIDTH,
            "max-cost row must render a full bar"
        );
        // Cost cell sits at the canonical right edge (the first block
        // in the extras section starts exactly 2 chars past that).
        let cost_cell = format_cost_cents(2_265.0);
        let cost_cell_right = char_pos_of(&row, &cost_cell).unwrap() + cost_cell.chars().count();
        assert_eq!(
            cost_cell_right,
            cost_right_edge(label_w),
            "row cost cell must end at the same column as the header COST title"
        );
        // Extras follow with the 2-space gap baked into the shared row
        // formatter.
        assert!(row.contains("src=branch"));
        assert!(row.contains("04-20-pava-1669"));

        // `(untagged)` row with a real dollar cost equal to max:
        // bar present, full width. Prior to #449 this row rendered
        // with no bar at all because message_count didn't correlate
        // with cost.
        let untagged = format_breakdown_row_text(
            "(untagged)",
            label_w,
            9_122.0,
            9_122.0,
            &format!("{:<18}  {:<24}", "src=--", "--"),
        );
        assert!(untagged.contains("(untagged)"));
        assert_eq!(
            untagged.chars().filter(|c| *c == '\u{2588}').count(),
            BREAKDOWN_BAR_WIDTH,
            "the dominant row (same as max_cost) must render a full bar"
        );
    }

    #[test]
    fn breakdown_row_snapshot_activities_view() {
        // Golden snapshot for `budi stats --activities`. Preserves the
        // `conf=…` in-row legend (addressed more fully by #450) while
        // pinning the #449 layout contract.
        let label_w = 18usize;
        let extras = format!("{:<11}  {:<22}", "CONFIDENCE", "TOP_BRANCH");
        let header = format_breakdown_header_text("ACTIVITY", label_w, &extras);

        let cost_title_right = header.find("COST").unwrap() + "COST".len();
        assert_eq!(
            cost_title_right,
            cost_right_edge(label_w),
            "ACTIVITY view: COST title must align to the shared cost-cell right edge"
        );
        assert!(header.contains("CONFIDENCE"));
        assert!(header.contains("TOP_BRANCH"));

        let row = format_breakdown_row_text(
            "coding",
            label_w,
            1_234.56,
            1_234.56,
            &format!("{:<11}  {:<22}", "conf=medium", "main"),
        );
        assert!(row.starts_with("  coding"));
        assert!(row.contains("conf=medium"));
        // Row is the max-cost row → full bar.
        assert_eq!(
            row.chars().filter(|c| *c == '\u{2588}').count(),
            BREAKDOWN_BAR_WIDTH,
        );

        // $0 activity row renders a blank bar.
        let zero = format_breakdown_row_text(
            "classifier_pending",
            label_w,
            0.0,
            1_234.56,
            &format!("{:<11}  {:<22}", "conf=--", "--"),
        );
        assert_eq!(zero.chars().filter(|c| *c == '\u{2588}').count(), 0);
    }

    #[test]
    fn breakdown_row_snapshot_tag_view() {
        // Golden snapshot for `budi stats --tag <key>` — the layout
        // every other breakdown now mirrors. Kept as the canonical
        // shape so a future refactor that regresses one view will
        // fail a targeted test.
        let label_w = 40usize;
        let header = format_breakdown_header_text("VALUE", label_w, "");
        assert!(header.starts_with("  VALUE"));
        let cost_title_right = header.find("COST").unwrap() + "COST".len();
        assert_eq!(
            cost_title_right,
            cost_right_edge(label_w),
            "TAG view: COST title must align to the shared cost-cell right edge"
        );

        // Half-cost row: bar fills exactly half, cost cell lines up
        // under the COST header.
        let row = format_breakdown_row_text("feature-work", label_w, 500.0, 1_000.0, "");
        assert!(row.starts_with("  feature-work"));
        let half_blocks = row.chars().filter(|c| *c == '\u{2588}').count();
        assert_eq!(
            half_blocks,
            BREAKDOWN_BAR_WIDTH / 2,
            "50% cost row must render half a bar"
        );
        let cost_cell = format_cost_cents(500.0);
        let cost_cell_right = char_pos_of(&row, &cost_cell).unwrap() + cost_cell.chars().count();
        assert_eq!(cost_cell_right, cost_right_edge(label_w));

        // Long tag values truncate with `…` prefix, not byte-index
        // panic (#389 / #383 / #404 / #445 UTF-8 class).
        let long = truncate_label("verylongtagvaluethatwillwraparound_123456789_abcdef", 40);
        assert_eq!(long.chars().count(), 40);
        assert!(long.starts_with('…'));
    }
}
