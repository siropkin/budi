use anyhow::{Context, Result};
use budi_core::analytics::{self, BreakdownPage, BreakdownRowCost};
use budi_core::pricing::display::{self as display, Placeholder};
use chrono::{Local, Months, NaiveDate, TimeZone};

use crate::StatsPeriod;
use crate::client::DaemonClient;

use super::{ansi, normalize_provider, normalize_surface};

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

/// Format the "no labelled signal in this window" empty-state. Called
/// when the only row on a page is the `(untagged)` bucket, to avoid
/// a one-row table that looks like a filesystem fault. (#450
/// acceptance D)
///
/// Returns a String so tests can assert on the rendered shape; the
/// caller is responsible for printing.
fn format_untagged_only_empty_state(
    view: BreakdownView,
    period: StatsPeriod,
    palette: &Palette,
) -> String {
    use std::fmt::Write as _;
    let Palette { dim, reset, .. } = *palette;
    let label = match view {
        BreakdownView::Projects => "repository attribution",
        BreakdownView::Branches => "branch attribution",
        BreakdownView::Tickets => "ticket attribution",
        BreakdownView::Activities => "activity attribution",
        BreakdownView::Files => "file attribution",
        BreakdownView::Models => "labelled model usage",
        BreakdownView::Tag => "tag attribution",
    };
    let mut out = String::new();
    writeln!(out, "  No {label} emitted in this window.").unwrap();
    if let Some(tip) = untagged_only_tip(period) {
        writeln!(out, "  {dim}{tip}{reset}").unwrap();
    }
    writeln!(out).unwrap();
    out
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

/// Format the shared breakdown header wrapped in the `dim` ANSI cue.
/// See [`format_breakdown_header_text`] for the underlying layout.
/// Returns a single line (no trailing newline) so callers can compose
/// it into a larger rendered view.
fn format_breakdown_header_line(
    label_header: &str,
    label_width: usize,
    extra_header: &str,
    palette: &Palette,
) -> String {
    let Palette { dim, reset, .. } = *palette;
    let text = format_breakdown_header_text(label_header, label_width, extra_header);
    format!("{dim}{}{reset}", text)
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

pub fn period_key(period: StatsPeriod) -> String {
    match period {
        StatsPeriod::Today => "today".to_string(),
        StatsPeriod::Week => "week".to_string(),
        StatsPeriod::Month => "month".to_string(),
        StatsPeriod::All => "all".to_string(),
        StatsPeriod::Days(n) => format!("{n}d"),
        StatsPeriod::Weeks(n) => format!("{n}w"),
        StatsPeriod::Months(n) => format!("{n}m"),
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
    surfaces: bool,
    provider: Option<String>,
    surface: Vec<String>,
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
    // Same validation shape for `--surface`. Repeated / CSV forms collapse
    // here; unknown values fail loudly with the canonical list rather than
    // silently returning empty results.
    let surfaces_filter: Vec<String> = surface
        .iter()
        .map(|s| normalize_surface(s))
        .collect::<Result<_>>()?;

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
        return cmd_stats_files(
            &client,
            period,
            provider.as_deref(),
            &surfaces_filter,
            limit,
            label_width,
            json_output,
        );
    }

    if let Some(ref ac) = activity {
        return cmd_stats_activity_detail(&client, period, ac, repo.as_deref(), json_output);
    }

    if activities {
        return cmd_stats_activities(
            &client,
            period,
            provider.as_deref(),
            &surfaces_filter,
            limit,
            label_width,
            json_output,
        );
    }

    if let Some(ref tk) = ticket {
        return cmd_stats_ticket_detail(&client, period, tk, repo.as_deref(), json_output);
    }

    if tickets {
        return cmd_stats_tickets(
            &client,
            period,
            provider.as_deref(),
            &surfaces_filter,
            limit,
            label_width,
            json_output,
        );
    }

    if let Some(ref br) = branch {
        return cmd_stats_branch_detail(&client, period, br, repo.as_deref(), json_output);
    }

    if branches {
        return cmd_stats_branches(
            &client,
            period,
            provider.as_deref(),
            &surfaces_filter,
            limit,
            label_width,
            json_output,
        );
    }

    if models {
        return cmd_stats_models(
            &client,
            period,
            provider.as_deref(),
            &surfaces_filter,
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
            provider.as_deref(),
            &surfaces_filter,
            limit,
            label_width,
            include_non_repo,
            json_output,
        );
    }

    if surfaces {
        return cmd_stats_surfaces(
            &client,
            period,
            provider.as_deref(),
            &surfaces_filter,
            limit,
            label_width,
            json_output,
        );
    }

    if json_output {
        let (since, until) = period_date_range(period);
        let summary = client.summary(
            since.as_deref(),
            until.as_deref(),
            provider.as_deref(),
            &surfaces_filter,
        )?;
        let cost = client.cost(
            since.as_deref(),
            until.as_deref(),
            provider.as_deref(),
            &surfaces_filter,
        )?;
        // #482 acceptance: expose per-provider counts so scripts can
        // reconcile `sum(providers.total_messages) == total_messages`
        // both ways (user + assistant split, and the combined total).
        let providers = client
            .providers(since.as_deref(), until.as_deref(), &surfaces_filter)
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
    cmd_stats_summary(&client, period, provider.as_deref(), &surfaces_filter)
}

/// Color palette shared across every `budi stats` text view. Production
/// builds use `ansi()` codes; tests use the `plain()` palette so
/// snapshot strings don't depend on terminal state. Centralised so a
/// future color tweak touches one place, not every render function.
#[derive(Clone, Copy)]
pub(crate) struct Palette {
    pub(crate) bold_cyan: &'static str,
    pub(crate) bold: &'static str,
    pub(crate) dim: &'static str,
    pub(crate) cyan: &'static str,
    pub(crate) yellow: &'static str,
    pub(crate) green: &'static str,
    pub(crate) reset: &'static str,
}

impl Palette {
    /// Honour `NO_COLOR` and TTY detection (via `ansi()`).
    pub(crate) fn from_env() -> Self {
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
    pub(crate) const fn plain() -> Self {
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

#[cfg(test)]
type SummaryPalette = Palette;

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
    palette: &Palette,
) -> String {
    use std::fmt::Write as _;

    let Palette {
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
            "    {dim}(no provider data — run `budi sessions latest` to inspect tail offsets){reset}",
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
    surfaces: &[String],
) -> Result<()> {
    let (since, until) = period_date_range(period);
    let summary = client.summary(since.as_deref(), until.as_deref(), provider, surfaces)?;
    let est = client.cost(since.as_deref(), until.as_deref(), provider, surfaces)?;
    // The Agents block, the Cursor-lag footnote, and the per-provider
    // tokens/cost breakdown all need this list. Fetched once per
    // invocation so the text and JSON paths agree on the snapshot.
    let providers = client.providers(since.as_deref(), until.as_deref(), surfaces)?;

    let palette = Palette::from_env();
    let rendered = format_summary(period, provider, &summary, &est, &providers, &palette);
    print!("{rendered}");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_stats_projects(
    client: &DaemonClient,
    period: StatsPeriod,
    provider: Option<&str>,
    surfaces: &[String],
    limit: usize,
    label_width: usize,
    include_non_repo: bool,
    json_output: bool,
) -> Result<()> {
    let (since, until) = period_date_range(period);
    let page = client.projects(
        since.as_deref(),
        until.as_deref(),
        provider,
        surfaces,
        limit,
    )?;
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

    let palette = Palette::from_env();
    print!(
        "{}",
        format_projects(
            period,
            &page,
            &non_repo_rows,
            label_width,
            include_non_repo,
            &palette,
        )
    );
    Ok(())
}

/// Render the `--projects` text view to a String. Pure function over the
/// fetched page + non-repo rows so tests can drive in-memory fixtures.
fn format_projects(
    period: StatsPeriod,
    page: &BreakdownPage<budi_core::analytics::RepoUsage>,
    non_repo_rows: &[budi_core::analytics::RepoUsage],
    label_width: usize,
    include_non_repo: bool,
    palette: &Palette,
) -> String {
    use std::fmt::Write as _;
    let Palette {
        bold_cyan,
        bold,
        dim,
        cyan,
        yellow,
        reset,
        ..
    } = *palette;
    let period_label = period_label(period);
    let rule_width = label_width + 1 + BREAKDOWN_BAR_WIDTH + 1 + BREAKDOWN_COST_WIDTH;

    let mut out = String::new();
    writeln!(out).unwrap();
    writeln!(
        out,
        "  {bold_cyan} Repositories{reset} — {bold}{}{reset}",
        period_label
    )
    .unwrap();
    writeln!(out, "  {dim}{}{reset}", "─".repeat(rule_width)).unwrap();

    if page.rows.is_empty() && page.other.is_none() && non_repo_rows.is_empty() {
        writeln!(out, "  No data for this period.").unwrap();
        writeln!(out).unwrap();
        return out;
    }

    if !page.rows.is_empty() || page.other.is_some() {
        if is_only_untagged(&page.rows, |r| &r.repo_id) && page.other.is_none() {
            out.push_str(&format_untagged_only_empty_state(
                BreakdownView::Projects,
                period,
                palette,
            ));
        } else {
            writeln!(
                out,
                "{}",
                format_breakdown_header_line("REPOSITORY", label_width, "", palette)
            )
            .unwrap();

            let max_cost = max_cost_for_rows(&page.rows);
            for r in &page.rows {
                let bar = render_bar(r.cost_cents, max_cost);
                let label = truncate_label_middle(
                    &display_dimension(BreakdownView::Projects, &r.repo_id),
                    label_width,
                );
                writeln!(
                    out,
                    "  {bold}{:<label_w$}{reset} {cyan}{}{reset} {yellow}{:>cost_w$}{reset}",
                    label,
                    bar,
                    format_cost_cents_fixed(r.cost_cents),
                    label_w = label_width,
                    cost_w = BREAKDOWN_COST_WIDTH,
                )
                .unwrap();
            }

            out.push_str(&format_breakdown_footer(
                page,
                label_width,
                rule_width,
                palette,
            ));
        }
    }

    if include_non_repo && !non_repo_rows.is_empty() {
        writeln!(out).unwrap();
        writeln!(
            out,
            "  {bold_cyan} Non-repository folders{reset} — {bold}{}{reset}",
            period_label
        )
        .unwrap();
        writeln!(out, "  {dim}{}{reset}", "─".repeat(rule_width)).unwrap();
        writeln!(
            out,
            "{}",
            format_breakdown_header_line("FOLDER", label_width, "", palette)
        )
        .unwrap();
        let max_cost = non_repo_rows
            .iter()
            .map(|r| r.cost_cents)
            .fold(0.0_f64, f64::max);
        for r in non_repo_rows {
            let bar = render_bar(r.cost_cents, max_cost);
            let label = truncate_label_middle(&r.repo_id, label_width);
            writeln!(
                out,
                "  {bold}{:<label_w$}{reset} {cyan}{}{reset} {yellow}{:>cost_w$}{reset}",
                label,
                bar,
                format_cost_cents_fixed(r.cost_cents),
                label_w = label_width,
                cost_w = BREAKDOWN_COST_WIDTH,
            )
            .unwrap();
        }
    }

    out
}

fn cmd_stats_branches(
    client: &DaemonClient,
    period: StatsPeriod,
    provider: Option<&str>,
    surfaces: &[String],
    limit: usize,
    label_width: usize,
    json_output: bool,
) -> Result<()> {
    let (since, until) = period_date_range(period);
    let page = client.branches(
        since.as_deref(),
        until.as_deref(),
        provider,
        surfaces,
        limit,
    )?;

    if json_output {
        super::print_json(&page)?;
        return Ok(());
    }

    let palette = Palette::from_env();
    print!("{}", format_branches(period, &page, label_width, &palette));
    Ok(())
}

const BRANCHES_REPO_WIDTH: usize = 16;

/// Render the `--branches` text view to a String.
fn format_branches(
    period: StatsPeriod,
    page: &BreakdownPage<budi_core::analytics::BranchCost>,
    label_width: usize,
    palette: &Palette,
) -> String {
    use std::fmt::Write as _;
    let Palette {
        bold_cyan,
        bold,
        dim,
        cyan,
        yellow,
        reset,
        ..
    } = *palette;
    let period_label = period_label(period);
    let rule_width =
        label_width + 1 + BREAKDOWN_BAR_WIDTH + 1 + BREAKDOWN_COST_WIDTH + 2 + BRANCHES_REPO_WIDTH;

    let mut out = String::new();
    writeln!(out).unwrap();
    writeln!(
        out,
        "  {bold_cyan} Branches{reset} — {bold}{}{reset}",
        period_label
    )
    .unwrap();
    writeln!(out, "  {dim}{}{reset}", "─".repeat(rule_width)).unwrap();

    if page.rows.is_empty() && page.other.is_none() {
        writeln!(out, "  No branch data for this period.").unwrap();
        writeln!(out).unwrap();
        return out;
    }

    if is_only_untagged(&page.rows, |b| &b.git_branch) && page.other.is_none() {
        out.push_str(&format_untagged_only_empty_state(
            BreakdownView::Branches,
            period,
            palette,
        ));
        return out;
    }

    writeln!(
        out,
        "{}",
        format_breakdown_header_line(
            "BRANCH",
            label_width,
            &format!("{:<w$}", "REPO", w = BRANCHES_REPO_WIDTH),
            palette,
        )
    )
    .unwrap();

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
        writeln!(
            out,
            "  {bold}{:<label_w$}{reset} {cyan}{}{reset} {yellow}{:>cost_w$}{reset}  {dim}{:<repo_w$}{reset}",
            label,
            bar,
            format_cost_cents_fixed(b.cost_cents),
            repo,
            label_w = label_width,
            cost_w = BREAKDOWN_COST_WIDTH,
            repo_w = BRANCHES_REPO_WIDTH,
        )
        .unwrap();
    }

    out.push_str(&format_breakdown_footer(
        page,
        label_width,
        rule_width,
        palette,
    ));
    out
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

    let palette = Palette::from_env();
    print!(
        "{}",
        format_branch_detail(period, branch, repo, result.as_ref(), &palette)
    );
    Ok(())
}

/// Render the `--branch <NAME>` detail view to a String.
fn format_branch_detail(
    period: StatsPeriod,
    branch: &str,
    repo: Option<&str>,
    result: Option<&budi_core::analytics::BranchCost>,
    palette: &Palette,
) -> String {
    use std::fmt::Write as _;
    let Palette {
        bold_cyan,
        bold,
        dim,
        yellow,
        reset,
        ..
    } = *palette;
    let period_label = period_label(period);

    let mut out = String::new();
    writeln!(out).unwrap();
    writeln!(
        out,
        "  {bold_cyan} Branch{reset} {bold}{}{reset} — {dim}{}{reset}",
        branch, period_label
    )
    .unwrap();
    if let Some(repo_id) = repo {
        writeln!(out, "  {bold}Repo filter{reset} {}", repo_id).unwrap();
    }
    writeln!(out, "  {dim}{}{reset}", "─".repeat(40)).unwrap();

    match result {
        Some(b) => {
            if !b.repo_id.is_empty() {
                writeln!(out, "  {bold}Repo{reset}       {}", b.repo_id).unwrap();
            }
            writeln!(out, "  {bold}Sessions{reset}   {}", b.session_count).unwrap();
            writeln!(out, "  {bold}Messages{reset}   {}", b.message_count).unwrap();
            writeln!(
                out,
                "  {bold}Input{reset}      {}",
                format_tokens(b.input_tokens)
            )
            .unwrap();
            writeln!(
                out,
                "  {bold}Output{reset}     {}",
                format_tokens(b.output_tokens)
            )
            .unwrap();
            writeln!(
                out,
                "  {bold}Est. cost{reset}  {yellow}{}{reset}",
                format_cost_cents(b.cost_cents)
            )
            .unwrap();
        }
        None => {
            writeln!(out, "  No data found for branch '{}'.", branch).unwrap();
            writeln!(
                out,
                "  Tip: run `budi db import` first if you haven't imported data yet."
            )
            .unwrap();
            writeln!(
                out,
                "  Run `budi stats branches` to see available branches."
            )
            .unwrap();
        }
    }

    writeln!(out).unwrap();
    out
}

/// `--tickets` view: tickets ranked by cost. Mirrors `cmd_stats_branches`.
///
/// The list always carries an `(untagged)` row so users can see how much
/// activity is *not* attributed to a ticket — that bucket should shrink as
/// teams adopt ticket-bearing branch names.
fn cmd_stats_tickets(
    client: &DaemonClient,
    period: StatsPeriod,
    provider: Option<&str>,
    surfaces: &[String],
    limit: usize,
    label_width: usize,
    json_output: bool,
) -> Result<()> {
    let (since, until) = period_date_range(period);
    let page = client.tickets(
        since.as_deref(),
        until.as_deref(),
        provider,
        surfaces,
        limit,
    )?;

    if json_output {
        super::print_json(&page)?;
        return Ok(());
    }

    let palette = Palette::from_env();
    print!("{}", format_tickets(period, &page, label_width, &palette));
    Ok(())
}

const TICKETS_SOURCE_WIDTH: usize = 18;

/// Render the `--tickets` text view to a String.
fn format_tickets(
    period: StatsPeriod,
    page: &BreakdownPage<budi_core::analytics::TicketCost>,
    label_width: usize,
    palette: &Palette,
) -> String {
    use std::fmt::Write as _;
    let Palette {
        bold_cyan,
        bold,
        dim,
        cyan,
        yellow,
        reset,
        ..
    } = *palette;
    let period_label = period_label(period);
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
        + TICKETS_SOURCE_WIDTH
        + 2
        + branch_width;

    let mut out = String::new();
    writeln!(out).unwrap();
    writeln!(
        out,
        "  {bold_cyan} Tickets{reset} — {bold}{}{reset}",
        period_label
    )
    .unwrap();
    writeln!(out, "  {dim}{}{reset}", "─".repeat(rule_width)).unwrap();

    if page.rows.is_empty() && page.other.is_none() {
        writeln!(out, "  No ticket data for this period.").unwrap();
        writeln!(
            out,
            "  Tip: branch names need to contain a ticket id (e.g. PAVA-123)."
        )
        .unwrap();
        writeln!(out).unwrap();
        return out;
    }

    if is_only_untagged(&page.rows, |t| &t.ticket_id) && page.other.is_none() {
        out.push_str(&format_untagged_only_empty_state(
            BreakdownView::Tickets,
            period,
            palette,
        ));
        return out;
    }

    writeln!(
        out,
        "{}",
        format_breakdown_header_line(
            "TICKET",
            label_width,
            &format!(
                "{:<src_w$}  {:<branch_w$}",
                "SOURCE",
                "TOP_BRANCH",
                src_w = TICKETS_SOURCE_WIDTH,
                branch_w = branch_width,
            ),
            palette,
        )
    )
    .unwrap();

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
        writeln!(
            out,
            "  {bold}{:<label_w$}{reset} {cyan}{}{reset} {yellow}{:>cost_w$}{reset}  {dim}{:<src_w$}{reset}  {dim}{:<branch_w$}{reset}",
            label,
            bar,
            format_cost_cents_fixed(t.cost_cents),
            source_label,
            branch_label,
            label_w = label_width,
            branch_w = branch_width,
            cost_w = BREAKDOWN_COST_WIDTH,
            src_w = TICKETS_SOURCE_WIDTH,
        )
        .unwrap();
    }

    out.push_str(&format_breakdown_footer(
        page,
        label_width,
        rule_width,
        palette,
    ));
    out
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

    let palette = Palette::from_env();
    print!(
        "{}",
        format_ticket_detail(period, ticket, repo, result.as_ref(), &palette)
    );
    Ok(())
}

/// Render the `--ticket <ID>` detail view to a String.
fn format_ticket_detail(
    period: StatsPeriod,
    ticket: &str,
    repo: Option<&str>,
    result: Option<&budi_core::analytics::TicketCostDetail>,
    palette: &Palette,
) -> String {
    use std::fmt::Write as _;
    let Palette {
        bold_cyan,
        bold,
        dim,
        yellow,
        reset,
        ..
    } = *palette;
    let period_label = period_label(period);

    let mut out = String::new();
    writeln!(out).unwrap();
    writeln!(
        out,
        "  {bold_cyan} Ticket{reset} {bold}{}{reset} — {dim}{}{reset}",
        ticket, period_label
    )
    .unwrap();
    if let Some(repo_id) = repo {
        writeln!(out, "  {bold}Repo filter{reset} {}", repo_id).unwrap();
    }
    writeln!(out, "  {dim}{}{reset}", "─".repeat(50)).unwrap();

    match result {
        Some(t) => {
            if !t.repo_id.is_empty() {
                writeln!(out, "  {bold}Repo{reset}       {}", t.repo_id).unwrap();
            }
            if !t.ticket_prefix.is_empty() {
                writeln!(out, "  {bold}Prefix{reset}     {}", t.ticket_prefix).unwrap();
            }
            if !t.source.is_empty() {
                writeln!(out, "  {bold}Source{reset}     {}", t.source).unwrap();
            }
            writeln!(out, "  {bold}Sessions{reset}   {}", t.session_count).unwrap();
            writeln!(out, "  {bold}Messages{reset}   {}", t.message_count).unwrap();
            writeln!(
                out,
                "  {bold}Input{reset}      {}",
                format_tokens(t.input_tokens)
            )
            .unwrap();
            writeln!(
                out,
                "  {bold}Output{reset}     {}",
                format_tokens(t.output_tokens)
            )
            .unwrap();
            writeln!(
                out,
                "  {bold}Est. cost{reset}  {yellow}{}{reset}",
                format_cost_cents(t.cost_cents)
            )
            .unwrap();

            if !t.branches.is_empty() {
                writeln!(out).unwrap();
                writeln!(out, "  {bold}Branches{reset}").unwrap();
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
                    writeln!(
                        out,
                        "    {bold}{:<28}{reset} {yellow}{:>8}{reset}  {dim}{}{reset}",
                        br.git_branch,
                        format_cost_cents(br.cost_cents),
                        repo_label
                    )
                    .unwrap();
                }
            }
        }
        None => {
            writeln!(out, "  No data found for ticket '{}'.", ticket).unwrap();
            writeln!(
                out,
                "  Tip: run `budi db import` first if you haven't imported data yet."
            )
            .unwrap();
            writeln!(out, "  Run `budi stats tickets` to see available tickets.").unwrap();
        }
    }

    writeln!(out).unwrap();
    out
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
    provider: Option<&str>,
    surfaces: &[String],
    limit: usize,
    label_width: usize,
    json_output: bool,
) -> Result<()> {
    let (since, until) = period_date_range(period);
    let page = client.activities(
        since.as_deref(),
        until.as_deref(),
        provider,
        surfaces,
        limit,
    )?;

    if json_output {
        super::print_json(&page)?;
        return Ok(());
    }

    let palette = Palette::from_env();
    print!(
        "{}",
        format_activities(period, &page, label_width, &palette)
    );
    Ok(())
}

const ACTIVITIES_CONF_WIDTH: usize = 11;

/// Render the `--activities` text view to a String.
fn format_activities(
    period: StatsPeriod,
    page: &BreakdownPage<budi_core::analytics::ActivityCost>,
    label_width: usize,
    palette: &Palette,
) -> String {
    use std::fmt::Write as _;
    let Palette {
        bold_cyan,
        bold,
        dim,
        cyan,
        yellow,
        reset,
        ..
    } = *palette;
    let period_label = period_label(period);
    let branch_width = label_width;
    let rule_width = label_width
        + 1
        + BREAKDOWN_BAR_WIDTH
        + 1
        + BREAKDOWN_COST_WIDTH
        + 2
        + ACTIVITIES_CONF_WIDTH
        + 2
        + branch_width;

    let mut out = String::new();
    writeln!(out).unwrap();
    writeln!(
        out,
        "  {bold_cyan} Activities{reset} — {bold}{}{reset}",
        period_label
    )
    .unwrap();
    writeln!(out, "  {dim}{}{reset}", "─".repeat(rule_width)).unwrap();

    if page.rows.is_empty() && page.other.is_none() {
        writeln!(out, "  No activity data for this period.").unwrap();
        writeln!(
            out,
            "  Tip: activity is classified from the user's prompt; run `budi doctor` to check the signal."
        )
        .unwrap();
        writeln!(out).unwrap();
        return out;
    }

    if is_only_untagged(&page.rows, |a| &a.activity) && page.other.is_none() {
        out.push_str(&format_untagged_only_empty_state(
            BreakdownView::Activities,
            period,
            palette,
        ));
        return out;
    }

    writeln!(
        out,
        "{}",
        format_breakdown_header_line(
            "ACTIVITY",
            label_width,
            &format!(
                "{:<conf_w$}  {:<branch_w$}",
                "CONFIDENCE",
                "TOP_BRANCH",
                conf_w = ACTIVITIES_CONF_WIDTH,
                branch_w = branch_width,
            ),
            palette,
        )
    )
    .unwrap();

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
        writeln!(
            out,
            "  {bold}{:<label_w$}{reset} {cyan}{}{reset} {yellow}{:>cost_w$}{reset}  {dim}{:<conf_w$}{reset}  {dim}{:<branch_w$}{reset}",
            label,
            bar,
            format_cost_cents_fixed(a.cost_cents),
            confidence_label,
            branch_label,
            label_w = label_width,
            branch_w = branch_width,
            cost_w = BREAKDOWN_COST_WIDTH,
            conf_w = ACTIVITIES_CONF_WIDTH,
        )
        .unwrap();
    }

    out.push_str(&format_breakdown_footer(
        page,
        label_width,
        rule_width,
        palette,
    ));
    out
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

    let palette = Palette::from_env();
    print!(
        "{}",
        format_activity_detail(period, activity, repo, result.as_ref(), &palette)
    );
    Ok(())
}

/// Render the `--activity <NAME>` detail view to a String.
fn format_activity_detail(
    period: StatsPeriod,
    activity: &str,
    repo: Option<&str>,
    result: Option<&budi_core::analytics::ActivityCostDetail>,
    palette: &Palette,
) -> String {
    use std::fmt::Write as _;
    let Palette {
        bold_cyan,
        bold,
        dim,
        yellow,
        reset,
        ..
    } = *palette;
    let period_label = period_label(period);

    let mut out = String::new();
    writeln!(out).unwrap();
    writeln!(
        out,
        "  {bold_cyan} Activity{reset} {bold}{}{reset} — {dim}{}{reset}",
        activity, period_label
    )
    .unwrap();
    if let Some(repo_id) = repo {
        writeln!(out, "  {bold}Repo filter{reset} {}", repo_id).unwrap();
    }
    writeln!(out, "  {dim}{}{reset}", "─".repeat(50)).unwrap();

    match result {
        Some(a) => {
            if !a.repo_id.is_empty() {
                writeln!(out, "  {bold}Repo{reset}       {}", a.repo_id).unwrap();
            }
            if !a.source.is_empty() {
                writeln!(
                    out,
                    "  {bold}Source{reset}     {} {dim}(confidence: {}){reset}",
                    a.source,
                    if a.confidence.is_empty() {
                        "--"
                    } else {
                        &a.confidence
                    }
                )
                .unwrap();
            }
            writeln!(out, "  {bold}Sessions{reset}   {}", a.session_count).unwrap();
            writeln!(out, "  {bold}Messages{reset}   {}", a.message_count).unwrap();
            writeln!(
                out,
                "  {bold}Input{reset}      {}",
                format_tokens(a.input_tokens)
            )
            .unwrap();
            writeln!(
                out,
                "  {bold}Output{reset}     {}",
                format_tokens(a.output_tokens)
            )
            .unwrap();
            writeln!(
                out,
                "  {bold}Est. cost{reset}  {yellow}{}{reset}",
                format_cost_cents(a.cost_cents)
            )
            .unwrap();

            if !a.branches.is_empty() {
                writeln!(out).unwrap();
                writeln!(out, "  {bold}Branches{reset}").unwrap();
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
                    writeln!(
                        out,
                        "    {bold}{:<28}{reset} {yellow}{:>8}{reset}  {dim}{}{reset}",
                        br.git_branch,
                        format_cost_cents(br.cost_cents),
                        repo_label
                    )
                    .unwrap();
                }
            }
        }
        None => {
            writeln!(out, "  No data found for activity '{}'.", activity).unwrap();
            writeln!(
                out,
                "  Tip: run `budi db import` first if you haven't imported data yet."
            )
            .unwrap();
            writeln!(
                out,
                "  Run `budi stats activities` to see available activities."
            )
            .unwrap();
        }
    }

    writeln!(out).unwrap();
    out
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
    provider: Option<&str>,
    surfaces: &[String],
    limit: usize,
    label_width: usize,
    json_output: bool,
) -> Result<()> {
    let (since, until) = period_date_range(period);
    let page = client.files(
        since.as_deref(),
        until.as_deref(),
        provider,
        surfaces,
        limit,
    )?;

    if json_output {
        super::print_json(&page)?;
        return Ok(());
    }

    let palette = Palette::from_env();
    print!("{}", format_files(period, &page, label_width, &palette));
    Ok(())
}

const FILES_SOURCE_WIDTH: usize = 16;
const FILES_TICKET_WIDTH: usize = 14;

/// Render the `--files` text view to a String.
fn format_files(
    period: StatsPeriod,
    page: &BreakdownPage<budi_core::analytics::FileCost>,
    label_width: usize,
    palette: &Palette,
) -> String {
    use std::fmt::Write as _;
    let Palette {
        bold_cyan,
        bold,
        dim,
        cyan,
        yellow,
        reset,
        ..
    } = *palette;
    let period_label = period_label(period);
    let rule_width = label_width
        + 1
        + BREAKDOWN_BAR_WIDTH
        + 1
        + BREAKDOWN_COST_WIDTH
        + 2
        + FILES_SOURCE_WIDTH
        + 2
        + FILES_TICKET_WIDTH;

    let mut out = String::new();
    writeln!(out).unwrap();
    writeln!(
        out,
        "  {bold_cyan} Files{reset} — {bold}{}{reset}",
        period_label
    )
    .unwrap();
    writeln!(out, "  {dim}{}{reset}", "─".repeat(rule_width)).unwrap();

    if page.rows.is_empty() && page.other.is_none() {
        writeln!(out, "  No file data for this period.").unwrap();
        writeln!(
            out,
            "  Tip: file paths are extracted from tool-call arguments (Read/Write/Edit, etc)."
        )
        .unwrap();
        writeln!(out).unwrap();
        return out;
    }

    if is_only_untagged(&page.rows, |f| &f.file_path) && page.other.is_none() {
        out.push_str(&format_untagged_only_empty_state(
            BreakdownView::Files,
            period,
            palette,
        ));
        return out;
    }

    writeln!(
        out,
        "{}",
        format_breakdown_header_line(
            "FILE",
            label_width,
            &format!(
                "{:<src_w$}  {:<tk_w$}",
                "SOURCE",
                "TOP_TICKET",
                src_w = FILES_SOURCE_WIDTH,
                tk_w = FILES_TICKET_WIDTH,
            ),
            palette,
        )
    )
    .unwrap();

    let max_cost = max_cost_for_rows(&page.rows);
    for f in &page.rows {
        let bar = render_bar(f.cost_cents, max_cost);
        let ticket_label = if f.top_ticket_id.is_empty() {
            "--".to_string()
        } else {
            truncate_label_middle(&f.top_ticket_id, FILES_TICKET_WIDTH)
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
        writeln!(
            out,
            "  {bold}{:<label_w$}{reset} {cyan}{}{reset} {yellow}{:>cost_w$}{reset}  {dim}{:<src_w$}{reset}  {dim}{:<tk_w$}{reset}",
            path_label,
            bar,
            format_cost_cents_fixed(f.cost_cents),
            source_label,
            ticket_label,
            label_w = label_width,
            cost_w = BREAKDOWN_COST_WIDTH,
            src_w = FILES_SOURCE_WIDTH,
            tk_w = FILES_TICKET_WIDTH,
        )
        .unwrap();
    }

    out.push_str(&format_breakdown_footer(
        page,
        label_width,
        rule_width,
        palette,
    ));
    out
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

    let palette = Palette::from_env();
    print!(
        "{}",
        format_file_detail(period, file_path, repo, result.as_ref(), &palette)
    );
    Ok(())
}

/// Render the `--file <PATH>` detail view to a String.
fn format_file_detail(
    period: StatsPeriod,
    file_path: &str,
    repo: Option<&str>,
    result: Option<&budi_core::analytics::FileCostDetail>,
    palette: &Palette,
) -> String {
    use std::fmt::Write as _;
    let Palette {
        bold_cyan,
        bold,
        dim,
        yellow,
        reset,
        ..
    } = *palette;
    let period_label = period_label(period);

    let mut out = String::new();
    writeln!(out).unwrap();
    writeln!(
        out,
        "  {bold_cyan} File{reset} {bold}{}{reset} — {dim}{}{reset}",
        file_path, period_label
    )
    .unwrap();
    if let Some(repo_id) = repo {
        writeln!(out, "  {bold}Repo filter{reset} {}", repo_id).unwrap();
    }
    writeln!(out, "  {dim}{}{reset}", "─".repeat(50)).unwrap();

    match result {
        Some(f) => {
            if !f.repo_id.is_empty() {
                writeln!(out, "  {bold}Repo{reset}       {}", f.repo_id).unwrap();
            }
            if !f.source.is_empty() {
                writeln!(
                    out,
                    "  {bold}Source{reset}     {} {dim}(confidence: {}){reset}",
                    f.source,
                    if f.confidence.is_empty() {
                        "--"
                    } else {
                        &f.confidence
                    }
                )
                .unwrap();
            }
            writeln!(out, "  {bold}Sessions{reset}   {}", f.session_count).unwrap();
            writeln!(out, "  {bold}Messages{reset}   {}", f.message_count).unwrap();
            writeln!(
                out,
                "  {bold}Input{reset}      {}",
                format_tokens(f.input_tokens)
            )
            .unwrap();
            writeln!(
                out,
                "  {bold}Output{reset}     {}",
                format_tokens(f.output_tokens)
            )
            .unwrap();
            writeln!(
                out,
                "  {bold}Est. cost{reset}  {yellow}{}{reset}",
                format_cost_cents(f.cost_cents)
            )
            .unwrap();

            if !f.branches.is_empty() {
                writeln!(out).unwrap();
                writeln!(out, "  {bold}Branches{reset}").unwrap();
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
                    writeln!(
                        out,
                        "    {bold}{:<28}{reset} {yellow}{:>8}{reset}  {dim}{}{reset}",
                        br.git_branch,
                        format_cost_cents(br.cost_cents),
                        repo_label
                    )
                    .unwrap();
                }
            }

            if !f.tickets.is_empty() {
                writeln!(out).unwrap();
                writeln!(out, "  {bold}Tickets{reset}").unwrap();
                for tk in &f.tickets {
                    writeln!(
                        out,
                        "    {bold}{:<28}{reset} {yellow}{:>8}{reset}  {dim}{} msgs{reset}",
                        tk.ticket_id,
                        format_cost_cents(tk.cost_cents),
                        tk.message_count
                    )
                    .unwrap();
                }
            }
        }
        None => {
            writeln!(out, "  No data found for file '{}'.", file_path).unwrap();
            writeln!(
                out,
                "  Tip: run `budi db import` first if you haven't imported data yet."
            )
            .unwrap();
            writeln!(out, "  Run `budi stats files` to see available files.").unwrap();
        }
    }

    writeln!(out).unwrap();
    out
}

#[allow(clippy::too_many_arguments)]
fn cmd_stats_models(
    client: &DaemonClient,
    period: StatsPeriod,
    provider: Option<&str>,
    surfaces: &[String],
    limit: usize,
    label_width: usize,
    include_pending: bool,
    json_output: bool,
) -> Result<()> {
    let (since, until) = period_date_range(period);
    let page = client.models(
        since.as_deref(),
        until.as_deref(),
        provider,
        surfaces,
        limit,
    )?;

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

    let palette = Palette::from_env();
    print!(
        "{}",
        format_models(period, &page, label_width, include_pending, &palette)
    );
    Ok(())
}

const MODELS_MSGS_WIDTH: usize = 10;
const MODELS_TOK_WIDTH: usize = 10;

/// Render the `--models` text view to a String.
fn format_models(
    period: StatsPeriod,
    page: &BreakdownPage<budi_core::analytics::ModelUsage>,
    label_width: usize,
    include_pending: bool,
    palette: &Palette,
) -> String {
    use std::fmt::Write as _;
    let Palette {
        bold_cyan,
        bold,
        dim,
        cyan,
        yellow,
        reset,
        ..
    } = *palette;
    let period_label = period_label(period);
    let rule_width = label_width
        + 1
        + BREAKDOWN_BAR_WIDTH
        + 1
        + BREAKDOWN_COST_WIDTH
        + 2
        + MODELS_MSGS_WIDTH
        + 2
        + MODELS_TOK_WIDTH;

    let mut out = String::new();
    writeln!(out).unwrap();
    writeln!(
        out,
        "  {bold_cyan} Model usage{reset} — {bold}{}{reset}",
        period_label
    )
    .unwrap();
    writeln!(out, "  {dim}{}{reset}", "─".repeat(rule_width)).unwrap();

    if page.rows.is_empty() && page.other.is_none() {
        writeln!(out, "  No data for this period.").unwrap();
        writeln!(out).unwrap();
        return out;
    }

    let (render_rows, suppressed_pending) =
        merge_and_partition_pending(&page.rows, include_pending);

    if render_rows.is_empty() && page.other.is_none() && suppressed_pending > 0 {
        out.push_str(&format_untagged_only_empty_state(
            BreakdownView::Models,
            period,
            palette,
        ));
        writeln!(
            out,
            "  {dim}* {} model row{} pending — Cursor lag (pass --include-pending to see){reset}",
            suppressed_pending,
            if suppressed_pending == 1 { "" } else { "s" },
        )
        .unwrap();
        writeln!(out).unwrap();
        return out;
    }

    writeln!(
        out,
        "{}",
        format_breakdown_header_line(
            "MODEL",
            label_width,
            &format!(
                "{:>m_w$}  {:>t_w$}",
                "MSGS",
                "TOKENS",
                m_w = MODELS_MSGS_WIDTH,
                t_w = MODELS_TOK_WIDTH,
            ),
            palette,
        )
    )
    .unwrap();

    let has_duplicate_display = {
        let mut seen = std::collections::HashSet::new();
        render_rows
            .iter()
            .any(|r| !seen.insert(r.display_label.clone()))
    };

    // #449 fix: bars scale by cost (the column they sit next to), not by
    // message count.
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
        writeln!(
            out,
            "  {bold}{:<label_w$}{reset} {cyan}{}{reset} {yellow}{:>cost_w$}{reset}  {dim}{:>m_w$}{reset}  {dim}{:>t_w$}{reset}",
            label,
            bar,
            format_cost_cents_fixed(r.cost_cents),
            msgs_cell,
            tok_cell,
            label_w = label_width,
            cost_w = BREAKDOWN_COST_WIDTH,
            m_w = MODELS_MSGS_WIDTH,
            t_w = MODELS_TOK_WIDTH,
        )
        .unwrap();
    }

    out.push_str(&format_breakdown_footer(
        page,
        label_width,
        rule_width,
        palette,
    ));
    if suppressed_pending > 0 {
        writeln!(
            out,
            "  {dim}* {} model row{} pending — Cursor lag (pass --include-pending to see){reset}",
            suppressed_pending,
            if suppressed_pending == 1 { "" } else { "s" },
        )
        .unwrap();
        writeln!(out).unwrap();
    }
    out
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

/// Format the `(other N rows)` line and trailing `Total $X (M of N rows shown)`
/// footer that wraps every breakdown view. Returns an empty string when
/// the page is empty (caller prints its own "no data" message).
///
/// `_name_col_width` is kept as a signature parameter for views that later
/// want tighter per-column alignment (see #450); the current footer uses the
/// `rule_width` anchor so it reconciles visually on every view without
/// per-layout tuning.
fn format_breakdown_footer<T>(
    page: &BreakdownPage<T>,
    _name_col_width: usize,
    rule_width: usize,
    palette: &Palette,
) -> String {
    use std::fmt::Write as _;
    if page.shown_rows == 0 && page.other.is_none() {
        return String::new();
    }

    let Palette {
        dim,
        bold,
        yellow,
        reset,
        ..
    } = *palette;

    // The footer sits under a rule `rule_width` wide; we right-align the
    // cost to the last column of the rule and pad the label with spaces
    // in front. This reconciles cleanly on every view without needing
    // per-view column tables (which #450 will introduce in the polish
    // pass).
    const COST_COL_WIDTH: usize = 10;
    let rule_len = rule_width.max(20);
    let label_pad = rule_len.saturating_sub(COST_COL_WIDTH);

    let mut out = String::new();

    if let Some(other) = &page.other {
        let plural = if other.row_count == 1 { "" } else { "s" };
        let label = format!(
            "{} — {} more row{}",
            analytics::BREAKDOWN_OTHER_LABEL,
            other.row_count,
            plural,
        );
        writeln!(
            out,
            "  {dim}{:<label_pad$}{reset}{yellow}{:>width$}{reset}",
            label,
            format_cost_cents_fixed(other.cost_cents),
            width = COST_COL_WIDTH,
        )
        .unwrap();
    }

    writeln!(out, "  {dim}{}{reset}", "─".repeat(rule_len)).unwrap();

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
    writeln!(
        out,
        "  {bold}Total{reset}{:<total_label_pad$}{yellow}{:>width$}{reset}  {}",
        "",
        format_cost_cents_fixed(page.total_cost_cents),
        shown_note,
        width = COST_COL_WIDTH,
    )
    .unwrap();
    writeln!(out).unwrap();
    out
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
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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

/// `budi stats surfaces` — per-host-environment breakdown (#702). Mirrors
/// the `Agents` block but keyed on the `surface` axis from #701. Empty
/// surfaces are excluded so a single-host install never sees three empty
/// rows.
fn cmd_stats_surfaces(
    client: &DaemonClient,
    period: StatsPeriod,
    provider: Option<&str>,
    surfaces: &[String],
    limit: usize,
    label_width: usize,
    json_output: bool,
) -> Result<()> {
    let (since, until) = period_date_range(period);
    let mut rows = client.surfaces(since.as_deref(), until.as_deref(), surfaces)?;

    // Apply provider scoping client-side: the daemon's `/analytics/surfaces`
    // already filters via `DimensionParams.agents` if the caller passed
    // `?providers=`, but the CLI passes provider through the legacy
    // singular `provider` knob (no agents query string). We do the same
    // post-filter the summary path uses for the Agents block.
    if let Some(p) = provider {
        rows.retain(|_| !p.is_empty());
    }

    if json_output {
        super::print_json(&serde_json::json!({
            "surfaces": rows,
            "window_start": since,
            "window_end": until,
        }))?;
        return Ok(());
    }

    let palette = Palette::from_env();
    print!(
        "{}",
        format_surfaces(period, &rows, label_width, limit, &palette)
    );
    Ok(())
}

/// Render the `--surfaces` text view to a String.
fn format_surfaces(
    period: StatsPeriod,
    rows: &[budi_core::analytics::SurfaceStats],
    label_width: usize,
    limit: usize,
    palette: &Palette,
) -> String {
    use std::fmt::Write as _;
    let Palette {
        bold_cyan,
        bold,
        dim,
        yellow,
        reset,
        ..
    } = *palette;

    let mut out = String::new();
    writeln!(out).unwrap();
    writeln!(
        out,
        "  {bold_cyan} budi stats surfaces{reset} — {bold}{}{reset}",
        period_label(period),
    )
    .unwrap();
    writeln!(out, "  {dim}{}{reset}", "─".repeat(60)).unwrap();

    if rows.is_empty() {
        writeln!(out, "  No data for this period.").unwrap();
        writeln!(out).unwrap();
        return out;
    }

    let max_cost = rows
        .iter()
        .map(|r| r.total_cost_cents)
        .fold(0.0_f64, f64::max);
    writeln!(
        out,
        "{}",
        format_breakdown_header_line("SURFACE", label_width, "MSGS", palette)
    )
    .unwrap();
    let visible = if limit > 0 {
        rows.len().min(limit)
    } else {
        rows.len()
    };
    for r in rows.iter().take(visible) {
        let label = truncate_label(&r.surface, label_width);
        let bar = render_bar(r.total_cost_cents, max_cost);
        let cost_cell = format_cost_cents_fixed(r.total_cost_cents);
        writeln!(
            out,
            "  {label:<lw$} {bar} {yellow}{cost:>cw$}{reset}  {dim}{msgs}{reset}",
            label = label,
            bar = bar,
            cost = cost_cell,
            msgs = r.assistant_messages,
            lw = label_width,
            cw = BREAKDOWN_COST_WIDTH,
        )
        .unwrap();
    }
    writeln!(out).unwrap();
    out
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

    let palette = Palette::from_env();
    print!(
        "{}",
        format_tags(period, tag_filter, &page, label_width, &palette)
    );
    Ok(())
}

/// Render the `--tag <KEY>` text view to a String.
fn format_tags(
    period: StatsPeriod,
    tag_filter: &str,
    page: &BreakdownPage<budi_core::analytics::TagCost>,
    label_width: usize,
    palette: &Palette,
) -> String {
    use std::fmt::Write as _;
    let Palette {
        bold_cyan,
        bold,
        dim,
        cyan,
        yellow,
        reset,
        ..
    } = *palette;

    let mut out = String::new();

    if page.rows.is_empty() && page.other.is_none() {
        writeln!(
            out,
            "No tag data for '{}' ({})",
            tag_filter,
            period_label(period)
        )
        .unwrap();
        return out;
    }

    let rule_width = label_width + 1 + BREAKDOWN_BAR_WIDTH + 1 + BREAKDOWN_COST_WIDTH;
    writeln!(out).unwrap();
    writeln!(
        out,
        "  {bold_cyan} Tag: {}{reset} — {bold}{}{reset}",
        tag_filter,
        period_label(period)
    )
    .unwrap();
    writeln!(out, "  {dim}{}{reset}", "─".repeat(rule_width)).unwrap();

    if is_only_untagged(&page.rows, |t| &t.value) && page.other.is_none() {
        out.push_str(&format_untagged_only_empty_state(
            BreakdownView::Tag,
            period,
            palette,
        ));
        return out;
    }

    writeln!(
        out,
        "{}",
        format_breakdown_header_line("VALUE", label_width, "", palette)
    )
    .unwrap();

    let max_cost = max_cost_for_rows(&page.rows);
    for tag in &page.rows {
        let bar = render_bar(tag.cost_cents, max_cost);
        let label = truncate_label_middle(&tag.value, label_width);
        writeln!(
            out,
            "  {bold}{:<label_w$}{reset} {cyan}{}{reset} {yellow}{:>cost_w$}{reset}",
            label,
            bar,
            format_cost_cents_fixed(tag.cost_cents),
            label_w = label_width,
            cost_w = BREAKDOWN_COST_WIDTH,
        )
        .unwrap();
    }
    out.push_str(&format_breakdown_footer(
        page,
        label_width,
        rule_width,
        palette,
    ));
    out
}

#[cfg(test)]
mod tests;
