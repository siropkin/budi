use anyhow::{Context, Result};
use budi_core::analytics::{self, BreakdownPage, BreakdownRowCost};
use budi_core::pricing::display::{self as display, Placeholder};
use chrono::{Local, Months, NaiveDate, TimeZone};

use crate::StatsPeriod;
use crate::client::DaemonClient;

use super::ansi;

// ─── Shared Breakdown Rendering (#449) ───────────────────────────────────────
//
// Every `budi stats` breakdown view (`projects / branches / tickets /
// activities / files / models / tag`) ships through the same bar
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

/// Returns `true` when the only row on the page is the DB's generic
/// `(untagged)` sentinel. Used to swap the default per-row rendering
/// for a one-line empty-state message — a one-row breakdown made of
/// just `(untagged)` looks like a filesystem fault rather than "no
/// attribution emitted in this window" (#450 acceptance D).
fn is_only_untagged<T, F>(rows: &[T], name: F) -> bool
where
    F: Fn(&T) -> &str,
{
    rows.len() == 1 && is_untagged(name(&rows[0]))
}

/// Period-aware tip rendered when a breakdown's only row is the
/// `(untagged)` bucket. Encourages widening the window so attribution
/// signal (which accumulates over time) actually has a chance to show
/// up — the `Today` and `1d` cases are the most common offenders.
fn untagged_only_tip(period: StatsPeriod) -> Option<&'static str> {
    match period {
        StatsPeriod::Today | StatsPeriod::Days(1) => Some("Try --period 7d."),
        _ => None,
    }
}

/// Print the "no labelled signal in this window" empty-state. Called
/// when the only row on a page is the `(untagged)` bucket, to avoid
/// a one-row table that looks like a filesystem fault. (#450
/// acceptance D)
fn render_untagged_only_empty_state(view: BreakdownView, period: StatsPeriod) {
    let dim = ansi("\x1b[90m");
    let reset = ansi("\x1b[0m");
    let label = match view {
        BreakdownView::Projects => "repository attribution",
        BreakdownView::Branches => "branch attribution",
        BreakdownView::Tickets => "ticket attribution",
        BreakdownView::Activities => "activity attribution",
        BreakdownView::Files => "file attribution",
        BreakdownView::Models => "labelled model usage",
        BreakdownView::Tag => "tag attribution",
    };
    println!("  No {label} emitted in this window.");
    if let Some(tip) = untagged_only_tip(period) {
        println!("  {dim}{tip}{reset}");
    }
    println!();
}

/// Truncate a display label to at most `max_chars` characters, prefixing
/// the kept suffix with `…` when truncation happens. Operates on Unicode
/// scalars (not bytes) to stay safe on multi-byte file paths / ticket
/// ids — see #389 / #383 / #404 / #445 for the precedent bug class.
///
/// Retained for legacy call sites and snapshot tests; production rows
/// render through [`truncate_label_middle`] so head + tail remain
/// visible for long identifiers (branch prefixes, path suffixes).
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

/// Middle-ellipsis truncation: keep the leading and trailing chars,
/// drop the middle with `…`. Unicode-scalar math (not byte math) so
/// multi-byte codepoints never split — see #389 / #383 / #404 / #445.
///
/// The split biases slightly toward the tail so file paths retain
/// their most identifying segment (the filename), while still
/// surfacing the repo-root prefix. Returns the input unchanged when
/// already short enough.
fn truncate_label_middle(label: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let char_count = label.chars().count();
    if char_count <= max_chars {
        return label.to_string();
    }
    if max_chars <= 2 {
        // Too narrow to keep head + tail + ellipsis; fall back to the
        // tail-preserving form so the output still fits.
        return truncate_label(label, max_chars);
    }
    let keep = max_chars - 1; // reserve one slot for `…`
    let head_len = keep / 2;
    let tail_len = keep - head_len;
    let chars: Vec<char> = label.chars().collect();
    let mut out = String::with_capacity(max_chars * 4);
    out.extend(chars.iter().take(head_len));
    out.push('…');
    out.extend(chars.iter().skip(char_count - tail_len));
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
    let cost_cell = format_cost_cents_fixed(cost_cents);
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
    label_width: usize,
    include_pending: bool,
    include_non_repo: bool,
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

    // Clamp label-width to a sensible floor so the middle-ellipsis
    // renderer always has room for head + tail + `…`.
    let label_width = label_width.max(8);

    if let Some(ref tag_filter) = tag {
        return cmd_stats_tags(&client, period, tag_filter, limit, label_width, json_output);
    }

    if let Some(ref f) = file {
        return cmd_stats_file_detail(&client, period, f, repo.as_deref(), json_output);
    }

    if files {
        return cmd_stats_files(&client, period, limit, label_width, json_output);
    }

    if let Some(ref ac) = activity {
        return cmd_stats_activity_detail(&client, period, ac, repo.as_deref(), json_output);
    }

    if activities {
        return cmd_stats_activities(&client, period, limit, label_width, json_output);
    }

    if let Some(ref tk) = ticket {
        return cmd_stats_ticket_detail(&client, period, tk, repo.as_deref(), json_output);
    }

    if tickets {
        return cmd_stats_tickets(&client, period, limit, label_width, json_output);
    }

    if let Some(ref br) = branch {
        return cmd_stats_branch_detail(&client, period, br, repo.as_deref(), json_output);
    }

    if branches {
        return cmd_stats_branches(&client, period, limit, label_width, json_output);
    }

    if models {
        return cmd_stats_models(
            &client,
            period,
            limit,
            label_width,
            include_pending,
            json_output,
        );
    }

    if projects {
        return cmd_stats_projects(
            &client,
            period,
            limit,
            label_width,
            include_non_repo,
            json_output,
        );
    }

    if json_output {
        let (since, until) = period_date_range(period);
        let summary = client.summary(since.as_deref(), until.as_deref(), provider.as_deref())?;
        let cost = client.cost(since.as_deref(), until.as_deref(), provider.as_deref())?;
        // #482 acceptance: expose per-provider counts so scripts can
        // reconcile `sum(providers.total_messages) == total_messages`
        // both ways (user + assistant split, and the combined total).
        let providers = client
            .providers(since.as_deref(), until.as_deref())
            .unwrap_or_default();
        let filtered_providers: Vec<&analytics::ProviderStats> = providers
            .iter()
            .filter(|p| provider.as_deref().is_none_or(|sel| p.provider == sel))
            .collect();
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
            map.insert(
                "providers".to_string(),
                serde_json::to_value(&filtered_providers)?,
            );
            // Expose the resolved window so scripting users can verify
            // which period `--period today|week|month|Nd|Nw|Nm|all`
            // actually mapped to (#447 acceptance: `budi stats -p week`
            // and `-p 7d` must resolve to the same window on every
            // weekday).
            map.insert("window_start".to_string(), serde_json::json!(since));
            map.insert("window_end".to_string(), serde_json::json!(until));
        }
        super::print_json(&obj)?;
        return Ok(());
    }

    // #451: every period renders the same set of summary blocks
    // (header, Agents, Total, Tokens, Est. cost, cost-component
    // sub-line, cache savings, optional Cursor-lag footnote). The
    // pre-#451 dispatcher branched on `providers.len() > 1`, which
    // dropped the Agents block whenever a window happened to surface
    // a single provider — making `today` look thinner than `1d` /
    // `7d` / `month` for the same data.
    cmd_stats_summary(&client, period, provider.as_deref())
}

/// Color palette for the summary view. Production builds use `ansi()`
/// codes; tests use the `plain()` palette so snapshot strings don't
/// depend on terminal state.
struct SummaryPalette {
    bold_cyan: &'static str,
    bold: &'static str,
    dim: &'static str,
    cyan: &'static str,
    yellow: &'static str,
    green: &'static str,
    reset: &'static str,
}

impl SummaryPalette {
    /// Honour `NO_COLOR` and TTY detection (via `ansi()`).
    fn from_env() -> Self {
        Self {
            bold_cyan: ansi("\x1b[1;36m"),
            bold: ansi("\x1b[1m"),
            dim: ansi("\x1b[90m"),
            cyan: ansi("\x1b[36m"),
            yellow: ansi("\x1b[33m"),
            green: ansi("\x1b[32m"),
            reset: ansi("\x1b[0m"),
        }
    }

    /// All-empty palette so test snapshots are pure ASCII.
    #[cfg(test)]
    const fn plain() -> Self {
        Self {
            bold_cyan: "",
            bold: "",
            dim: "",
            cyan: "",
            yellow: "",
            green: "",
            reset: "",
        }
    }
}

/// Render the summary view to a String. Pure function — fetched data
/// goes in, formatted text comes out. The shape is fixed regardless of
/// period (#451): header → Agents → Total → Tokens → Est. cost →
/// cost-component sub-line → cache savings → optional Cursor-lag
/// footnote.
fn format_summary(
    period: StatsPeriod,
    provider: Option<&str>,
    summary: &budi_core::analytics::UsageSummary,
    est: &budi_core::cost::CostEstimate,
    providers: &[analytics::ProviderStats],
    palette: &SummaryPalette,
) -> String {
    use std::fmt::Write as _;

    let SummaryPalette {
        bold_cyan,
        bold,
        dim,
        cyan,
        yellow,
        green,
        reset,
    } = *palette;

    let mut out = String::new();
    let period_label = period_label(period);
    let provider_label = provider.unwrap_or("all");

    let displayed_providers: Vec<&analytics::ProviderStats> = providers
        .iter()
        .filter(|p| provider.is_none_or(|sel| p.provider == sel))
        .collect();

    writeln!(out).unwrap();
    if provider.is_some() {
        writeln!(
            out,
            "  {bold_cyan} budi stats{reset} — {bold}{period_label}{reset} {dim}({provider_label}){reset}",
        )
        .unwrap();
    } else {
        writeln!(
            out,
            "  {bold_cyan} budi stats{reset} — {bold}{period_label}{reset}",
        )
        .unwrap();
    }
    writeln!(out, "  {dim}{}{reset}", "─".repeat(40)).unwrap();

    if summary.total_messages == 0 {
        writeln!(out, "  No data for this period.").unwrap();
        writeln!(out).unwrap();
        return out;
    }

    // Agents block — unconditional shape (#451). Even with one
    // provider in the window we render the row so the summary doesn't
    // change shape between Today (often 1 provider) and 1d / 7d / 30d
    // (often multi-provider).
    writeln!(out, "  {bold}Agents{reset}").unwrap();
    if displayed_providers.is_empty() {
        // Reachable when `--provider P` filters out every provider in
        // the window, or when the daemon hasn't recorded a per-agent
        // breakdown yet. We still print the block so the shape is
        // identical to the populated case.
        writeln!(
            out,
            "    {dim}(no provider data — run `budi vitals` to inspect tail offsets){reset}",
        )
        .unwrap();
    } else {
        // #482: render the per-provider row's message count as
        // `total_messages` (user + assistant) so the column label `msgs`
        // carries the same unit as the Total row below
        // (`3050 messages (1680 user, 1370 assistant)`). Pre-8.3.1 this
        // was assistant-only, which read as a slice of Total but actually
        // undercounted by `total_user_messages`.
        //
        // #494: every tokens cell carries an explicit `tok` suffix so
        // zero-cost providers render `0 tok` instead of a bare `0`
        // beside `159.0M` on the same column. Matches the breakdown
        // views' `{n} tok` shape.
        //
        // #486: per-provider cost uses fixed-precision `$X,XXX.XX` so
        // Claude Code's `$126.40` doesn't collapse to `$126` beside
        // Cursor's `$0.00` on the same column. Matches the summary
        // `Est. cost` + component sub-line precision below.
        for ps in &displayed_providers {
            let total_tokens = ps.input_tokens
                + ps.output_tokens
                + ps.cache_creation_tokens
                + ps.cache_read_tokens;
            let cost_cents = if ps.total_cost_cents > 0.0 {
                ps.total_cost_cents
            } else {
                ps.estimated_cost * 100.0
            };
            writeln!(
                out,
                "    {cyan}{:<14}{reset} {:>5} msgs  {} tok  {yellow}{}{reset}",
                ps.display_name,
                ps.total_messages,
                format_tokens(total_tokens),
                format_cost_cents_fixed(cost_cents),
            )
            .unwrap();
        }
    }
    writeln!(out).unwrap();

    writeln!(
        out,
        "  {bold}Total{reset}        {} messages {dim}({} user, {} assistant){reset}",
        summary.total_messages, summary.total_user_messages, summary.total_assistant_messages,
    )
    .unwrap();
    // #445 item 5: the pre-8.3 summary showed only `{input} in, {output}
    // out` while the per-agent rows above summed **all four** token
    // components (input + output + cache-write + cache-read). Cursor
    // traffic is cache-heavy, so the two lines appeared to disagree —
    // `591.2M + 137.7M + 805.4M` on per-agent rows vs `89.9M in, 6.8M
    // out` on the summary. Render every token component the per-agent
    // total includes so the reader can reconcile them without reading
    // the renderer source.
    writeln!(
        out,
        "  {bold}Tokens{reset}       {} input · {} output · {} cache-write · {} cache-read",
        format_tokens(summary.total_input_tokens),
        format_tokens(summary.total_output_tokens),
        format_tokens(summary.total_cache_creation_tokens),
        format_tokens(summary.total_cache_read_tokens),
    )
    .unwrap();

    writeln!(out).unwrap();
    // #486: summary-block cost precision is fixed-decimal `$X,XXX.XX`
    // so the top-line `Est. cost` never collapses cents (pre-8.3.1
    // `$126` masked a real `$126.40` visible on the component sub-line,
    // leaving a fresh reader unable to tell whether the top value was
    // `$126.00` rounded or the sub-line total silently lost 40¢).
    // `format_cost_cents_fixed` takes cents; multiply dollars by 100.
    writeln!(
        out,
        "  {bold}Est. cost{reset}    {yellow}{}{reset}",
        format_cost_cents_fixed(est.total_cost * 100.0)
    )
    .unwrap();
    // Cost-component sub-line — unconditional (#451). Reads $0.00 in
    // each cell when the window has no spend, so the shape stays
    // identical to a populated summary. Uses the same fixed-decimal
    // formatter as the top-line so the four base components always
    // sum to the top-line value on the rendered screen.
    //
    // #520: if `other_cost` is non-zero — typically Cursor fast-mode,
    // thinking-token cost, or web-search fees that are in
    // `total_cost` (via `SUM(cost_cents)` at ingest) but absent from
    // the four base token×rate components — render a fifth `other`
    // cell so the sub-line visually reconciles to the top-line.
    // Silently omitted when the residual is zero (typical for a
    // Claude-only window) to keep the sub-line readable.
    if est.other_cost > 0.0 {
        writeln!(
            out,
            "  {dim}  input {}  output {}  cache write {}  cache read {}  other {}{reset}",
            format_cost_cents_fixed(est.input_cost * 100.0),
            format_cost_cents_fixed(est.output_cost * 100.0),
            format_cost_cents_fixed(est.cache_write_cost * 100.0),
            format_cost_cents_fixed(est.cache_read_cost * 100.0),
            format_cost_cents_fixed(est.other_cost * 100.0),
        )
        .unwrap();
    } else {
        writeln!(
            out,
            "  {dim}  input {}  output {}  cache write {}  cache read {}{reset}",
            format_cost_cents_fixed(est.input_cost * 100.0),
            format_cost_cents_fixed(est.output_cost * 100.0),
            format_cost_cents_fixed(est.cache_write_cost * 100.0),
            format_cost_cents_fixed(est.cache_read_cost * 100.0),
        )
        .unwrap();
    }
    // Cache-savings line — unconditional (#451). $0.00 when no cache
    // hits accumulated; skipping it in 8.2 made `today` look
    // structurally different from `1d` whenever cache savings happened
    // to be zero.
    writeln!(
        out,
        "  {green}  cache savings {}{reset}",
        format_cost_cents_fixed(est.cache_savings * 100.0)
    )
    .unwrap();

    // Cursor-lag footnote — printed whenever Cursor is one of the
    // displayed agents in the window. The pre-#451 summary-filtered
    // path only printed it when the user explicitly passed
    // `--provider cursor`, so the multi-agent view (which already
    // showed a Cursor row) silently dropped the caveat that row
    // needs.
    if displayed_providers.iter().any(|p| p.provider == "cursor") {
        writeln!(
            out,
            "  {dim}* {}{reset}",
            budi_core::analytics::CURSOR_LAG_HINT
        )
        .unwrap();
    }

    writeln!(out).unwrap();
    out
}

/// Unified `budi stats` summary renderer (#451).
///
/// Every period — `today`, `week`, `month`, `1d`, `7d`, `30d` — emits the
/// same shape via [`format_summary`]. The pre-#451 renderer split into
/// `cmd_stats_summary_filtered` (no Agents block) and `cmd_stats_multi_agent`
/// (Agents block, gated on `providers.len() > 1`), which made the most-used
/// view (`today`) the thinnest one whenever the window happened to surface
/// a single provider.
fn cmd_stats_summary(
    client: &DaemonClient,
    period: StatsPeriod,
    provider: Option<&str>,
) -> Result<()> {
    let (since, until) = period_date_range(period);
    let summary = client.summary(since.as_deref(), until.as_deref(), provider)?;
    let est = client.cost(since.as_deref(), until.as_deref(), provider)?;
    // The Agents block, the Cursor-lag footnote, and the per-provider
    // tokens/cost breakdown all need this list. Fetched once per
    // invocation so the text and JSON paths agree on the snapshot.
    let providers = client.providers(since.as_deref(), until.as_deref())?;

    let palette = SummaryPalette::from_env();
    let rendered = format_summary(period, provider, &summary, &est, &providers, &palette);
    print!("{rendered}");
    Ok(())
}

fn cmd_stats_projects(
    client: &DaemonClient,
    period: StatsPeriod,
    limit: usize,
    label_width: usize,
    include_non_repo: bool,
    json_output: bool,
) -> Result<()> {
    let (since, until) = period_date_range(period);
    let page = client.projects(since.as_deref(), until.as_deref(), limit)?;
    let non_repo_rows = if include_non_repo {
        // #442: fetch per-cwd-basename detail for the non-repo bucket so
        // operators who want the pre-8.3 folder-name view can still get
        // it. Default behavior leaves the main table clean.
        client.non_repo(since.as_deref(), until.as_deref(), limit)?
    } else {
        Vec::new()
    };

    if json_output {
        if include_non_repo {
            let payload = serde_json::json!({
                "repositories": &page,
                "non_repo": &non_repo_rows,
            });
            super::print_json(&payload)?;
        } else {
            super::print_json(&page)?;
        }
        return Ok(());
    }

    let period_label = period_label(period);

    let bold_cyan = ansi("\x1b[1;36m");
    let bold = ansi("\x1b[1m");
    let dim = ansi("\x1b[90m");
    let cyan = ansi("\x1b[36m");
    let yellow = ansi("\x1b[33m");
    let reset = ansi("\x1b[0m");

    let rule_width = label_width + 1 + BREAKDOWN_BAR_WIDTH + 1 + BREAKDOWN_COST_WIDTH;
    println!();
    println!(
        "  {bold_cyan} Repositories{reset} — {bold}{}{reset}",
        period_label
    );
    println!("  {dim}{}{reset}", "─".repeat(rule_width));

    if page.rows.is_empty() && page.other.is_none() && non_repo_rows.is_empty() {
        println!("  No data for this period.");
        println!();
        return Ok(());
    }

    if !page.rows.is_empty() || page.other.is_some() {
        if is_only_untagged(&page.rows, |r| &r.repo_id) && page.other.is_none() {
            render_untagged_only_empty_state(BreakdownView::Projects, period);
        } else {
            print_breakdown_header("REPOSITORY", label_width, "");

            let max_cost = max_cost_for_rows(&page.rows);
            for r in &page.rows {
                let bar = render_bar(r.cost_cents, max_cost);
                let label = truncate_label_middle(
                    &display_dimension(BreakdownView::Projects, &r.repo_id),
                    label_width,
                );
                println!(
                    "  {bold}{:<label_w$}{reset} {cyan}{}{reset} {yellow}{:>cost_w$}{reset}",
                    label,
                    bar,
                    format_cost_cents_fixed(r.cost_cents),
                    label_w = label_width,
                    cost_w = BREAKDOWN_COST_WIDTH,
                );
            }

            render_breakdown_footer(&page, label_width, rule_width);
        }
    }

    if include_non_repo && !non_repo_rows.is_empty() {
        println!();
        println!(
            "  {bold_cyan} Non-repository folders{reset} — {bold}{}{reset}",
            period_label
        );
        println!("  {dim}{}{reset}", "─".repeat(rule_width));
        print_breakdown_header("FOLDER", label_width, "");
        let max_cost = non_repo_rows
            .iter()
            .map(|r| r.cost_cents)
            .fold(0.0_f64, f64::max);
        for r in &non_repo_rows {
            let bar = render_bar(r.cost_cents, max_cost);
            let label = truncate_label_middle(&r.repo_id, label_width);
            println!(
                "  {bold}{:<label_w$}{reset} {cyan}{}{reset} {yellow}{:>cost_w$}{reset}",
                label,
                bar,
                format_cost_cents_fixed(r.cost_cents),
                label_w = label_width,
                cost_w = BREAKDOWN_COST_WIDTH,
            );
        }
    }

    Ok(())
}

fn cmd_stats_branches(
    client: &DaemonClient,
    period: StatsPeriod,
    limit: usize,
    label_width: usize,
    json_output: bool,
) -> Result<()> {
    let (since, until) = period_date_range(period);
    let page = client.branches(since.as_deref(), until.as_deref(), limit)?;

    if json_output {
        super::print_json(&page)?;
        return Ok(());
    }

    let period_label = period_label(period);

    let bold_cyan = ansi("\x1b[1;36m");
    let bold = ansi("\x1b[1m");
    let dim = ansi("\x1b[90m");
    let cyan = ansi("\x1b[36m");
    let yellow = ansi("\x1b[33m");
    let reset = ansi("\x1b[0m");

    const REPO_WIDTH: usize = 16;
    let rule_width =
        label_width + 1 + BREAKDOWN_BAR_WIDTH + 1 + BREAKDOWN_COST_WIDTH + 2 + REPO_WIDTH;
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

    if is_only_untagged(&page.rows, |b| &b.git_branch) && page.other.is_none() {
        render_untagged_only_empty_state(BreakdownView::Branches, period);
        return Ok(());
    }

    print_breakdown_header("BRANCH", label_width, &format!("{:<REPO_WIDTH$}", "REPO"));

    let max_cost = max_cost_for_rows(&page.rows);
    for b in &page.rows {
        let raw = b
            .git_branch
            .strip_prefix("refs/heads/")
            .unwrap_or(&b.git_branch);
        let display = display_dimension(BreakdownView::Branches, raw);
        let label = truncate_label_middle(&display, label_width);
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
            "  {bold}{:<label_w$}{reset} {cyan}{}{reset} {yellow}{:>cost_w$}{reset}  {dim}{:<REPO_WIDTH$}{reset}",
            label,
            bar,
            format_cost_cents_fixed(b.cost_cents),
            repo,
            label_w = label_width,
            cost_w = BREAKDOWN_COST_WIDTH,
        );
    }

    render_breakdown_footer(&page, label_width, rule_width);
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
        super::print_json(&result)?;
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
            println!("  Run `budi stats branches` to see available branches.");
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
    label_width: usize,
    json_output: bool,
) -> Result<()> {
    let (since, until) = period_date_range(period);
    let page = client.tickets(since.as_deref(), until.as_deref(), limit)?;

    if json_output {
        super::print_json(&page)?;
        return Ok(());
    }

    let period_label = period_label(period);

    let bold_cyan = ansi("\x1b[1;36m");
    let bold = ansi("\x1b[1m");
    let dim = ansi("\x1b[90m");
    let cyan = ansi("\x1b[36m");
    let yellow = ansi("\x1b[33m");
    let reset = ansi("\x1b[0m");

    const SOURCE_WIDTH: usize = 18;
    // TOP_BRANCH uses the same middle-ellipsis truncation as the main
    // label column; keep its column width pinned to `label_width` so
    // long branch names don't spill into adjacent columns (#450 B).
    let branch_width = label_width;
    let rule_width = label_width
        + 1
        + BREAKDOWN_BAR_WIDTH
        + 1
        + BREAKDOWN_COST_WIDTH
        + 2
        + SOURCE_WIDTH
        + 2
        + branch_width;
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

    if is_only_untagged(&page.rows, |t| &t.ticket_id) && page.other.is_none() {
        render_untagged_only_empty_state(BreakdownView::Tickets, period);
        return Ok(());
    }

    print_breakdown_header(
        "TICKET",
        label_width,
        &format!(
            "{:<SOURCE_WIDTH$}  {:<branch_w$}",
            "SOURCE",
            "TOP_BRANCH",
            branch_w = branch_width,
        ),
    );

    let max_cost = max_cost_for_rows(&page.rows);
    for t in &page.rows {
        let bar = render_bar(t.cost_cents, max_cost);
        let label = truncate_label_middle(
            &display_dimension(BreakdownView::Tickets, &t.ticket_id),
            label_width,
        );
        let branch_label = if t.top_branch.is_empty() {
            "--".to_string()
        } else {
            truncate_label_middle(&t.top_branch, branch_width)
        };
        let source_label = if t.source.is_empty() {
            "--".to_string()
        } else {
            format!("src={}", t.source)
        };
        println!(
            "  {bold}{:<label_w$}{reset} {cyan}{}{reset} {yellow}{:>cost_w$}{reset}  {dim}{:<SOURCE_WIDTH$}{reset}  {dim}{:<branch_w$}{reset}",
            label,
            bar,
            format_cost_cents_fixed(t.cost_cents),
            source_label,
            branch_label,
            label_w = label_width,
            branch_w = branch_width,
            cost_w = BREAKDOWN_COST_WIDTH,
        );
    }

    render_breakdown_footer(&page, label_width, rule_width);
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
        super::print_json(&result)?;
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
            println!("  Run `budi stats tickets` to see available tickets.");
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
    label_width: usize,
    json_output: bool,
) -> Result<()> {
    let (since, until) = period_date_range(period);
    let page = client.activities(since.as_deref(), until.as_deref(), limit)?;

    if json_output {
        super::print_json(&page)?;
        return Ok(());
    }

    let period_label = period_label(period);

    let bold_cyan = ansi("\x1b[1;36m");
    let bold = ansi("\x1b[1m");
    let dim = ansi("\x1b[90m");
    let cyan = ansi("\x1b[36m");
    let yellow = ansi("\x1b[33m");
    let reset = ansi("\x1b[0m");

    const CONF_WIDTH: usize = 11;
    let branch_width = label_width;
    let rule_width = label_width
        + 1
        + BREAKDOWN_BAR_WIDTH
        + 1
        + BREAKDOWN_COST_WIDTH
        + 2
        + CONF_WIDTH
        + 2
        + branch_width;
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

    if is_only_untagged(&page.rows, |a| &a.activity) && page.other.is_none() {
        render_untagged_only_empty_state(BreakdownView::Activities, period);
        return Ok(());
    }

    print_breakdown_header(
        "ACTIVITY",
        label_width,
        &format!(
            "{:<CONF_WIDTH$}  {:<branch_w$}",
            "CONFIDENCE",
            "TOP_BRANCH",
            branch_w = branch_width,
        ),
    );

    let max_cost = max_cost_for_rows(&page.rows);
    for a in &page.rows {
        let bar = render_bar(a.cost_cents, max_cost);
        let label = truncate_label_middle(
            &display_dimension(BreakdownView::Activities, &a.activity),
            label_width,
        );
        let branch_label = if a.top_branch.is_empty() {
            "--".to_string()
        } else {
            truncate_label_middle(&a.top_branch, branch_width)
        };
        let confidence_label = if a.confidence.is_empty() {
            "--".to_string()
        } else {
            format!("conf={}", a.confidence)
        };
        println!(
            "  {bold}{:<label_w$}{reset} {cyan}{}{reset} {yellow}{:>cost_w$}{reset}  {dim}{:<CONF_WIDTH$}{reset}  {dim}{:<branch_w$}{reset}",
            label,
            bar,
            format_cost_cents_fixed(a.cost_cents),
            confidence_label,
            branch_label,
            label_w = label_width,
            branch_w = branch_width,
            cost_w = BREAKDOWN_COST_WIDTH,
        );
    }

    render_breakdown_footer(&page, label_width, rule_width);
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
        super::print_json(&result)?;
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
            println!("  Run `budi stats activities` to see available activities.");
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
    label_width: usize,
    json_output: bool,
) -> Result<()> {
    let (since, until) = period_date_range(period);
    let page = client.files(since.as_deref(), until.as_deref(), limit)?;

    if json_output {
        super::print_json(&page)?;
        return Ok(());
    }

    let period_label = period_label(period);

    let bold_cyan = ansi("\x1b[1;36m");
    let bold = ansi("\x1b[1m");
    let dim = ansi("\x1b[90m");
    let cyan = ansi("\x1b[36m");
    let yellow = ansi("\x1b[33m");
    let reset = ansi("\x1b[0m");

    const SOURCE_WIDTH: usize = 16;
    const TICKET_WIDTH: usize = 14;
    let rule_width = label_width
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

    if is_only_untagged(&page.rows, |f| &f.file_path) && page.other.is_none() {
        render_untagged_only_empty_state(BreakdownView::Files, period);
        return Ok(());
    }

    print_breakdown_header(
        "FILE",
        label_width,
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
            truncate_label_middle(&f.top_ticket_id, TICKET_WIDTH)
        };
        let source_label = if f.source.is_empty() {
            "--".to_string()
        } else {
            format!("src={}", f.source)
        };
        // Truncate very long paths so the row stays readable in narrow
        // terminals. Full paths remain visible via `--file <PATH>` and
        // `--format json`.
        let path_label = truncate_label_middle(
            &display_dimension(BreakdownView::Files, &f.file_path),
            label_width,
        );
        println!(
            "  {bold}{:<label_w$}{reset} {cyan}{}{reset} {yellow}{:>cost_w$}{reset}  {dim}{:<SOURCE_WIDTH$}{reset}  {dim}{:<TICKET_WIDTH$}{reset}",
            path_label,
            bar,
            format_cost_cents_fixed(f.cost_cents),
            source_label,
            ticket_label,
            label_w = label_width,
            cost_w = BREAKDOWN_COST_WIDTH,
        );
    }

    render_breakdown_footer(&page, label_width, rule_width);
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
        super::print_json(&result)?;
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
            println!("  Run `budi stats files` to see available files.");
        }
    }

    println!();
    Ok(())
}

fn cmd_stats_models(
    client: &DaemonClient,
    period: StatsPeriod,
    limit: usize,
    label_width: usize,
    include_pending: bool,
    json_output: bool,
) -> Result<()> {
    let (since, until) = period_date_range(period);
    let page = client.models(since.as_deref(), until.as_deref(), limit)?;

    if json_output {
        // #443 acceptance: JSON exposes the Budi-canonical `display_name`
        // and `effort_modifier` alongside the raw provider-emitted
        // `model` / `provider_model_id`. `model` is preserved so
        // existing scripting callers that read it continue to work; new
        // callers can filter on `display_name` for cross-provider
        // aggregation.
        print_models_json(&page)?;
        return Ok(());
    }

    let period_label = period_label(period);

    let bold_cyan = ansi("\x1b[1;36m");
    let bold = ansi("\x1b[1m");
    let dim = ansi("\x1b[90m");
    let cyan = ansi("\x1b[36m");
    let yellow = ansi("\x1b[33m");
    let reset = ansi("\x1b[0m");

    const MSGS_WIDTH: usize = 10;
    const TOK_WIDTH: usize = 10;
    let rule_width = label_width
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

    // #443 acceptance: Cursor's `default` (Auto routing) and the
    // `(untagged)` sentinel collapse into a single
    // `(model not yet attributed)` bucket per provider — both mean
    // "we don't have a specific model id for this cost". Summed
    // per-provider so the grand-total footer from #448 still
    // reconciles to the cent.
    //
    // #450 acceptance carry-forward: a merged bucket whose cost is
    // zero is treated as a pending transient (the pure-`(untagged)`
    // case with no backing `default` spend) and suppressed by
    // default; `--include-pending` keeps it visible.
    let (render_rows, suppressed_pending) =
        merge_and_partition_pending(&page.rows, include_pending);

    if render_rows.is_empty() && page.other.is_none() && suppressed_pending > 0 {
        render_untagged_only_empty_state(BreakdownView::Models, period);
        println!(
            "  {dim}* {} model row{} pending — Cursor lag (pass --include-pending to see){reset}",
            suppressed_pending,
            if suppressed_pending == 1 { "" } else { "s" },
        );
        println!();
        return Ok(());
    }

    print_breakdown_header(
        "MODEL",
        label_width,
        &format!("{:>MSGS_WIDTH$}  {:>TOK_WIDTH$}", "MSGS", "TOKENS"),
    );

    let has_duplicate_display = {
        let mut seen = std::collections::HashSet::new();
        render_rows
            .iter()
            .any(|r| !seen.insert(r.display_label.clone()))
    };

    // #449 fix: bars scale by cost (the column they sit next to), not by
    // message count. A $66 row no longer renders with more blocks than a
    // $548 row just because it crossed a provider-specific high-volume
    // threshold.
    let max_cost = render_rows
        .iter()
        .map(|r| r.cost_cents)
        .fold(0.0_f64, f64::max);
    for r in &render_rows {
        let bar = render_bar(r.cost_cents, max_cost);
        let total_tok =
            r.input_tokens + r.output_tokens + r.cache_read_tokens + r.cache_creation_tokens;
        let raw_label = if has_duplicate_display {
            format!("{} ({})", r.display_label, r.provider)
        } else {
            r.display_label.clone()
        };
        let label = truncate_label_middle(&raw_label, label_width);
        let msgs_cell = format!("{} msgs", r.message_count);
        let tok_cell = format!("{} tok", format_tokens(total_tok));
        println!(
            "  {bold}{:<label_w$}{reset} {cyan}{}{reset} {yellow}{:>cost_w$}{reset}  {dim}{:>MSGS_WIDTH$}{reset}  {dim}{:>TOK_WIDTH$}{reset}",
            label,
            bar,
            format_cost_cents_fixed(r.cost_cents),
            msgs_cell,
            tok_cell,
            label_w = label_width,
            cost_w = BREAKDOWN_COST_WIDTH,
        );
    }

    render_breakdown_footer(&page, label_width, rule_width);
    if suppressed_pending > 0 {
        println!(
            "  {dim}* {} model row{} pending — Cursor lag (pass --include-pending to see){reset}",
            suppressed_pending,
            if suppressed_pending == 1 { "" } else { "s" },
        );
        println!();
    }
    Ok(())
}

/// One row slated for text rendering in the `--models` view, after
/// placeholder merge (#443) and pending suppression (#450) have run.
#[derive(Debug, Clone)]
struct RenderModelRow {
    display_label: String,
    provider: String,
    message_count: u64,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    cost_cents: f64,
}

/// Build the set of rows the text view should render, applying both
/// the #443 placeholder merge and the #450 pending-suppression rule.
///
/// - Real rows (`Placeholder::None`) pass through as one
///   [`RenderModelRow`] each, keyed on the resolved display label
///   (display_name + optional effort).
/// - Placeholder rows (`Placeholder::CursorAuto` /
///   `Placeholder::NotAttributed`) merge per-provider into a single
///   `(model not yet attributed)` row. Costs, tokens, and message
///   counts are summed.
/// - A merged row whose cost is exactly zero is treated as a pending
///   transient and suppressed unless `include_pending` is set. The
///   suppressed-row count flows to the footnote.
///
/// Rows are returned in descending-cost order so the bar scale cue
/// keeps meaning after the merge.
fn merge_and_partition_pending(
    rows: &[budi_core::analytics::ModelUsage],
    include_pending: bool,
) -> (Vec<RenderModelRow>, usize) {
    use std::collections::HashMap;

    let mut real: Vec<RenderModelRow> = Vec::with_capacity(rows.len());
    // Keyed on provider so Cursor's pending rows and (hypothetically)
    // Claude-Code's pending rows render as separate buckets.
    let mut placeholder_by_provider: HashMap<String, RenderModelRow> = HashMap::new();

    for m in rows {
        let d = display::resolve(&m.model);
        match d.placeholder {
            Placeholder::None => {
                real.push(RenderModelRow {
                    display_label: d.combined_label(),
                    provider: m.provider.clone(),
                    message_count: m.message_count,
                    input_tokens: m.input_tokens,
                    output_tokens: m.output_tokens,
                    cache_read_tokens: m.cache_read_tokens,
                    cache_creation_tokens: m.cache_creation_tokens,
                    cost_cents: m.cost_cents,
                });
            }
            Placeholder::CursorAuto | Placeholder::NotAttributed => {
                let entry = placeholder_by_provider
                    .entry(m.provider.clone())
                    .or_insert_with(|| RenderModelRow {
                        display_label: display::UNATTRIBUTED_LABEL.to_string(),
                        provider: m.provider.clone(),
                        message_count: 0,
                        input_tokens: 0,
                        output_tokens: 0,
                        cache_read_tokens: 0,
                        cache_creation_tokens: 0,
                        cost_cents: 0.0,
                    });
                entry.message_count += m.message_count;
                entry.input_tokens += m.input_tokens;
                entry.output_tokens += m.output_tokens;
                entry.cache_read_tokens += m.cache_read_tokens;
                entry.cache_creation_tokens += m.cache_creation_tokens;
                entry.cost_cents += m.cost_cents;
            }
        }
    }

    let mut suppressed_pending = 0usize;
    let mut merged: Vec<RenderModelRow> = real;
    for (_, row) in placeholder_by_provider {
        if !include_pending && row.cost_cents <= 0.0 {
            suppressed_pending += 1;
            continue;
        }
        merged.push(row);
    }

    // Descending cost keeps the bar scale meaningful after the merge
    // reordered things.
    merged.sort_by(|a, b| {
        b.cost_cents
            .partial_cmp(&a.cost_cents)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    (merged, suppressed_pending)
}

/// Enriched row emitted by `--models --format json` alongside the
/// existing ModelUsage fields. `display_name` and `effort_modifier`
/// are sourced from the #443 Budi display overlay (`pricing::display`);
/// `provider_model_id` duplicates `model` so the acceptance criterion
/// "JSON exposes both display_name and the raw provider_model_id"
/// is satisfied without renaming the long-standing `model` key.
#[derive(serde::Serialize)]
struct EnrichedModelRow<'a> {
    model: &'a str,
    provider_model_id: &'a str,
    display_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    effort_modifier: Option<String>,
    placeholder: &'static str,
    provider: &'a str,
    message_count: u64,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    cost_cents: f64,
}

/// Enriched page envelope. Keeps the `other` / `total_cost_cents` /
/// `total_rows` / `shown_rows` / `limit` fields in the exact shape #448
/// locked down so reconciliation consumers are untouched.
#[derive(serde::Serialize)]
struct EnrichedModelPage<'a> {
    rows: Vec<EnrichedModelRow<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    other: &'a Option<budi_core::analytics::BreakdownOther>,
    total_cost_cents: f64,
    total_rows: usize,
    shown_rows: usize,
    limit: usize,
}

/// Build the enriched page — shared between the CLI JSON output and
/// the shape test.
fn build_models_json_page<'a>(
    page: &'a BreakdownPage<budi_core::analytics::ModelUsage>,
) -> EnrichedModelPage<'a> {
    let rows: Vec<EnrichedModelRow<'_>> = page
        .rows
        .iter()
        .map(|m| {
            let d = display::resolve(&m.model);
            EnrichedModelRow {
                model: &m.model,
                provider_model_id: &m.model,
                display_name: d.display_name,
                effort_modifier: d.effort,
                placeholder: placeholder_tag(d.placeholder),
                provider: &m.provider,
                message_count: m.message_count,
                input_tokens: m.input_tokens,
                output_tokens: m.output_tokens,
                cache_read_tokens: m.cache_read_tokens,
                cache_creation_tokens: m.cache_creation_tokens,
                cost_cents: m.cost_cents,
            }
        })
        .collect();

    EnrichedModelPage {
        rows,
        other: &page.other,
        total_cost_cents: page.total_cost_cents,
        total_rows: page.total_rows,
        shown_rows: page.shown_rows,
        limit: page.limit,
    }
}

/// Serialize a `--models` page with the #443 display-name enrichment:
/// every row grows `display_name`, `effort_modifier`, and
/// `provider_model_id` (alias for `model`) alongside the existing
/// fields.
fn print_models_json(page: &BreakdownPage<budi_core::analytics::ModelUsage>) -> Result<()> {
    let enriched = build_models_json_page(page);
    super::print_json(&enriched)
}

#[cfg(test)]
fn serde_models_page_for_test(
    page: &BreakdownPage<budi_core::analytics::ModelUsage>,
) -> serde_json::Value {
    serde_json::to_value(build_models_json_page(page)).unwrap()
}

/// Serialize a [`Placeholder`] as a short tag used by the `--models`
/// JSON envelope. Real rows carry `"none"` so consumers can filter on
/// a single field without checking for key presence.
fn placeholder_tag(p: Placeholder) -> &'static str {
    match p {
        Placeholder::None => "none",
        Placeholder::CursorAuto => "cursor_auto",
        Placeholder::NotAttributed => "not_attributed",
    }
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
            format_cost_cents_fixed(other.cost_cents),
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
        format_cost_cents_fixed(page.total_cost_cents),
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

/// Fixed-precision dollar formatting for breakdown tables: always
/// `$X,XXX.XX` with thousands separators and two decimal places, so
/// every row in a breakdown column shares one visual shape. This is
/// the #450 acceptance for "Single currency format per column type" —
/// the summary view keeps the humanized `$1.2K` form via
/// [`format_cost_cents`]; breakdowns use this one.
pub fn format_cost_cents_fixed(cents: f64) -> String {
    let dollars = cents / 100.0;
    let sign = if dollars < 0.0 { "-" } else { "" };
    let magnitude = dollars.abs();
    let whole = magnitude.trunc() as u64;
    let frac = ((magnitude - whole as f64) * 100.0).round() as u64;
    // Defensive: rounding can push the fractional part to 100; carry
    // the one into `whole` so "$0.995" renders as "$1.00" instead of
    // "$0.100".
    let (whole, frac) = if frac >= 100 {
        (whole + 1, frac - 100)
    } else {
        (whole, frac)
    };
    let whole_str = insert_thousands_separator(whole);
    format!("{sign}${whole_str}.{frac:02}")
}

fn insert_thousands_separator(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    let bytes = digits.as_bytes();
    let len = bytes.len();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// Identity of the breakdown view a row is being rendered for. Used
/// to translate the DB's generic `(untagged)` dimension value into a
/// view-specific label — a branch-less message means "no branch", a
/// ticket-less message means "no ticket", etc. (#450 acceptance E)
#[derive(Clone, Copy, PartialEq, Eq)]
enum BreakdownView {
    Projects,
    Branches,
    Tickets,
    Activities,
    Files,
    Models,
    Tag,
}

impl BreakdownView {
    /// Render-time replacement for the `(untagged)` sentinel stored in
    /// the DB. Tag view keeps `(untagged)` verbatim since the key
    /// meaning is user-defined.
    fn untagged_label(self) -> &'static str {
        match self {
            BreakdownView::Projects => "(no repository)",
            BreakdownView::Branches => "(no branch)",
            BreakdownView::Tickets => "(no ticket)",
            BreakdownView::Activities => "(unclassified)",
            BreakdownView::Files => "(no file tag)",
            BreakdownView::Models => "(model not yet attributed)",
            BreakdownView::Tag => "(untagged)",
        }
    }
}

/// Returns `true` when `value` is the DB's generic `(untagged)`
/// sentinel. Kept in one place so renaming the sentinel (unlikely)
/// touches a single site.
fn is_untagged(value: &str) -> bool {
    value == budi_core::analytics::UNTAGGED_DIMENSION
}

/// Translate `(untagged)` into the view-specific label when rendering
/// a row; every other value passes through unchanged.
fn display_dimension(view: BreakdownView, value: &str) -> String {
    if is_untagged(value) {
        view.untagged_label().to_string()
    } else {
        value.to_string()
    }
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
    label_width: usize,
    json_output: bool,
) -> Result<()> {
    let (since, until) = period_date_range(period);

    let page = client.tags(Some(tag_filter), since.as_deref(), until.as_deref(), limit)?;

    if json_output {
        super::print_json(&page)?;
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

    let rule_width = label_width + 1 + BREAKDOWN_BAR_WIDTH + 1 + BREAKDOWN_COST_WIDTH;
    println!();
    println!(
        "  {bold_cyan} Tag: {}{reset} — {bold}{}{reset}",
        tag_filter,
        period_label(period)
    );
    println!("  {dim}{}{reset}", "─".repeat(rule_width));

    if is_only_untagged(&page.rows, |t| &t.value) && page.other.is_none() {
        render_untagged_only_empty_state(BreakdownView::Tag, period);
        return Ok(());
    }

    print_breakdown_header("VALUE", label_width, "");

    let max_cost = max_cost_for_rows(&page.rows);
    for tag in &page.rows {
        let bar = render_bar(tag.cost_cents, max_cost);
        let label = truncate_label_middle(&tag.value, label_width);
        println!(
            "  {bold}{:<label_w$}{reset} {cyan}{}{reset} {yellow}{:>cost_w$}{reset}",
            label,
            bar,
            format_cost_cents_fixed(tag.cost_cents),
            label_w = label_width,
            cost_w = BREAKDOWN_COST_WIDTH,
        );
    }
    render_breakdown_footer(&page, label_width, rule_width);
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
        // Snapshot baseline for `budi stats models` row layout
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
        // Golden snapshot for `budi stats tickets` — pinned so the
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
        // Golden snapshot for `budi stats activities`. Preserves the
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
        // Golden snapshot for `budi stats tag <key>` — the layout
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

    // ─── #451 summary-shape tests ────────────────────────────────────

    /// Build a representative `UsageSummary` for the summary-shape
    /// tests. Numbers are arbitrary but non-zero so the formatter
    /// exercises every code path.
    fn fixture_summary() -> budi_core::analytics::UsageSummary {
        budi_core::analytics::UsageSummary {
            total_messages: 262,
            total_user_messages: 16,
            total_assistant_messages: 246,
            total_input_tokens: 1_300_000,
            total_output_tokens: 874_100,
            total_cache_creation_tokens: 50_000,
            total_cache_read_tokens: 200_000,
            total_cost_cents: 11_400.0,
        }
    }

    fn fixture_cost(cache_savings: f64) -> budi_core::cost::CostEstimate {
        budi_core::cost::CostEstimate {
            total_cost: 114.0,
            input_cost: 4.49,
            output_cost: 20.82,
            cache_write_cost: 27.48,
            cache_read_cost: 65.94,
            other_cost: 0.0,
            cache_savings,
        }
    }

    fn fixture_provider(
        provider: &str,
        display: &str,
        msgs: u64,
        cents: f64,
    ) -> analytics::ProviderStats {
        // #482: `msgs` is interpreted as the assistant-side count (same
        // unit every pre-8.3.1 caller passed). The fixture picks a
        // user_messages value slightly larger than assistant so
        // `total_messages = assistant + user` stays unambiguous across
        // snapshot tests.
        let user = msgs.saturating_add(3);
        analytics::ProviderStats {
            provider: provider.into(),
            display_name: display.into(),
            assistant_messages: msgs,
            user_messages: user,
            total_messages: msgs + user,
            input_tokens: 100_000,
            output_tokens: 50_000,
            cache_creation_tokens: 10_000,
            cache_read_tokens: 20_000,
            estimated_cost: cents / 100.0,
            total_cost_cents: cents,
        }
    }

    /// All blocks that #451 requires to be present in every summary,
    /// regardless of period or provider count.
    fn assert_summary_has_required_blocks(rendered: &str, expect_cursor_footnote: bool) {
        assert!(
            rendered.contains("budi stats"),
            "missing header line:\n{rendered}"
        );
        assert!(
            rendered.contains("Agents"),
            "missing Agents block (#451):\n{rendered}"
        );
        assert!(
            rendered.contains("Total"),
            "missing Total line:\n{rendered}"
        );
        assert!(
            rendered.contains("Tokens"),
            "missing Tokens line:\n{rendered}"
        );
        assert!(
            rendered.contains("Est. cost"),
            "missing Est. cost line:\n{rendered}"
        );
        assert!(
            rendered.contains("input ") && rendered.contains("output "),
            "missing cost-component sub-line (#451):\n{rendered}"
        );
        assert!(
            rendered.contains("cache write") && rendered.contains("cache read"),
            "missing cost-component sub-line cache cells (#451):\n{rendered}"
        );
        assert!(
            rendered.contains("cache savings"),
            "missing cache-savings line (#451 — must be unconditional):\n{rendered}"
        );
        if expect_cursor_footnote {
            assert!(
                rendered.contains("Cursor cost data may lag"),
                "missing Cursor-lag footnote when Cursor is in providers (#451):\n{rendered}"
            );
        }
    }

    #[test]
    fn summary_tokens_row_reconciles_with_per_agent_totals() {
        // #445 item 5: the per-agent rows in the Agents block render a
        // token total that *includes* cache traffic (input + output +
        // cache-write + cache-read). Before this fix the summary
        // Tokens row only showed `{input} in, {output} out`, which
        // looked like it disagreed with the per-agent row on cache-heavy
        // workloads. Lock in the four-component shape so a future
        // renderer change cannot silently regress the reconciliation.
        let summary = fixture_summary();
        let est = fixture_cost(0.0);
        let providers = vec![fixture_provider(
            "claude_code",
            "Claude Code",
            262,
            summary.total_cost_cents,
        )];
        let palette = SummaryPalette::plain();
        let rendered = format_summary(
            StatsPeriod::Days(7),
            None,
            &summary,
            &est,
            &providers,
            &palette,
        );

        // Every token component must appear on the Tokens line (not
        // just input / output).
        assert!(
            rendered.contains("input"),
            "summary missing input label:\n{rendered}"
        );
        assert!(
            rendered.contains("output"),
            "summary missing output label:\n{rendered}"
        );
        assert!(
            rendered.contains("cache-write"),
            "summary missing cache-write component (#445 reconciliation):\n{rendered}"
        );
        assert!(
            rendered.contains("cache-read"),
            "summary missing cache-read component (#445 reconciliation):\n{rendered}"
        );

        // The pre-#445 shape `<n> in, <n> out` must be gone so consumers
        // can't rely on it and readers never see it.
        for line in rendered.lines() {
            if line.trim_start().starts_with("Tokens") {
                assert!(
                    !line.contains(" in, "),
                    "legacy two-component Tokens line still renders: {line}"
                );
            }
        }
    }

    #[test]
    fn summary_shape_is_identical_across_all_six_periods() {
        // #451 acceptance: `today / week / month / 1d / 7d / 30d` all
        // produce the same set of blocks (header, Agents, Total,
        // Tokens, Est. cost, cost-component sub-line, cache savings,
        // optional Cursor-lag footnote). Pre-#451, today and week
        // dropped the Agents block whenever the window happened to
        // surface a single provider.
        let summary = fixture_summary();
        let est = fixture_cost(593.0);
        let providers = vec![
            fixture_provider("codex", "Codex", 566, 13_700.0),
            fixture_provider("cursor", "Cursor", 280, 16_300.0),
            fixture_provider("claude_code", "Claude Code", 3, 72.0),
        ];
        let palette = SummaryPalette::plain();

        for period in [
            StatsPeriod::Today,
            StatsPeriod::Week,
            StatsPeriod::Month,
            StatsPeriod::Days(1),
            StatsPeriod::Days(7),
            StatsPeriod::Days(30),
        ] {
            let rendered = format_summary(period, None, &summary, &est, &providers, &palette);
            assert_summary_has_required_blocks(&rendered, true);
            // Each provider row must show up.
            for ps in &providers {
                assert!(
                    rendered.contains(&ps.display_name),
                    "{period:?}: missing provider {} row in Agents block:\n{rendered}",
                    ps.display_name
                );
            }
        }
    }

    #[test]
    fn summary_keeps_agents_block_with_one_provider() {
        // #451 primary fix: even with a single provider in the window,
        // the Agents block renders. Pre-#451 the dispatcher dropped
        // it entirely when `providers.len() == 1`, making `today` look
        // structurally different from `7d` on the same machine.
        let summary = fixture_summary();
        let est = fixture_cost(0.0);
        let providers = vec![fixture_provider(
            "claude_code",
            "Claude Code",
            262,
            11_400.0,
        )];
        let palette = SummaryPalette::plain();

        let rendered = format_summary(
            StatsPeriod::Today,
            None,
            &summary,
            &est,
            &providers,
            &palette,
        );
        assert!(
            rendered.contains("Agents"),
            "Agents block must render with a single provider (#451):\n{rendered}"
        );
        assert!(
            rendered.contains("Claude Code"),
            "the single provider's row must render:\n{rendered}"
        );
        // Cursor-lag footnote is OFF when Cursor is not in the window.
        assert!(
            !rendered.contains("Cursor cost data may lag"),
            "Cursor-lag footnote must NOT render when Cursor is absent:\n{rendered}"
        );
    }

    #[test]
    fn summary_cache_savings_line_is_unconditional() {
        // #451 acceptance: cache-savings line is part of the
        // summary's structural shape, even when the value is $0.00.
        // Otherwise the line's presence becomes a leaked signal of
        // whether any cache hits accumulated, and the summary's shape
        // varies between Today and 7d.
        let summary = fixture_summary();
        let providers = vec![fixture_provider(
            "claude_code",
            "Claude Code",
            262,
            11_400.0,
        )];
        let palette = SummaryPalette::plain();

        let with_savings = format_summary(
            StatsPeriod::Today,
            None,
            &summary,
            &fixture_cost(593.0),
            &providers,
            &palette,
        );
        assert!(with_savings.contains("cache savings $593"));

        let no_savings = format_summary(
            StatsPeriod::Today,
            None,
            &summary,
            &fixture_cost(0.0),
            &providers,
            &palette,
        );
        assert!(
            no_savings.contains("cache savings $0.00"),
            "cache-savings line must render even when zero:\n{no_savings}"
        );
    }

    #[test]
    fn summary_provider_filter_renders_one_row_in_agents_block() {
        // `--provider cursor` keeps the Agents block intact but
        // narrows it to the selected agent. The Cursor-lag footnote
        // also stays because cursor is one of the displayed
        // providers.
        let summary = fixture_summary();
        let est = fixture_cost(0.0);
        let providers = vec![
            fixture_provider("codex", "Codex", 566, 13_700.0),
            fixture_provider("cursor", "Cursor", 280, 16_300.0),
        ];
        let palette = SummaryPalette::plain();

        let rendered = format_summary(
            StatsPeriod::Today,
            Some("cursor"),
            &summary,
            &est,
            &providers,
            &palette,
        );
        assert_summary_has_required_blocks(&rendered, true);
        assert!(rendered.contains("Cursor"));
        assert!(
            !rendered.contains("Codex"),
            "filtered Agents block must hide non-matching providers:\n{rendered}"
        );
    }

    #[test]
    fn summary_empty_window_renders_no_data_message_and_skips_blocks() {
        // The "no data" early return is preserved: empty windows
        // print a friendly message instead of an Agents block with
        // a "(no provider data)" placeholder. This keeps the shape
        // contract scoped to non-empty windows.
        let mut summary = fixture_summary();
        summary.total_messages = 0;
        summary.total_user_messages = 0;
        summary.total_assistant_messages = 0;
        let est = fixture_cost(0.0);
        let providers: Vec<analytics::ProviderStats> = vec![];
        let palette = SummaryPalette::plain();

        let rendered = format_summary(
            StatsPeriod::Today,
            None,
            &summary,
            &est,
            &providers,
            &palette,
        );
        assert!(rendered.contains("No data for this period"));
        assert!(
            !rendered.contains("Agents"),
            "empty window must not print the Agents block"
        );
        assert!(
            !rendered.contains("Est. cost"),
            "empty window must not print the cost block"
        );
    }

    #[test]
    fn summary_cursor_lag_footnote_rides_on_displayed_providers() {
        // #451: the Cursor-lag footnote prints whenever the displayed
        // providers list includes Cursor, regardless of whether the
        // user passed `--provider cursor`. Pre-#451 the
        // summary-filtered path only printed it on explicit filter
        // and the multi-agent path silently included a Cursor row
        // without the caveat — both regressions are fixed by sharing
        // one renderer.
        let summary = fixture_summary();
        let est = fixture_cost(0.0);
        let palette = SummaryPalette::plain();

        // Cursor present in window, no filter → footnote on.
        let providers = vec![
            fixture_provider("codex", "Codex", 1, 100.0),
            fixture_provider("cursor", "Cursor", 1, 100.0),
        ];
        let r = format_summary(
            StatsPeriod::Today,
            None,
            &summary,
            &est,
            &providers,
            &palette,
        );
        assert!(r.contains("Cursor cost data may lag"));

        // Filtered to codex → footnote off (Cursor row not displayed).
        let r = format_summary(
            StatsPeriod::Today,
            Some("codex"),
            &summary,
            &est,
            &providers,
            &palette,
        );
        assert!(!r.contains("Cursor cost data may lag"));

        // No Cursor in window at all → footnote off.
        let providers_no_cursor = vec![fixture_provider("codex", "Codex", 1, 100.0)];
        let r = format_summary(
            StatsPeriod::Today,
            None,
            &summary,
            &est,
            &providers_no_cursor,
            &palette,
        );
        assert!(!r.contains("Cursor cost data may lag"));
    }

    #[test]
    fn summary_agents_block_tokens_cell_always_has_unit_suffix() {
        // #494: every tokens cell in the Agents block carries an
        // explicit `tok` suffix — the pre-8.3.1 render was `0` (bare)
        // next to `159.0M` (with its unit baked in), leaving a fresh
        // reader unsure which was tokens and which was a count. Both
        // rows now render `{n} tok`.
        let summary = fixture_summary();
        let est = fixture_cost(0.0);
        // Mix zero-token row (typical for Cursor) with a non-zero
        // row (typical for Claude Code).
        let zero_provider = analytics::ProviderStats {
            provider: "cursor".into(),
            display_name: "Cursor".into(),
            assistant_messages: 12,
            user_messages: 10,
            total_messages: 22,
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            estimated_cost: 0.0,
            total_cost_cents: 0.0,
        };
        let providers = vec![
            fixture_provider("claude_code", "Claude Code", 262, 11_400.0),
            zero_provider,
        ];
        let palette = SummaryPalette::plain();
        let rendered = format_summary(
            StatsPeriod::Today,
            None,
            &summary,
            &est,
            &providers,
            &palette,
        );

        // Both rows carry the explicit ` tok` suffix.
        for line in rendered.lines() {
            let trimmed = line.trim();
            // Agents rows start with a provider display name after the
            // leading spaces of the block.
            if trimmed.starts_with("Claude Code") || trimmed.starts_with("Cursor") {
                assert!(
                    trimmed.contains(" tok"),
                    "Agents-block row must include the `tok` suffix: {trimmed:?}",
                );
            }
        }
    }

    #[test]
    fn summary_est_cost_precision_matches_component_sub_line() {
        // #486: summary `Est. cost` precision matches the component
        // sub-line precision (`$X,XXX.XX`). Pre-8.3.1 a top-line total
        // above $100 collapsed to `$126` while the sub-line kept
        // `$0.04 / $19.92 / $32.36 / $74.08` → sum `$126.40`. The
        // fresh reader couldn't tell whether the top value was
        // `$126.00` rounded or a sub-line total that silently lost
        // 40¢. Both lines now use `format_cost_cents_fixed`.
        let summary = fixture_summary();
        let est = budi_core::cost::CostEstimate {
            total_cost: 126.40,
            input_cost: 0.04,
            output_cost: 19.92,
            cache_write_cost: 32.36,
            cache_read_cost: 74.08,
            other_cost: 0.0,
            cache_savings: 0.0,
        };
        let providers = vec![fixture_provider(
            "claude_code",
            "Claude Code",
            262,
            12_640.0,
        )];
        let palette = SummaryPalette::plain();
        let rendered = format_summary(
            StatsPeriod::Today,
            None,
            &summary,
            &est,
            &providers,
            &palette,
        );

        // Top-line never collapses cents above $100.
        assert!(
            rendered.contains("Est. cost    $126.40"),
            "Est. cost must render `$126.40`, not `$126`:\n{rendered}",
        );
        // Component sub-line carries matching cents precision.
        assert!(
            rendered.contains("input $0.04"),
            "input component must render `$0.04`:\n{rendered}",
        );
        assert!(
            rendered.contains("output $19.92"),
            "output component must render `$19.92`:\n{rendered}",
        );
        // Top-line dollars never rendered with a `K` suffix in the
        // summary block — locking this out catches any future caller
        // that accidentally swaps in the humanized formatter.
        for line in rendered.lines() {
            if line.contains("Est. cost") {
                assert!(
                    !line.contains("K") && !line.contains("M"),
                    "top-line `Est. cost` must not humanize: {line:?}",
                );
            }
        }
    }

    /// #520: when `other_cost > 0`, the summary sub-line grows a
    /// fifth `other $N.NN` cell so the four base components + `other`
    /// sum to the top line. Fresh Claude-only windows (other_cost=0)
    /// keep the pre-8.3.2 four-cell sub-line so the surface stays
    /// uncluttered.
    #[test]
    fn summary_renders_other_cost_cell_when_nonzero() {
        let summary = fixture_summary();
        // Seed the 2026-04-22 audit's actual 30d numbers: top-line
        // \$3,915.47, components sum \$3,800.47, other = \$115.00.
        let est = budi_core::cost::CostEstimate {
            total_cost: 3_915.47,
            input_cost: 995.38,
            output_cost: 316.65,
            cache_write_cost: 703.15,
            cache_read_cost: 1_785.29,
            other_cost: 115.00,
            cache_savings: 0.0,
        };
        let providers = vec![fixture_provider(
            "claude_code",
            "Claude Code",
            262,
            391_547.0,
        )];
        let palette = SummaryPalette::plain();
        let rendered = format_summary(
            StatsPeriod::Today,
            None,
            &summary,
            &est,
            &providers,
            &palette,
        );
        assert!(
            rendered.contains("other $115.00"),
            "other cell must render when non-zero:\n{rendered}",
        );
        assert!(
            rendered.contains("Est. cost    $3,915.47"),
            "top-line cost precision must carry thousands sep + cents:\n{rendered}",
        );
    }

    #[test]
    fn summary_omits_other_cost_cell_when_zero() {
        let summary = fixture_summary();
        let est = budi_core::cost::CostEstimate {
            total_cost: 7.50,
            input_cost: 5.00,
            output_cost: 2.50,
            cache_write_cost: 0.0,
            cache_read_cost: 0.0,
            other_cost: 0.0,
            cache_savings: 0.0,
        };
        let providers = vec![fixture_provider("claude_code", "Claude Code", 10, 750.0)];
        let palette = SummaryPalette::plain();
        let rendered = format_summary(
            StatsPeriod::Today,
            None,
            &summary,
            &est,
            &providers,
            &palette,
        );
        // The "other" label only appears inside the summary block
        // when `other_cost > 0`; a zero window keeps the pre-8.3.2
        // four-cell shape.
        for line in rendered.lines() {
            if line.contains("input ") && line.contains("output ") {
                assert!(
                    !line.contains("other"),
                    "sub-line should not contain `other` when other_cost == 0: {line:?}",
                );
            }
        }
    }

    #[test]
    fn summary_agents_block_count_matches_total_row_unit() {
        // #482: the Agents block's `msgs` column is the same unit as
        // `Total {n} messages` (user + assistant), not the pre-8.3.1
        // assistant-only count. Without this, a fresh reader seeing
        // `Claude Code 1358 msgs` on top of `Total 3050 messages` has
        // no way to tell that `1358` undercounts by `total_user_messages`.
        //
        // Contract: the per-provider count printed in the Agents block
        // equals `ProviderStats.total_messages`, AND `sum(displayed
        // providers.total_messages) == summary.total_messages` when no
        // filter is active.
        let summary = budi_core::analytics::UsageSummary {
            total_messages: 3050,
            total_user_messages: 1680,
            total_assistant_messages: 1370,
            total_input_tokens: 1_300_000,
            total_output_tokens: 874_100,
            total_cache_creation_tokens: 50_000,
            total_cache_read_tokens: 200_000,
            total_cost_cents: 12_640.0,
        };
        let est = fixture_cost(0.0);
        // Hand-built providers so we can pin the exact split seen on
        // the 2026-04-22 audit machine.
        let claude = analytics::ProviderStats {
            provider: "claude_code".into(),
            display_name: "Claude Code".into(),
            assistant_messages: 1358,
            user_messages: 1670,
            total_messages: 3028,
            input_tokens: 1_300_000,
            output_tokens: 874_100,
            cache_creation_tokens: 50_000,
            cache_read_tokens: 200_000,
            estimated_cost: 126.40,
            total_cost_cents: 12_640.0,
        };
        let cursor = analytics::ProviderStats {
            provider: "cursor".into(),
            display_name: "Cursor".into(),
            assistant_messages: 12,
            user_messages: 10,
            total_messages: 22,
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            estimated_cost: 0.0,
            total_cost_cents: 0.0,
        };
        let providers = vec![claude.clone(), cursor.clone()];
        let palette = SummaryPalette::plain();

        let rendered = format_summary(
            StatsPeriod::Today,
            None,
            &summary,
            &est,
            &providers,
            &palette,
        );

        // The per-provider row renders `total_messages`, not
        // `assistant_messages`. Locking the exact substring prevents a
        // future renderer from quietly swapping back to assistant-only.
        assert!(
            rendered.contains(&format!("{} msgs", claude.total_messages)),
            "Claude Code row must render total_messages ({}), not assistant_messages ({}):\n{rendered}",
            claude.total_messages,
            claude.assistant_messages,
        );
        assert!(
            rendered.contains(&format!("{} msgs", cursor.total_messages)),
            "Cursor row must render total_messages ({}):\n{rendered}",
            cursor.total_messages,
        );
        assert!(
            !rendered.contains(&format!("{} msgs", claude.assistant_messages)),
            "must not render assistant-only count ({}) in Agents column:\n{rendered}",
            claude.assistant_messages,
        );

        // Per-provider totals reconcile to the summary total to within
        // the sync-lag budget. Here the fixture is internally consistent
        // so the delta is exactly 0.
        let agg: u64 = providers.iter().map(|p| p.total_messages).sum();
        assert_eq!(
            agg, summary.total_messages,
            "sum of Agents-block counts ({}) must match Total row ({})",
            agg, summary.total_messages,
        );
    }

    // ─── #450 breakdown polish tests ──────────────────────────────────

    #[test]
    fn format_cost_cents_fixed_is_always_two_decimals_with_thousands_sep() {
        // #450 acceptance C: breakdown columns render one currency
        // shape — fixed `$X,XXX.XX`, never the humanized `$1.2K`. A
        // breakdown column with `$1.5K / $288 / $90.39 / $0.42` all
        // resolves to the same visual shape.
        assert_eq!(format_cost_cents_fixed(0.0), "$0.00");
        assert_eq!(format_cost_cents_fixed(42.0), "$0.42");
        assert_eq!(format_cost_cents_fixed(9_039.0), "$90.39");
        assert_eq!(format_cost_cents_fixed(28_800.0), "$288.00");
        assert_eq!(format_cost_cents_fixed(150_000.0), "$1,500.00");
        assert_eq!(format_cost_cents_fixed(1_234_567.0), "$12,345.67");
        assert_eq!(format_cost_cents_fixed(10_000_000.0), "$100,000.00");
        // Rounding carry: $0.995 → $1.00, not $0.100.
        assert_eq!(format_cost_cents_fixed(99.5), "$1.00");
    }

    #[test]
    fn truncate_label_middle_keeps_head_and_tail() {
        // #450 acceptance B: every label-like column uses one
        // truncation strategy — middle ellipsis so the head (branch
        // prefix) and tail (file name) both survive.
        assert_eq!(truncate_label_middle("short", 40), "short");
        assert_eq!(truncate_label_middle("", 40), "");

        let branch = "04-20-pava-1669_adds_an_optional_inputmode_prop_to_chararrayinput";
        let truncated = truncate_label_middle(branch, 20);
        assert_eq!(truncated.chars().count(), 20);
        assert!(truncated.contains('…'));
        assert!(truncated.starts_with("04-20-"));
        assert!(
            truncated.ends_with("arrayinput"),
            "middle ellipsis must preserve the tail, got {:?}",
            truncated
        );

        // Multi-byte content: emoji still splits on char boundaries,
        // not bytes, so we never panic (#389 / #383 / #404 / #445).
        let emoji = "src/🚀/main.rs".repeat(5);
        let t = truncate_label_middle(&emoji, 12);
        assert_eq!(t.chars().count(), 12);
    }

    #[test]
    fn truncate_label_middle_falls_back_for_very_narrow_widths() {
        // If the caller asks for less than 3 chars we can't fit both a
        // head and tail plus ellipsis. Fall back to the legacy tail
        // renderer so the output still fits the width budget.
        let t = truncate_label_middle("abcdefghij", 2);
        assert_eq!(t.chars().count(), 2);
    }

    #[test]
    fn breakdown_view_untagged_label_is_view_specific() {
        // #450 acceptance E: one `(untagged)` DB sentinel, six
        // different display labels.
        assert_eq!(BreakdownView::Projects.untagged_label(), "(no repository)");
        assert_eq!(BreakdownView::Branches.untagged_label(), "(no branch)");
        assert_eq!(BreakdownView::Tickets.untagged_label(), "(no ticket)");
        assert_eq!(BreakdownView::Activities.untagged_label(), "(unclassified)");
        assert_eq!(BreakdownView::Files.untagged_label(), "(no file tag)");
        assert_eq!(
            BreakdownView::Models.untagged_label(),
            "(model not yet attributed)"
        );
        // Tag view keeps the generic sentinel because a tag key can
        // mean anything — "(no X)" would be misleading.
        assert_eq!(BreakdownView::Tag.untagged_label(), "(untagged)");
    }

    #[test]
    fn display_dimension_translates_untagged_but_leaves_other_values_alone() {
        assert_eq!(
            display_dimension(BreakdownView::Tickets, "(untagged)"),
            "(no ticket)"
        );
        assert_eq!(
            display_dimension(BreakdownView::Files, "(untagged)"),
            "(no file tag)"
        );
        assert_eq!(display_dimension(BreakdownView::Branches, "main"), "main");
        assert_eq!(
            display_dimension(BreakdownView::Tickets, "PAVA-1669"),
            "PAVA-1669"
        );
    }

    #[test]
    fn merge_and_partition_pending_suppresses_zero_cost_placeholder() {
        // #443 + #450 acceptance: placeholder rows merge into one
        // `(model not yet attributed)` bucket per provider, and a
        // merged bucket whose cost is zero stays suppressed by
        // default (the pure `(untagged)`-transient case).
        use budi_core::analytics::ModelUsage;

        fn row(model: &str, provider: &str, cost: f64, msgs: u64) -> ModelUsage {
            ModelUsage {
                model: model.to_string(),
                provider: provider.to_string(),
                message_count: msgs,
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                cost_cents: cost,
            }
        }

        let rows = vec![
            row("claude-opus-4-7", "claude_code", 210_000.0, 24_114),
            row("(untagged)", "cursor", 0.0, 15),
            row("claude-sonnet-4-6", "claude_code", 30_000.0, 6138),
            row("(untagged)", "cursor", 0.0, 7),
        ];

        // Default: `(untagged)` collapses to one zero-cost Cursor
        // bucket, which gets suppressed — two input transients fold
        // into the one-row `suppressed_pending` count.
        let (visible, suppressed) = merge_and_partition_pending(&rows, false);
        assert_eq!(visible.len(), 2);
        assert_eq!(suppressed, 1);
        assert!(
            visible
                .iter()
                .all(|r| r.display_label != display::UNATTRIBUTED_LABEL)
        );

        // With --include-pending the merged placeholder shows.
        let (visible_all, suppressed_all) = merge_and_partition_pending(&rows, true);
        assert_eq!(visible_all.len(), 3);
        assert_eq!(suppressed_all, 0);
        assert!(
            visible_all
                .iter()
                .any(|r| r.display_label == display::UNATTRIBUTED_LABEL)
        );
    }

    #[test]
    fn merge_and_partition_pending_surfaces_cursor_auto_cost_by_default() {
        // #443 acceptance: Cursor's `default` (Auto mode) rows carry
        // real cost; they must render by default, merged under the
        // canonical `(model not yet attributed)` label — *not*
        // suppressed the way pure-untagged zero-cost transients are.
        use budi_core::analytics::ModelUsage;

        let rows = vec![
            ModelUsage {
                model: "default".into(),
                provider: "cursor".into(),
                message_count: 25,
                input_tokens: 1_000_000,
                output_tokens: 200_000,
                cache_read_tokens: 10_200_000,
                cache_creation_tokens: 1_000_000,
                cost_cents: 437.0,
            },
            ModelUsage {
                model: "(untagged)".into(),
                provider: "cursor".into(),
                message_count: 15,
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                cost_cents: 0.0,
            },
        ];

        let (visible, suppressed) = merge_and_partition_pending(&rows, false);
        assert_eq!(suppressed, 0);
        assert_eq!(visible.len(), 1);
        let merged = &visible[0];
        assert_eq!(merged.display_label, display::UNATTRIBUTED_LABEL);
        assert_eq!(merged.provider, "cursor");
        assert_eq!(merged.message_count, 40);
        assert_eq!(merged.cost_cents, 437.0);
    }

    #[test]
    fn models_json_carries_display_name_and_provider_model_id() {
        // #443 acceptance 5: `--format json` must expose both the
        // canonical `display_name` and the raw `provider_model_id`,
        // plus the effort modifier as a separate field.
        use budi_core::analytics::{BreakdownPage, ModelUsage};

        let rows = vec![
            ModelUsage {
                model: "claude-opus-4-7-thinking-high".into(),
                provider: "cursor".into(),
                message_count: 249,
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                cost_cents: 45_300.0,
            },
            ModelUsage {
                model: "claude-opus-4-7".into(),
                provider: "claude_code".into(),
                message_count: 890,
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                cost_cents: 45_600.0,
            },
        ];
        let page = BreakdownPage {
            rows,
            other: None,
            total_cost_cents: 90_900.0,
            total_rows: 2,
            shown_rows: 2,
            limit: 50,
        };

        let json = serde_json::to_value(serde_models_page_for_test(&page)).unwrap();
        let rows_json = json["rows"].as_array().unwrap();
        assert_eq!(rows_json.len(), 2);

        let cursor_row = &rows_json[0];
        assert_eq!(
            cursor_row["model"].as_str(),
            Some("claude-opus-4-7-thinking-high")
        );
        assert_eq!(
            cursor_row["provider_model_id"].as_str(),
            Some("claude-opus-4-7-thinking-high")
        );
        assert_eq!(cursor_row["display_name"].as_str(), Some("Claude Opus 4.7"));
        assert_eq!(
            cursor_row["effort_modifier"].as_str(),
            Some("thinking-high")
        );
        assert_eq!(cursor_row["placeholder"].as_str(), Some("none"));

        let canonical_row = &rows_json[1];
        assert_eq!(
            canonical_row["display_name"].as_str(),
            Some("Claude Opus 4.7")
        );
        // effort_modifier is omitted when None (serde skip).
        assert!(canonical_row.get("effort_modifier").is_none());
    }

    #[test]
    fn is_only_untagged_detects_lone_fallback_row() {
        // #450 acceptance D: a single-row page made of just the
        // `(untagged)` bucket is the "no labelled signal" case —
        // render an empty-state tip instead of a 1-row table that
        // looks like a filesystem fault.
        use budi_core::analytics::{FileCost, TicketCost};

        let lonely_files = vec![FileCost {
            file_path: "(untagged)".into(),
            session_count: 1,
            message_count: 5,
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            cost_cents: 114.0,
            top_repo_id: String::new(),
            top_branch: String::new(),
            top_ticket_id: String::new(),
            source: String::new(),
        }];
        assert!(is_only_untagged(&lonely_files, |f| &f.file_path));

        let mixed_tickets = vec![
            TicketCost {
                ticket_id: "PAVA-1".into(),
                ticket_prefix: "PAVA".into(),
                session_count: 1,
                message_count: 10,
                input_tokens: 0,
                output_tokens: 0,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                cost_cents: 100.0,
                top_branch: "main".into(),
                top_repo_id: String::new(),
                source: "branch".into(),
            },
            TicketCost {
                ticket_id: "(untagged)".into(),
                ticket_prefix: String::new(),
                session_count: 1,
                message_count: 10,
                input_tokens: 0,
                output_tokens: 0,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                cost_cents: 50.0,
                top_branch: String::new(),
                top_repo_id: String::new(),
                source: String::new(),
            },
        ];
        assert!(!is_only_untagged(&mixed_tickets, |t| &t.ticket_id));

        let empty: Vec<TicketCost> = vec![];
        assert!(!is_only_untagged(&empty, |t| &t.ticket_id));
    }

    #[test]
    fn untagged_only_tip_nudges_short_windows_to_widen() {
        // The "try a longer window" hint is only helpful on `today`
        // and `1d` — wider windows already saw enough data to know
        // nothing landed, so the extra line becomes noise.
        assert!(untagged_only_tip(StatsPeriod::Today).is_some());
        assert!(untagged_only_tip(StatsPeriod::Days(1)).is_some());
        assert!(untagged_only_tip(StatsPeriod::Days(7)).is_none());
        assert!(untagged_only_tip(StatsPeriod::Week).is_none());
        assert!(untagged_only_tip(StatsPeriod::Month).is_none());
    }

    // ─── #450 view-layout snapshot tests ──────────────────────────────
    //
    // Every breakdown view (`--tickets / --files / --branches /
    // --activities / --models`) is rendered through the same shared
    // helpers (`format_breakdown_header_text`, `format_breakdown_row_text`,
    // `format_cost_cents_fixed`). Pin the rendered shape under the
    // `today` and `30d` headings so the next regression shows up in
    // the diff instead of in a user's terminal.

    /// Build a baseline row for a view with the current label width,
    /// untagged translation, and fixed currency format — the surface
    /// the ticket asks us to keep stable.
    fn snapshot_row(view: BreakdownView, raw_label: &str, cents: f64, extras: &str) -> String {
        let label_width = 40usize;
        let translated = display_dimension(view, raw_label);
        let label = truncate_label_middle(&translated, label_width);
        format_breakdown_row_text(&label, label_width, cents, cents.max(1.0), extras)
    }

    #[test]
    fn snapshot_tickets_today_and_30d_layout_is_stable() {
        let today_label = period_label(StatsPeriod::Today);
        let thirty_label = period_label(StatsPeriod::Days(30));
        assert_eq!(today_label, "Today");
        assert_eq!(thirty_label, "Last 30 days");

        let header = format_breakdown_header_text("TICKET", 40, "SOURCE     TOP_BRANCH");
        assert!(header.contains("TICKET"));
        assert!(header.contains("COST"));
        assert!(header.contains("SOURCE"));

        let real = snapshot_row(
            BreakdownView::Tickets,
            "PAVA-1669",
            2_265.0,
            "src=branch  04-20-pava-1669",
        );
        assert!(real.contains("PAVA-1669"));
        assert!(real.contains("$22.65"));

        let untagged = snapshot_row(
            BreakdownView::Tickets,
            "(untagged)",
            9_122.0,
            "src=--      --",
        );
        assert!(untagged.contains("(no ticket)"));
        assert!(untagged.contains("$91.22"));
        assert!(!untagged.contains("(untagged)"));
    }

    #[test]
    fn snapshot_files_today_and_30d_layout_is_stable() {
        let row = snapshot_row(
            BreakdownView::Files,
            "crates/budi-cli/src/commands/stats.rs",
            58_732.0,
            "src=cwd     PAVA-1669",
        );
        assert!(row.contains("stats.rs"));
        assert!(row.contains("$587.32"));

        let long_path = snapshot_row(
            BreakdownView::Files,
            "very/deeply/nested/path/that/wraps/into/the/next/column/file.proto",
            12_000.0,
            "",
        );
        // Middle ellipsis keeps the start AND the filename visible.
        assert!(long_path.contains('…'));
        assert!(long_path.contains("file.proto") || long_path.contains(".proto"));

        let untagged = snapshot_row(BreakdownView::Files, "(untagged)", 11_400.0, "");
        assert!(untagged.contains("(no file tag)"));
    }

    #[test]
    fn snapshot_branches_today_and_30d_layout_is_stable() {
        let main = snapshot_row(BreakdownView::Branches, "main", 150_000.0, "budi");
        assert!(main.contains("main"));
        assert!(main.contains("$1,500.00"));

        let feature = snapshot_row(
            BreakdownView::Branches,
            "04-20-pava-1669_adds_an_optional_inputmode_prop_to_chararrayinput",
            2_265.0,
            "Verkada-Web",
        );
        // Middle-ellipsis retains the date + ticket prefix and the
        // distinctive suffix — the two ends readers actually need.
        assert!(feature.starts_with("  04-20-"));
        assert!(feature.contains('…'));

        let untagged = snapshot_row(BreakdownView::Branches, "(untagged)", 100.0, "--");
        assert!(untagged.contains("(no branch)"));
    }

    #[test]
    fn snapshot_activities_today_and_30d_layout_is_stable() {
        let coding = snapshot_row(
            BreakdownView::Activities,
            "coding",
            123_456.0,
            "conf=high  main",
        );
        assert!(coding.contains("coding"));
        assert!(coding.contains("$1,234.56"));

        let untagged = snapshot_row(
            BreakdownView::Activities,
            "(untagged)",
            420.0,
            "conf=--    --",
        );
        assert!(untagged.contains("(unclassified)"));
    }

    #[test]
    fn snapshot_models_today_and_30d_layout_is_stable() {
        // `--models` snapshot: the `(untagged)` sentinel renders as
        // `(model not yet attributed)` (#443 — same label the
        // `display::resolve` overlay uses, so the empty-state tip
        // and the row label agree). Breakdown currency is fixed
        // with thousands separators.
        let opus = snapshot_row(
            BreakdownView::Models,
            "claude-opus-4-7",
            210_000.0,
            "24114 msgs  120M tok",
        );
        assert!(opus.contains("claude-opus-4-7"));
        assert!(opus.contains("$2,100.00"));

        let pending_visible =
            snapshot_row(BreakdownView::Models, "(untagged)", 0.0, "162 msgs   0 tok");
        assert!(pending_visible.contains("(model not yet attributed)"));
    }

    #[test]
    fn format_breakdown_footer_uses_fixed_currency() {
        // The Total row renders with `format_cost_cents_fixed`, so a
        // value like `$2100.00` never humanizes to `$2.1K` at the
        // footer — reconciliation to the cent requires the fixed
        // shape. We assert the helper directly; the footer path uses
        // the same call.
        assert_eq!(format_cost_cents_fixed(210_000.0), "$2,100.00");
    }
}
