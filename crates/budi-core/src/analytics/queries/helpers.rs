//! Shared helpers for the analytics query layer.
//!
//! Holds the filter / rollup / breakdown-envelope primitives consumed by every
//! sibling module under `queries/`. Private helpers are `pub(super)` so they
//! resolve to a single canonical implementation; nothing here escapes the
//! `queries` module without an explicit `pub` marker.

use chrono::{DateTime, NaiveDate, Timelike, Utc};
use rusqlite::Connection;
use std::collections::HashSet;

use super::breakdowns::{ActivityCost, ModelUsage, TagCost, TicketCost};
use super::dimensions::FileCost;
use super::summary::{BranchCost, RepoUsage};

pub const UNTAGGED_DIMENSION: &str = "(untagged)";
pub(super) const ROLLUPS_HOURLY_TABLE: &str = "message_rollups_hourly";
pub(super) const ROLLUPS_DAILY_TABLE: &str = "message_rollups_daily";

#[derive(Debug, Clone, Copy)]
pub(super) enum RollupLevel {
    Hourly,
    Daily,
}

#[derive(Debug, Clone)]
pub(super) struct RollupWindow {
    pub(super) level: RollupLevel,
    pub(super) since: Option<String>,
    pub(super) until: Option<String>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct DimensionFilters {
    #[serde(default)]
    pub agents: Vec<String>,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub projects: Vec<String>,
    #[serde(default)]
    pub branches: Vec<String>,
    /// Host environment filter — `vscode`, `cursor`, `jetbrains`, `terminal`,
    /// `unknown`. Mirrors `agents` shape: lowercased + trimmed + deduped on
    /// normalize, unknown values pass through and yield empty results so a
    /// new-host extension hitting an old daemon does not crash. See
    /// `crate::surface` and ticket #702.
    #[serde(default)]
    pub surfaces: Vec<String>,
}

impl DimensionFilters {
    pub fn normalize(mut self) -> Self {
        self.agents = normalize_values(&self.agents);
        self.models = normalize_values(&self.models);
        self.projects = normalize_values(&self.projects);
        self.branches = normalize_branches(&self.branches);
        self.surfaces = normalize_surfaces(&self.surfaces);
        self
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FilterOptions {
    pub agents: Vec<String>,
    pub models: Vec<String>,
    pub projects: Vec<String>,
    pub branches: Vec<String>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Validate an ISO 8601 date/datetime string.
/// Accepts formats: "2026-03-14", "2026-03-14T18:00:00Z", "2026-03-14T18:00:00+00:00".
pub(super) fn is_valid_timestamp(s: &str) -> bool {
    // Must start with YYYY-MM-DD pattern
    if s.len() < 10 {
        return false;
    }
    let bytes = s.as_bytes();
    bytes[0..4].iter().all(|b| b.is_ascii_digit())
        && bytes[4] == b'-'
        && bytes[5..7].iter().all(|b| b.is_ascii_digit())
        && bytes[7] == b'-'
        && bytes[8..10].iter().all(|b| b.is_ascii_digit())
}

/// Build a parameterized date filter clause and its bind values.
/// Returns (clause_str, params_vec) where clause_str uses ?N placeholders.
/// Invalid timestamps are silently skipped (treated as None).
#[cfg(test)]
pub(super) fn date_filter(
    since: Option<&str>,
    until: Option<&str>,
    keyword: &str,
) -> (String, Vec<String>) {
    let mut conditions = Vec::new();
    let mut param_values = Vec::new();
    if let Some(s) = since {
        if is_valid_timestamp(s) {
            param_values.push(s.to_string());
            conditions.push(format!("timestamp >= ?{}", param_values.len()));
        } else {
            tracing::warn!("date_filter: invalid 'since' timestamp ignored: {s}");
        }
    }
    if let Some(u) = until {
        if is_valid_timestamp(u) {
            param_values.push(u.to_string());
            conditions.push(format!("timestamp < ?{}", param_values.len()));
        } else {
            tracing::warn!("date_filter: invalid 'until' timestamp ignored: {u}");
        }
    }
    if conditions.is_empty() {
        (String::new(), param_values)
    } else {
        (
            format!("{} {}", keyword, conditions.join(" AND ")),
            param_values,
        )
    }
}

pub(super) fn normalize_values(values: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for value in values {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            continue;
        }
        let normalized = trimmed.to_string();
        if seen.insert(normalized.clone()) {
            out.push(normalized);
        }
    }
    out
}

/// Normalize a surface filter list: trim, lowercase, drop empties, dedupe.
/// Lowercasing is safe because surface values are canonical lowercase
/// (`vscode`, `cursor`, `jetbrains`, `terminal`, `unknown`); we accept
/// mixed-case input from CLI users without rejecting it. Unknown values
/// pass through unchanged so the caller (host-extension or curl) gets a
/// clean empty result rather than an error — same shape as agents/providers.
pub(crate) fn normalize_surfaces(values: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for value in values {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            continue;
        }
        let normalized = trimmed.to_ascii_lowercase();
        if seen.insert(normalized.clone()) {
            out.push(normalized);
        }
    }
    out
}

pub(super) fn normalize_branches(values: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for value in values {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            continue;
        }
        let without_ref = trimmed.strip_prefix("refs/heads/").unwrap_or(trimmed);
        let normalized = if without_ref.is_empty() {
            UNTAGGED_DIMENSION.to_string()
        } else {
            without_ref.to_string()
        };
        if seen.insert(normalized.clone()) {
            out.push(normalized);
        }
    }
    out
}

pub(super) fn append_in_condition(
    conditions: &mut Vec<String>,
    param_values: &mut Vec<String>,
    expression: &str,
    values: &[String],
) {
    if values.is_empty() {
        return;
    }

    let mut placeholders = Vec::with_capacity(values.len());
    for value in values {
        param_values.push(value.clone());
        placeholders.push(format!("?{}", param_values.len()));
    }
    conditions.push(format!("{expression} IN ({})", placeholders.join(", ")));
}

pub(super) fn normalized_model_expr(expr: &str) -> String {
    format!(
        "CASE WHEN {expr} IS NULL OR {expr} = '' OR SUBSTR({expr}, 1, 1) = '<' THEN '{UNTAGGED_DIMENSION}' ELSE {expr} END"
    )
}

pub(super) fn normalized_project_expr(expr: &str) -> String {
    format!("COALESCE(NULLIF(NULLIF({expr}, ''), 'unknown'), '{UNTAGGED_DIMENSION}')")
}

pub(super) fn normalized_branch_expr(expr: &str) -> String {
    format!(
        "COALESCE(NULLIF(CASE WHEN COALESCE({expr}, '') LIKE 'refs/heads/%' THEN SUBSTR(COALESCE({expr}, ''), 12) ELSE COALESCE({expr}, '') END, ''), '{UNTAGGED_DIMENSION}')"
    )
}

/// SQL fragment that COALESCE-normalizes a surface column to the canonical
/// lowercase form, falling back to `'unknown'` for NULL / empty rows. Used
/// by the surface filter (`?surface=` / `?surfaces=`) so a row with
/// `surface = NULL` still matches `?surface=unknown` instead of silently
/// dropping out — same pattern `normalized_*_expr` use for the other
/// dimensions. (#702)
pub(super) fn normalized_surface_expr(expr: &str) -> String {
    format!("COALESCE(NULLIF(LOWER({expr}), ''), 'unknown')")
}

#[allow(clippy::too_many_arguments)]
pub(super) fn apply_dimension_filters(
    conditions: &mut Vec<String>,
    param_values: &mut Vec<String>,
    filters: &DimensionFilters,
    provider_expr: &str,
    model_expr: &str,
    project_expr: &str,
    branch_expr: &str,
    surface_expr: &str,
) {
    append_in_condition(conditions, param_values, provider_expr, &filters.agents);
    append_in_condition(conditions, param_values, model_expr, &filters.models);
    append_in_condition(conditions, param_values, project_expr, &filters.projects);
    append_in_condition(conditions, param_values, branch_expr, &filters.branches);
    append_in_condition(conditions, param_values, surface_expr, &filters.surfaces);
}

pub(super) fn rollups_available(conn: &Connection) -> bool {
    let hourly_exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name = ?1)",
            [ROLLUPS_HOURLY_TABLE],
            |row| row.get(0),
        )
        .unwrap_or(false);
    let daily_exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name = ?1)",
            [ROLLUPS_DAILY_TABLE],
            |row| row.get(0),
        )
        .unwrap_or(false);
    hourly_exists && daily_exists
}

pub(super) fn parse_timestamp_boundary_utc(value: &str) -> Option<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
        return Some(dt.with_timezone(&Utc));
    }
    if let Ok(day) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return day
            .and_hms_opt(0, 0, 0)
            .map(|naive| DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc));
    }
    None
}

pub(super) fn is_day_aligned(ts: DateTime<Utc>) -> bool {
    ts.hour() == 0 && ts.minute() == 0 && ts.second() == 0 && ts.nanosecond() == 0
}

pub(super) fn is_hour_aligned(ts: DateTime<Utc>) -> bool {
    ts.minute() == 0 && ts.second() == 0 && ts.nanosecond() == 0
}

pub(super) fn choose_rollup_window(
    since: Option<&str>,
    until: Option<&str>,
    prefer_daily: bool,
) -> Option<RollupWindow> {
    let since_ts = since.and_then(parse_timestamp_boundary_utc);
    let until_ts = until.and_then(parse_timestamp_boundary_utc);

    if since.is_some() && since_ts.is_none() {
        return None;
    }
    if until.is_some() && until_ts.is_none() {
        return None;
    }
    if let (Some(s), Some(u)) = (since_ts, until_ts)
        && s >= u
    {
        return None;
    }

    let day_aligned = since_ts.is_none_or(is_day_aligned) && until_ts.is_none_or(is_day_aligned);
    if prefer_daily && day_aligned {
        return Some(RollupWindow {
            level: RollupLevel::Daily,
            since: since_ts.map(|ts| ts.format("%Y-%m-%d").to_string()),
            until: until_ts.map(|ts| ts.format("%Y-%m-%d").to_string()),
        });
    }

    let hour_aligned = since_ts.is_none_or(is_hour_aligned) && until_ts.is_none_or(is_hour_aligned);
    if hour_aligned {
        return Some(RollupWindow {
            level: RollupLevel::Hourly,
            since: since_ts.map(|ts| ts.format("%Y-%m-%dT%H:00:00Z").to_string()),
            until: until_ts.map(|ts| ts.format("%Y-%m-%dT%H:00:00Z").to_string()),
        });
    }

    None
}

pub(super) fn rollup_table(level: RollupLevel) -> &'static str {
    match level {
        RollupLevel::Hourly => ROLLUPS_HOURLY_TABLE,
        RollupLevel::Daily => ROLLUPS_DAILY_TABLE,
    }
}

pub(super) fn rollup_time_column(level: RollupLevel) -> &'static str {
    match level {
        RollupLevel::Hourly => "bucket_start",
        RollupLevel::Daily => "bucket_day",
    }
}

pub(super) fn append_rollup_time_filters(
    conditions: &mut Vec<String>,
    params: &mut Vec<String>,
    window: &RollupWindow,
) {
    let time_col = rollup_time_column(window.level);
    if let Some(s) = &window.since {
        params.push(s.clone());
        conditions.push(format!("{time_col} >= ?{}", params.len()));
    }
    if let Some(u) = &window.until {
        params.push(u.clone());
        conditions.push(format!("{time_col} < ?{}", params.len()));
    }
}

// ---------------------------------------------------------------------------
// Usage Summary
// ---------------------------------------------------------------------------

/// Summary statistics for display.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UsageSummary {
    pub total_messages: u64,
    pub total_user_messages: u64,
    pub total_assistant_messages: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cache_creation_tokens: u64,
    pub total_cache_read_tokens: u64,
    pub total_cost_cents: f64,
}

// ---------------------------------------------------------------------------
// Breakdown envelope (#448)
// ---------------------------------------------------------------------------
//
// Every `budi stats` breakdown view (`--projects`, `--branches`, `--tickets`,
// `--activities`, `--files`, `--models`, `--tag`) ships through this envelope
// so the shape of every endpoint carries a grand total and an explicit
// `(other)` aggregate when rows are truncated. Before 8.3 these views
// returned a bare `Vec<T>` capped at 30 rows with no footer, which caused
// `--files 30d` to silently underreport cost by ~9% on machines with more
// than 30 distinct file paths (#448 reproduction).
//
// Contract: `sum(rows) + other.cost_cents == total_cost_cents`, to the cent,
// for every period and breakdown — enforced by `paginate_breakdown` and
// exercised by the reconciliation tests in `analytics/tests.rs`.

/// Display label for the truncation-tail aggregate row in breakdown output.
///
/// Distinct from [`UNTAGGED_DIMENSION`]: `(other)` is "the cost we truncated
/// from the bottom of the ranked list", `(untagged)` is "the cost that
/// carries no tag value at all". Both can coexist on the same view.
pub const BREAKDOWN_OTHER_LABEL: &str = "(other)";

/// Aggregate of every row ranked below the requested limit. Emitted as a
/// sibling of the top-N rows so scripts reconcile to the grand total
/// without needing to re-query with `--limit 0`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BreakdownOther {
    /// How many rows were folded into this aggregate.
    pub row_count: usize,
    /// Summed cost of the folded rows, in cents.
    pub cost_cents: f64,
}

/// Envelope wrapping every breakdown response: top-N rows plus a truncated
/// `(other)` aggregate plus the grand total and total distinct-row count
/// for the requested window.
///
/// `limit == 0` means "no cap" — `rows` holds every matched row and `other`
/// is always `None`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BreakdownPage<T> {
    /// Top rows sorted by cost (highest first), truncated to `shown_rows`.
    pub rows: Vec<T>,
    /// Aggregate of rows beyond `limit`. `None` when nothing was truncated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub other: Option<BreakdownOther>,
    /// Grand total across every row in the window, to the cent. Equals
    /// `sum(rows.cost_cents) + other.cost_cents` when `other` is `Some`.
    pub total_cost_cents: f64,
    /// Number of distinct rows matched by the query, including rows folded
    /// into `(other)`.
    pub total_rows: usize,
    /// How many rows are in `rows` (always `<= limit` when `limit > 0`).
    pub shown_rows: usize,
    /// Effective limit applied. `0` = unlimited.
    pub limit: usize,
}

/// Contract for breakdown row types so [`paginate_breakdown`] can read
/// the cost field without type-specific casing. Implementations live
/// alongside the envelope because the trait is purely load-bearing for
/// truncation math, not part of the row's public semantics.
pub trait BreakdownRowCost {
    fn cost_cents(&self) -> f64;
}

impl BreakdownRowCost for RepoUsage {
    fn cost_cents(&self) -> f64 {
        self.cost_cents
    }
}

impl BreakdownRowCost for BranchCost {
    fn cost_cents(&self) -> f64 {
        self.cost_cents
    }
}

impl BreakdownRowCost for TicketCost {
    fn cost_cents(&self) -> f64 {
        self.cost_cents
    }
}

impl BreakdownRowCost for ActivityCost {
    fn cost_cents(&self) -> f64 {
        self.cost_cents
    }
}

impl BreakdownRowCost for FileCost {
    fn cost_cents(&self) -> f64 {
        self.cost_cents
    }
}

impl BreakdownRowCost for ModelUsage {
    fn cost_cents(&self) -> f64 {
        self.cost_cents
    }
}

impl BreakdownRowCost for TagCost {
    fn cost_cents(&self) -> f64 {
        self.cost_cents
    }
}

/// Sentinel limit that disables truncation. Passed to the underlying SQL
/// LIMIT clause as an i64, so we stay well under i64::MAX.
pub const BREAKDOWN_FETCH_ALL_LIMIT: usize = 1_000_000;

/// Split `all_rows` (already sorted by cost DESC) into the visible top-N
/// plus an `(other)` aggregate, and compute the grand total.
///
/// Callers fetch the full set first (via `*_cost_with_filters` with a very
/// large SQL `LIMIT`) and hand the vec to this helper. That keeps the
/// truncation logic in one place so every breakdown reconciles to the cent
/// (#448 acceptance).
pub fn paginate_breakdown<T: BreakdownRowCost>(
    mut all_rows: Vec<T>,
    limit: usize,
) -> BreakdownPage<T> {
    let total_rows = all_rows.len();
    let total_cost_cents: f64 = all_rows.iter().map(BreakdownRowCost::cost_cents).sum();

    if limit == 0 || total_rows <= limit {
        let shown_rows = total_rows;
        return BreakdownPage {
            rows: all_rows,
            other: None,
            total_cost_cents,
            total_rows,
            shown_rows,
            limit,
        };
    }

    // #484: derive `other.cost_cents` from the grand total minus the
    // kept-rows sum (computed AFTER drain, so it iterates the exact same
    // Vec<T> the caller will see on the wire). This makes the `#448`
    // reconciliation contract `sum(rows.cost_cents) + other.cost_cents
    // == total_cost_cents` a definitional identity in f64 — the caller
    // sums the same kept-Vec the daemon did, so both sides of the
    // identity accumulate in the same order and cancel exactly. Pre-
    // 8.3.1 `other.cost_cents` was a fresh sum over the drained tail,
    // which differed from `total - kept` by an f64 associativity
    // rounding error of up to a few cents on 30-day windows with
    // fractional per-row costs (the 2026-04-22 audit's 1-22¢ drift).
    let rest: Vec<T> = all_rows.drain(limit..).collect();
    let kept_cost: f64 = all_rows.iter().map(BreakdownRowCost::cost_cents).sum();
    let other_cost = total_cost_cents - kept_cost;
    let other = BreakdownOther {
        row_count: rest.len(),
        cost_cents: other_cost,
    };
    BreakdownPage {
        rows: all_rows,
        other: Some(other),
        total_cost_cents,
        total_rows,
        shown_rows: limit,
        limit,
    }
}
