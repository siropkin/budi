//! Analytics query functions: summaries, messages, repos, activity, branches,
//! tags, models, providers, cache efficiency, cost curves, and statusline stats.

use anyhow::Result;
use chrono::{DateTime, NaiveDate, Timelike, Utc};
use rusqlite::Connection;
use std::collections::HashSet;

use super::MessageRow;

pub const UNTAGGED_DIMENSION: &str = "(untagged)";
const ROLLUPS_HOURLY_TABLE: &str = "message_rollups_hourly";
const ROLLUPS_DAILY_TABLE: &str = "message_rollups_daily";

#[derive(Debug, Clone, Copy)]
enum RollupLevel {
    Hourly,
    Daily,
}

#[derive(Debug, Clone)]
struct RollupWindow {
    level: RollupLevel,
    since: Option<String>,
    until: Option<String>,
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
fn is_valid_timestamp(s: &str) -> bool {
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
fn date_filter(since: Option<&str>, until: Option<&str>, keyword: &str) -> (String, Vec<String>) {
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

fn normalize_values(values: &[String]) -> Vec<String> {
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

fn normalize_branches(values: &[String]) -> Vec<String> {
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

fn append_in_condition(
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

fn normalized_model_expr(expr: &str) -> String {
    format!(
        "CASE WHEN {expr} IS NULL OR {expr} = '' OR SUBSTR({expr}, 1, 1) = '<' THEN '{UNTAGGED_DIMENSION}' ELSE {expr} END"
    )
}

fn normalized_project_expr(expr: &str) -> String {
    format!("COALESCE(NULLIF(NULLIF({expr}, ''), 'unknown'), '{UNTAGGED_DIMENSION}')")
}

fn normalized_branch_expr(expr: &str) -> String {
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
fn normalized_surface_expr(expr: &str) -> String {
    format!("COALESCE(NULLIF(LOWER({expr}), ''), 'unknown')")
}

#[allow(clippy::too_many_arguments)]
fn apply_dimension_filters(
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

fn rollups_available(conn: &Connection) -> bool {
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

fn parse_timestamp_boundary_utc(value: &str) -> Option<DateTime<Utc>> {
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

fn is_day_aligned(ts: DateTime<Utc>) -> bool {
    ts.hour() == 0 && ts.minute() == 0 && ts.second() == 0 && ts.nanosecond() == 0
}

fn is_hour_aligned(ts: DateTime<Utc>) -> bool {
    ts.minute() == 0 && ts.second() == 0 && ts.nanosecond() == 0
}

fn choose_rollup_window(
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

fn rollup_table(level: RollupLevel) -> &'static str {
    match level {
        RollupLevel::Hourly => ROLLUPS_HOURLY_TABLE,
        RollupLevel::Daily => ROLLUPS_DAILY_TABLE,
    }
}

fn rollup_time_column(level: RollupLevel) -> &'static str {
    match level {
        RollupLevel::Hourly => "bucket_start",
        RollupLevel::Daily => "bucket_day",
    }
}

fn append_rollup_time_filters(
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

/// Query a usage summary, optionally filtered by date range.
/// Consolidated into a single scan of the messages table.
#[cfg(test)]
pub fn usage_summary(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<UsageSummary> {
    let (where_clause, date_params) = date_filter(since, until, "WHERE");
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = date_params
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    // Single scan: all aggregates in one query
    let sql = format!(
        "SELECT COUNT(*),
                COALESCE(SUM(CASE WHEN role = 'user' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN role = 'assistant' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(input_tokens), 0),
                COALESCE(SUM(output_tokens), 0),
                COALESCE(SUM(cache_creation_tokens), 0),
                COALESCE(SUM(cache_read_tokens), 0),
                COALESCE(SUM(cost_cents), 0.0)
         FROM messages {}",
        where_clause
    );
    let (
        total_messages,
        total_user_messages,
        total_assistant_messages,
        total_input,
        total_output,
        total_cache_create,
        total_cache_read,
        total_cost_cents,
    ): (u64, u64, u64, u64, u64, u64, u64, f64) =
        conn.query_row(&sql, param_refs.as_slice(), |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
            ))
        })?;

    Ok(UsageSummary {
        total_messages,
        total_user_messages,
        total_assistant_messages,
        total_input_tokens: total_input,
        total_output_tokens: total_output,
        total_cache_creation_tokens: total_cache_create,
        total_cache_read_tokens: total_cache_read,
        total_cost_cents,
    })
}

/// Query a usage summary, optionally filtered by date range and provider.
pub fn usage_summary_filtered(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    provider: Option<&str>,
) -> Result<UsageSummary> {
    let filters = DimensionFilters::default();
    usage_summary_with_filters(conn, since, until, provider, &filters)
}

fn usage_summary_from_rollups(
    conn: &Connection,
    window: &RollupWindow,
    provider: Option<&str>,
    filters: &DimensionFilters,
) -> Result<UsageSummary> {
    let mut conditions = Vec::new();
    let mut params: Vec<String> = Vec::new();
    append_rollup_time_filters(&mut conditions, &mut params, window);

    if let Some(p) = provider {
        params.push(p.to_string());
        conditions.push(format!("provider = ?{}", params.len()));
    }
    apply_dimension_filters(
        &mut conditions,
        &mut params,
        filters,
        "provider",
        "model",
        "repo_id",
        "git_branch",
        "surface",
    );

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };
    let sql = format!(
        "SELECT
            COALESCE(SUM(message_count), 0),
            COALESCE(SUM(CASE WHEN role = 'user' THEN message_count ELSE 0 END), 0),
            COALESCE(SUM(CASE WHEN role = 'assistant' THEN message_count ELSE 0 END), 0),
            COALESCE(SUM(input_tokens), 0),
            COALESCE(SUM(output_tokens), 0),
            COALESCE(SUM(cache_creation_tokens), 0),
            COALESCE(SUM(cache_read_tokens), 0),
            COALESCE(SUM(cost_cents), 0.0)
         FROM {} {where_clause}",
        rollup_table(window.level)
    );
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();
    let (
        total_messages,
        total_user_messages,
        total_assistant_messages,
        total_input,
        total_output,
        total_cache_create,
        total_cache_read,
        total_cost_cents,
    ): (u64, u64, u64, u64, u64, u64, u64, f64) =
        conn.query_row(&sql, param_refs.as_slice(), |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
            ))
        })?;

    Ok(UsageSummary {
        total_messages,
        total_user_messages,
        total_assistant_messages,
        total_input_tokens: total_input,
        total_output_tokens: total_output,
        total_cache_creation_tokens: total_cache_create,
        total_cache_read_tokens: total_cache_read,
        total_cost_cents,
    })
}

pub fn usage_summary_with_filters(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    provider: Option<&str>,
    filters: &DimensionFilters,
) -> Result<UsageSummary> {
    if rollups_available(conn)
        && let Some(window) = choose_rollup_window(since, until, true)
    {
        return usage_summary_from_rollups(conn, &window, provider, filters);
    }

    let mut conditions = Vec::new();
    let mut params: Vec<String> = Vec::new();

    if let Some(s) = since
        && is_valid_timestamp(s)
    {
        params.push(s.to_string());
        conditions.push(format!("timestamp >= ?{}", params.len()));
    }
    if let Some(u) = until
        && is_valid_timestamp(u)
    {
        params.push(u.to_string());
        conditions.push(format!("timestamp < ?{}", params.len()));
    }
    if let Some(p) = provider {
        params.push(p.to_string());
        conditions.push(format!(
            "COALESCE(provider, 'claude_code') = ?{}",
            params.len()
        ));
    }

    let model_expr = normalized_model_expr("model");
    let project_expr = normalized_project_expr("repo_id");
    let branch_expr = normalized_branch_expr("git_branch");
    let surface_expr = normalized_surface_expr("surface");
    apply_dimension_filters(
        &mut conditions,
        &mut params,
        filters,
        "COALESCE(provider, 'claude_code')",
        &model_expr,
        &project_expr,
        &branch_expr,
        &surface_expr,
    );

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    // Single scan: all aggregates in one query
    let sql = format!(
        "SELECT COUNT(*),
                COALESCE(SUM(CASE WHEN role = 'user' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN role = 'assistant' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(input_tokens), 0),
                COALESCE(SUM(output_tokens), 0),
                COALESCE(SUM(cache_creation_tokens), 0),
                COALESCE(SUM(cache_read_tokens), 0),
                COALESCE(SUM(cost_cents), 0.0)
         FROM messages {}",
        where_clause
    );
    let (
        total_messages,
        total_user_messages,
        total_assistant_messages,
        total_input,
        total_output,
        total_cache_create,
        total_cache_read,
        total_cost_cents,
    ): (u64, u64, u64, u64, u64, u64, u64, f64) =
        conn.query_row(&sql, param_refs.as_slice(), |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
            ))
        })?;

    Ok(UsageSummary {
        total_messages,
        total_user_messages,
        total_assistant_messages,
        total_input_tokens: total_input,
        total_output_tokens: total_output,
        total_cache_creation_tokens: total_cache_create,
        total_cache_read_tokens: total_cache_read,
        total_cost_cents,
    })
}

// ---------------------------------------------------------------------------
// Message List
// ---------------------------------------------------------------------------

/// Paginated message list result.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PaginatedMessages {
    pub messages: Vec<MessageRow>,
    pub total_count: u64,
}

/// Parameters for paginated message queries.
pub struct MessageListParams<'a> {
    pub since: Option<&'a str>,
    pub until: Option<&'a str>,
    pub search: Option<&'a str>,
    pub sort_by: Option<&'a str>,
    pub sort_asc: bool,
    pub limit: usize,
    pub offset: usize,
    /// Singular `?provider=<name>` shape — mirrors `SummaryParams`.
    pub provider: Option<&'a str>,
    /// Multi-value dimension filters (`providers`, `models`, `projects`,
    /// `branches`) — same shape every breakdown route already accepts.
    pub filters: &'a DimensionFilters,
}

/// List messages with pagination, search, and sorting.
pub fn message_list(conn: &Connection, p: &MessageListParams) -> Result<PaginatedMessages> {
    let mut conditions = vec!["messages.role = 'assistant'".to_string()];
    let mut param_values: Vec<String> = Vec::new();
    if let Some(s) = p.since {
        param_values.push(s.to_string());
        conditions.push(format!("messages.timestamp >= ?{}", param_values.len()));
    }
    if let Some(u) = p.until {
        param_values.push(u.to_string());
        conditions.push(format!("messages.timestamp < ?{}", param_values.len()));
    }
    if let Some(provider) = p.provider {
        param_values.push(provider.to_string());
        conditions.push(format!(
            "COALESCE(messages.provider, 'claude_code') = ?{}",
            param_values.len()
        ));
    }
    let model_expr = normalized_model_expr("messages.model");
    let project_expr = normalized_project_expr("messages.repo_id");
    let branch_expr = normalized_branch_expr("COALESCE(messages.git_branch, s.git_branch)");
    let surface_expr = normalized_surface_expr("messages.surface");
    apply_dimension_filters(
        &mut conditions,
        &mut param_values,
        p.filters,
        "COALESCE(messages.provider, 'claude_code')",
        &model_expr,
        &project_expr,
        &branch_expr,
        &surface_expr,
    );
    if let Some(q) = p.search
        && !q.is_empty()
    {
        let escaped = q
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        param_values.push(format!("%{escaped}%"));
        let idx = param_values.len();
        conditions.push(format!(
            "(messages.model LIKE ?{idx} ESCAPE '\\' OR messages.repo_id LIKE ?{idx} ESCAPE '\\' OR messages.provider LIKE ?{idx} ESCAPE '\\' OR COALESCE(messages.git_branch, s.git_branch) LIKE ?{idx} ESCAPE '\\' OR EXISTS (SELECT 1 FROM tags WHERE tags.message_id = messages.id AND tags.key = 'ticket_id' AND tags.value LIKE ?{idx} ESCAPE '\\'))"
        ));
    }
    let where_clause = format!("WHERE {}", conditions.join(" AND "));
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    let dir = if p.sort_asc { "ASC" } else { "DESC" };
    // For nullable text columns in ASC order, push NULLs/empty to the bottom.
    let order_expr = match p.sort_by.unwrap_or("timestamp") {
        col @ ("model" | "provider") => {
            let qcol = format!("messages.{col}");
            if p.sort_asc {
                format!("({qcol} IS NULL OR {qcol} = '') ASC, {qcol} {dir}")
            } else {
                format!("{qcol} {dir}")
            }
        }
        "tokens" => format!("(messages.input_tokens + messages.output_tokens) {dir}"),
        "cost" => format!("COALESCE(messages.cost_cents, 0.0) {dir}"),
        "branch" | "git_branch" | "ticket" => {
            let col = "COALESCE(messages.git_branch, s.git_branch)";
            if p.sort_asc {
                format!("({col} IS NULL OR {col} = '') ASC, {col} {dir}")
            } else {
                format!("{col} {dir}")
            }
        }
        "repo_id" => {
            let col = "COALESCE(messages.repo_id, s.repo_id)";
            if p.sort_asc {
                format!("({col} IS NULL OR {col} = '') ASC, {col} {dir}")
            } else {
                format!("{col} {dir}")
            }
        }
        _ => format!("messages.timestamp {dir}"),
    };

    let sql = format!(
        "SELECT messages.id, messages.session_id, messages.timestamp, messages.role, messages.model,
                COALESCE(messages.provider, 'claude_code'),
                COALESCE(messages.repo_id, s.repo_id),
                messages.input_tokens, messages.output_tokens,
                messages.cache_creation_tokens, messages.cache_read_tokens,
                COALESCE(messages.cost_cents, 0.0),
                COALESCE(messages.cost_confidence, 'estimated'),
                COALESCE(messages.git_branch, s.git_branch),
                COALESCE(NULLIF(messages.surface, ''), 'unknown') AS surface
         FROM messages
         LEFT JOIN sessions s ON s.id = messages.session_id
         {}
         ORDER BY {order_expr}
         LIMIT {} OFFSET {}",
        where_clause, p.limit, p.offset
    );

    // Count total matching rows separately so it's correct even when offset exceeds data
    let count_sql = format!(
        "SELECT COUNT(*)
         FROM messages
         LEFT JOIN sessions s ON s.id = messages.session_id
         {where_clause}"
    );
    let total_count: u64 = conn.query_row(&count_sql, param_refs.as_slice(), |row| row.get(0))?;

    let mut stmt = conn.prepare(&sql)?;
    let messages: Vec<MessageRow> = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(MessageRow {
                id: row.get(0)?,
                session_id: row.get(1)?,
                timestamp: row.get(2)?,
                role: row.get(3)?,
                model: row.get(4)?,
                provider: row.get(5)?,
                repo_id: row.get(6)?,
                input_tokens: row.get(7)?,
                output_tokens: row.get(8)?,
                cache_creation_tokens: row.get(9)?,
                cache_read_tokens: row.get(10)?,
                cost_cents: row.get(11)?,
                cost_confidence: row.get(12)?,
                git_branch: row.get(13)?,
                surface: row.get(14)?,
                request_id: None,
                assistant_sequence: None,
                tools: Vec::new(),
                tags: Vec::new(),
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(PaginatedMessages {
        messages,
        total_count,
    })
}

// ---------------------------------------------------------------------------
// Repository Usage
// ---------------------------------------------------------------------------

/// Repository usage stats, grouped by repo_id.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RepoUsage {
    pub repo_id: String,
    pub display_path: String,
    pub message_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_cents: f64,
}

/// Query repository usage, grouped by repo_id, optionally filtered by date.
pub fn repo_usage(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    limit: usize,
) -> Result<Vec<RepoUsage>> {
    let filters = DimensionFilters::default();
    repo_usage_with_filters(conn, since, until, &filters, limit)
}

fn repo_usage_from_rollups(
    conn: &Connection,
    window: &RollupWindow,
    filters: &DimensionFilters,
    limit: usize,
) -> Result<Vec<RepoUsage>> {
    let mut conditions = vec!["role = 'assistant'".to_string()];
    let mut params: Vec<String> = Vec::new();
    append_rollup_time_filters(&mut conditions, &mut params, window);
    apply_dimension_filters(
        &mut conditions,
        &mut params,
        filters,
        "provider",
        "model",
        "repo_id",
        "git_branch",
        "surface",
    );
    params.push(limit.to_string());
    let limit_idx = params.len();
    let repo_expr = normalized_project_expr("m.repo_id");
    let sql = format!(
        "WITH ranked AS (
             SELECT repo_id as repo,
                    COALESCE(SUM(message_count), 0) as cnt,
                    COALESCE(SUM(input_tokens), 0) as inp,
                    COALESCE(SUM(output_tokens), 0) as outp,
                    COALESCE(SUM(cost_cents), 0.0) as cost
             FROM {}
             WHERE {}
             GROUP BY repo
             ORDER BY cost DESC
             LIMIT ?{limit_idx}
         )
         SELECT ranked.repo,
                COALESCE(
                    (
                        SELECT MIN(m.cwd)
                        FROM messages m
                        WHERE m.role = 'assistant'
                          AND {repo_expr} = ranked.repo
                    ),
                    '(untagged)'
                ) as display_path,
                ranked.cnt,
                ranked.inp,
                ranked.outp,
                ranked.cost
         FROM ranked
         ORDER BY ranked.cost DESC",
        rollup_table(window.level),
        conditions.join(" AND ")
    );

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<RepoUsage> = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(RepoUsage {
                repo_id: row.get(0)?,
                display_path: row.get(1)?,
                message_count: row.get(2)?,
                input_tokens: row.get(3)?,
                output_tokens: row.get(4)?,
                cost_cents: row.get(5)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

pub fn repo_usage_with_filters(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    filters: &DimensionFilters,
    limit: usize,
) -> Result<Vec<RepoUsage>> {
    if rollups_available(conn)
        && let Some(window) = choose_rollup_window(since, until, true)
    {
        return repo_usage_from_rollups(conn, &window, filters, limit);
    }

    // Build parameterized date + dimension filters.
    let mut conditions = vec!["role = 'assistant'".to_string()];
    let mut param_values: Vec<String> = Vec::new();
    if let Some(s) = since {
        param_values.push(s.to_string());
        conditions.push(format!("timestamp >= ?{}", param_values.len()));
    }
    if let Some(u) = until {
        param_values.push(u.to_string());
        conditions.push(format!("timestamp < ?{}", param_values.len()));
    }
    let model_expr = normalized_model_expr("model");
    let project_expr = normalized_project_expr("repo_id");
    let branch_expr = normalized_branch_expr("git_branch");
    let surface_expr = normalized_surface_expr("surface");
    apply_dimension_filters(
        &mut conditions,
        &mut param_values,
        filters,
        "COALESCE(provider, 'claude_code')",
        &model_expr,
        &project_expr,
        &branch_expr,
        &surface_expr,
    );

    param_values.push(limit.to_string());
    let limit_idx = param_values.len();

    let sql = format!(
        "SELECT COALESCE(repo_id, '(untagged)') as repo,
                COALESCE(MIN(cwd), '(untagged)') as display_path,
                COUNT(*) as cnt,
                COALESCE(SUM(input_tokens), 0) as inp,
                COALESCE(SUM(output_tokens), 0) as outp,
                COALESCE(SUM(cost_cents), 0.0) as cost
         FROM messages
         WHERE {}
         GROUP BY repo
         ORDER BY cost DESC
         LIMIT ?{}",
        conditions.join(" AND "),
        limit_idx
    );

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<RepoUsage> = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(RepoUsage {
                repo_id: row.get(0)?,
                display_path: row.get(1)?,
                message_count: row.get(2)?,
                input_tokens: row.get(3)?,
                output_tokens: row.get(4)?,
                cost_cents: row.get(5)?,
            })
        })?
        .filter_map(|r| match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("skipping row: {e}");
                None
            }
        })
        .collect();

    Ok(rows)
}

/// #442: Per-cwd breakdown for `messages` rows whose `repo_id` is NULL
/// (non-repository work — scratch dirs, `~/Desktop`, brew-tap
/// checkouts). Rows are grouped by the **basename** of `cwd` so the
/// output matches the pre-8.3 per-folder-name labels users already
/// recognize from their history (`Desktop`, `ivan.seredkin`, etc.).
///
/// Used by `budi stats --projects --include-non-repo` to surface the
/// detail that was collapsed into `(no repository)` by default.
/// Returns an empty vec when there are no non-repo rows in the window.
pub fn non_repo_usage(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    limit: usize,
) -> Result<Vec<RepoUsage>> {
    let mut conditions = vec![
        "role = 'assistant'".to_string(),
        "(repo_id IS NULL OR repo_id = '')".to_string(),
        "cwd IS NOT NULL".to_string(),
        "cwd != ''".to_string(),
    ];
    let mut param_values: Vec<String> = Vec::new();
    if let Some(s) = since {
        param_values.push(s.to_string());
        conditions.push(format!("timestamp >= ?{}", param_values.len()));
    }
    if let Some(u) = until {
        param_values.push(u.to_string());
        conditions.push(format!("timestamp < ?{}", param_values.len()));
    }

    let sql = format!(
        "SELECT cwd,
                COUNT(*) AS cnt,
                COALESCE(SUM(input_tokens), 0) AS inp,
                COALESCE(SUM(output_tokens), 0) AS outp,
                COALESCE(SUM(cost_cents), 0.0) AS cost
         FROM messages
         WHERE {}
         GROUP BY cwd",
        conditions.join(" AND ")
    );

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<(String, u64, u64, u64, f64)> = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, u64>(2)?,
                row.get::<_, u64>(3)?,
                row.get::<_, f64>(4)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();

    // Aggregate same-basename cwds together. `~/Desktop` and
    // `/tmp/Desktop` both collapse to `Desktop`, matching the label the
    // user saw in their pre-8.3 stats.
    let mut agg: std::collections::BTreeMap<String, (String, u64, u64, u64, f64)> =
        std::collections::BTreeMap::new();
    for (cwd, cnt, inp, outp, cost) in rows {
        let label = cwd_basename(&cwd);
        let entry = agg
            .entry(label)
            .or_insert_with(|| (cwd.clone(), 0, 0, 0, 0.0));
        // Keep the lexicographically-smallest cwd as the display_path
        // so output is deterministic even when basenames collide.
        if cwd < entry.0 {
            entry.0 = cwd;
        }
        entry.1 += cnt;
        entry.2 += inp;
        entry.3 += outp;
        entry.4 += cost;
    }

    let mut out: Vec<RepoUsage> = agg
        .into_iter()
        .map(|(label, (sample_cwd, cnt, inp, outp, cost))| RepoUsage {
            repo_id: label,
            display_path: sample_cwd,
            message_count: cnt,
            input_tokens: inp,
            output_tokens: outp,
            cost_cents: cost,
        })
        .collect();
    out.sort_by(|a, b| {
        b.cost_cents
            .partial_cmp(&a.cost_cents)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.repo_id.cmp(&b.repo_id))
    });
    if limit > 0 && out.len() > limit {
        out.truncate(limit);
    }
    Ok(out)
}

/// Return the last path component of `cwd`, or `(unknown)` if empty.
/// Strips a trailing `/` so `/foo/bar/` and `/foo/bar` collapse to `bar`.
fn cwd_basename(cwd: &str) -> String {
    let trimmed = cwd.trim_end_matches('/');
    if trimmed.is_empty() {
        return "(unknown)".to_string();
    }
    trimmed
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("(unknown)")
        .to_string()
}

// ---------------------------------------------------------------------------
// Activity Chart
// ---------------------------------------------------------------------------

/// Activity data bucketed by time granularity.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ActivityBucket {
    pub label: String,
    pub message_count: u64,
    pub tool_call_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_cents: f64,
}

/// Query activity data bucketed in local time (see `tz_offset_min`).
///
/// `granularity`:
/// - `"hour"` — bucket label is hour of day (`strftime('%H:00', …)`).
/// - `"month"` — bucket is calendar month (`'%Y-%m'`).
/// - `"day"` and `"week"` — both use **calendar-day** buckets (`date(…)`); there is no ISO-week rollup yet.
///
/// `tz_offset_min`: timezone offset in minutes from UTC for grouping (e.g. -420 for PDT).
pub fn activity_chart(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    granularity: &str,
    tz_offset_min: i32,
) -> Result<Vec<ActivityBucket>> {
    let filters = DimensionFilters::default();
    activity_chart_with_filters(conn, since, until, &filters, granularity, tz_offset_min)
}

fn activity_chart_from_rollups(
    conn: &Connection,
    window: &RollupWindow,
    filters: &DimensionFilters,
    granularity: &str,
    tz_offset_min: i32,
) -> Result<Vec<ActivityBucket>> {
    let mut conditions = vec!["role = 'assistant'".to_string()];
    let mut params: Vec<String> = Vec::new();
    append_rollup_time_filters(&mut conditions, &mut params, window);
    apply_dimension_filters(
        &mut conditions,
        &mut params,
        filters,
        "provider",
        "model",
        "repo_id",
        "git_branch",
        "surface",
    );
    let where_clause = format!("WHERE {}", conditions.join(" AND "));
    let time_col = rollup_time_column(window.level);

    let group_expr = match window.level {
        RollupLevel::Daily => match granularity {
            "month" => format!("strftime('%Y-%m', {time_col})"),
            _ => time_col.to_string(),
        },
        RollupLevel::Hourly => {
            let hours = tz_offset_min / 60;
            let mins = (tz_offset_min % 60).abs();
            let sign = if tz_offset_min >= 0 { "+" } else { "-" };
            let tz_adjust = if tz_offset_min != 0 {
                format!(
                    "datetime({time_col}, '{}{:02}:{:02}')",
                    sign,
                    hours.abs(),
                    mins
                )
            } else {
                time_col.to_string()
            };
            match granularity {
                "hour" => format!("strftime('%H:00', {})", tz_adjust),
                "month" => format!("strftime('%Y-%m', {})", tz_adjust),
                _ => format!("date({})", tz_adjust),
            }
        }
    };

    let sql = format!(
        "SELECT {group_expr} as bucket,
                COALESCE(SUM(message_count), 0) as cnt,
                COALESCE(SUM(input_tokens), 0) as inp,
                COALESCE(SUM(output_tokens), 0) as outp,
                COALESCE(SUM(cost_cents), 0.0) as cost
         FROM {} {where_clause}
         GROUP BY bucket
         ORDER BY bucket",
        rollup_table(window.level)
    );
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(ActivityBucket {
                label: row.get(0)?,
                message_count: row.get(1)?,
                tool_call_count: 0,
                input_tokens: row.get(2)?,
                output_tokens: row.get(3)?,
                cost_cents: row.get(4)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

pub fn activity_chart_with_filters(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    filters: &DimensionFilters,
    granularity: &str,
    tz_offset_min: i32,
) -> Result<Vec<ActivityBucket>> {
    if rollups_available(conn) {
        let prefer_daily = tz_offset_min == 0 && granularity != "hour";
        if let Some(window) = choose_rollup_window(since, until, prefer_daily) {
            return activity_chart_from_rollups(conn, &window, filters, granularity, tz_offset_min);
        }
    }

    let mut conditions = vec!["role = 'assistant'".to_string()];
    let mut param_values = Vec::new();
    if let Some(s) = since
        && is_valid_timestamp(s)
    {
        param_values.push(s.to_string());
        conditions.push(format!("timestamp >= ?{}", param_values.len()));
    }
    if let Some(u) = until
        && is_valid_timestamp(u)
    {
        param_values.push(u.to_string());
        conditions.push(format!("timestamp < ?{}", param_values.len()));
    }
    let model_expr = normalized_model_expr("model");
    let project_expr = normalized_project_expr("repo_id");
    let branch_expr = normalized_branch_expr("git_branch");
    let surface_expr = normalized_surface_expr("surface");
    apply_dimension_filters(
        &mut conditions,
        &mut param_values,
        filters,
        "COALESCE(provider, 'claude_code')",
        &model_expr,
        &project_expr,
        &branch_expr,
        &surface_expr,
    );
    let where_clause = format!("WHERE {}", conditions.join(" AND "));
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    // Apply timezone offset to get local time grouping
    let hours = tz_offset_min / 60;
    let mins = (tz_offset_min % 60).abs();
    let sign = if tz_offset_min >= 0 { "+" } else { "-" };
    let tz_adjust = if tz_offset_min != 0 {
        format!(
            "datetime(timestamp, '{}{:02}:{:02}')",
            sign,
            hours.abs(),
            mins
        )
    } else {
        "timestamp".to_string()
    };

    // Only fixed literals reach SQL here (daemon HTTP layer allowlists granularity).
    let group_expr = match granularity {
        "hour" => format!("strftime('%H:00', {})", tz_adjust),
        "month" => format!("strftime('%Y-%m', {})", tz_adjust),
        // "day", "week", and internal callers: calendar-day buckets
        _ => format!("date({})", tz_adjust),
    };

    let sql = format!(
        "SELECT {group_expr} as bucket, COUNT(*) as cnt,
                COALESCE(SUM(input_tokens), 0) as inp,
                COALESCE(SUM(output_tokens), 0) as outp,
                COALESCE(SUM(cost_cents), 0.0) as cost
         FROM messages {where_clause}
         GROUP BY bucket
         ORDER BY bucket",
    );

    let mut stmt = conn.prepare(&sql)?;
    let results = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(ActivityBucket {
                label: row.get(0)?,
                message_count: row.get(1)?,
                input_tokens: row.get(2)?,
                output_tokens: row.get(3)?,
                cost_cents: row.get(4)?,
                tool_call_count: 0,
            })
        })?
        .filter_map(|r| match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("skipping row: {e}");
                None
            }
        })
        .collect();

    Ok(results)
}

// ---------------------------------------------------------------------------
// Branch Cost
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BranchCost {
    pub git_branch: String,
    pub repo_id: String,
    pub session_count: u64,
    pub message_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cost_cents: f64,
}

/// Query cost grouped by branch+repo using the denormalized git_branch column.
/// Groups by (git_branch, repo_id) so branches with the same name in different repos
/// are kept separate.
pub fn branch_cost(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    limit: usize,
) -> Result<Vec<BranchCost>> {
    let filters = DimensionFilters::default();
    branch_cost_with_filters(conn, since, until, &filters, limit)
}

pub fn branch_cost_with_filters(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    filters: &DimensionFilters,
    limit: usize,
) -> Result<Vec<BranchCost>> {
    let mut conditions = vec!["role = 'assistant'".to_string()];
    let mut param_values: Vec<String> = Vec::new();
    let mut idx = 0usize;

    if let Some(s) = since {
        idx += 1;
        conditions.push(format!("timestamp >= ?{idx}"));
        param_values.push(s.to_string());
    }
    if let Some(u) = until {
        idx += 1;
        conditions.push(format!("timestamp < ?{idx}"));
        param_values.push(u.to_string());
    }
    let model_expr = normalized_model_expr("model");
    let project_expr = normalized_project_expr("repo_id");
    let branch_expr = normalized_branch_expr("git_branch");
    let surface_expr = normalized_surface_expr("surface");
    apply_dimension_filters(
        &mut conditions,
        &mut param_values,
        filters,
        "COALESCE(provider, 'claude_code')",
        &model_expr,
        &project_expr,
        &branch_expr,
        &surface_expr,
    );
    param_values.push(limit.to_string());
    let limit_idx = param_values.len();

    let where_clause = format!("WHERE {}", conditions.join(" AND "));
    // Single-query approach: COALESCE NULL/empty branches into "(untagged)"
    let sql = format!(
        "SELECT COALESCE(NULLIF(git_branch, ''), '(untagged)') as branch,
                CASE WHEN COALESCE(NULLIF(git_branch, ''), '(untagged)') = '(untagged)'
                     THEN '' ELSE COALESCE(repo_id, '') END as repo,
                COUNT(DISTINCT session_id) as sess,
                COUNT(*) as cnt,
                COALESCE(SUM(input_tokens), 0) as inp,
                COALESCE(SUM(output_tokens), 0) as outp,
                COALESCE(SUM(cache_read_tokens), 0) as cache_r,
                COALESCE(SUM(cache_creation_tokens), 0) as cache_c,
                COALESCE(SUM(cost_cents), 0.0) as cost
         FROM messages
         {where_clause}
         GROUP BY branch, repo
         ORDER BY cost DESC
         LIMIT ?{limit_idx}",
    );

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<BranchCost> = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(BranchCost {
                git_branch: row.get(0)?,
                repo_id: row.get(1)?,
                session_count: row.get(2)?,
                message_count: row.get(3)?,
                input_tokens: row.get(4)?,
                output_tokens: row.get(5)?,
                cache_read_tokens: row.get(6)?,
                cache_creation_tokens: row.get(7)?,
                cost_cents: row.get(8)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(rows)
}

/// Query cost for a single branch using a dedicated SQL query.
/// Unlike `branch_cost()` (which has LIMIT 20), this searches all branches.
pub fn branch_cost_single(
    conn: &Connection,
    branch: &str,
    repo_id: Option<&str>,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Option<BranchCost>> {
    let branch_stripped = branch.strip_prefix("refs/heads/").unwrap_or(branch);
    let branch_with_ref = format!("refs/heads/{branch_stripped}");

    let mut conditions = vec![
        "role = 'assistant'".to_string(),
        "(git_branch = ?1 OR git_branch = ?2)".to_string(),
    ];
    let mut param_values: Vec<String> = vec![branch_stripped.to_string(), branch_with_ref];
    let mut idx = 2usize;

    if let Some(repo) = repo_id {
        idx += 1;
        conditions.push(format!("COALESCE(repo_id, '') = ?{idx}"));
        param_values.push(repo.to_string());
    }

    if let Some(s) = since {
        idx += 1;
        conditions.push(format!("timestamp >= ?{idx}"));
        param_values.push(s.to_string());
    }
    if let Some(u) = until {
        idx += 1;
        conditions.push(format!("timestamp < ?{idx}"));
        param_values.push(u.to_string());
    }

    let where_clause = format!("WHERE {}", conditions.join(" AND "));
    let sql = if repo_id.is_some() {
        format!(
            "SELECT ?1 as git_branch, COALESCE(repo_id, '') as repo,
                    COUNT(DISTINCT session_id) as sess,
                    COUNT(*) as cnt,
                    COALESCE(SUM(input_tokens), 0) as inp,
                    COALESCE(SUM(output_tokens), 0) as outp,
                    COALESCE(SUM(cache_read_tokens), 0) as cache_r,
                    COALESCE(SUM(cache_creation_tokens), 0) as cache_c,
                    COALESCE(SUM(cost_cents), 0.0) as cost
             FROM messages
             {where_clause}
             GROUP BY COALESCE(repo_id, '')
             LIMIT 1",
        )
    } else {
        format!(
            "SELECT ?1 as git_branch,
                    CASE WHEN COUNT(DISTINCT COALESCE(repo_id, '')) = 1
                         THEN COALESCE(MIN(repo_id), '')
                         ELSE '' END as repo,
                    COUNT(DISTINCT session_id) as sess,
                    COUNT(*) as cnt,
                    COALESCE(SUM(input_tokens), 0) as inp,
                    COALESCE(SUM(output_tokens), 0) as outp,
                    COALESCE(SUM(cache_read_tokens), 0) as cache_r,
                    COALESCE(SUM(cache_creation_tokens), 0) as cache_c,
                    COALESCE(SUM(cost_cents), 0.0) as cost
             FROM messages
             {where_clause}
             GROUP BY ?1
             LIMIT 1",
        )
    };

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map(param_refs.as_slice(), |row| {
        Ok(BranchCost {
            git_branch: row.get(0)?,
            repo_id: row.get(1)?,
            session_count: row.get(2)?,
            message_count: row.get(3)?,
            input_tokens: row.get(4)?,
            output_tokens: row.get(5)?,
            cache_read_tokens: row.get(6)?,
            cache_creation_tokens: row.get(7)?,
            cost_cents: row.get(8)?,
        })
    })?;

    match rows.next() {
        Some(Ok(bc)) => Ok(Some(bc)),
        Some(Err(e)) => Err(e.into()),
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Tags
// ---------------------------------------------------------------------------

/// A single tag key-value pair.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionTag {
    pub key: String,
    pub value: String,
}

/// Tag-based cost breakdown: cost grouped by tag key+value.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TagCost {
    pub key: String,
    pub value: String,
    pub session_count: u64,
    pub cost_cents: f64,
}

/// Query cost breakdown by tag, optionally filtered by tag key and date range.
/// Cost is per-message: sums cost_cents of all messages in sessions carrying each tag.
pub fn tag_stats(
    conn: &Connection,
    tag_key: Option<&str>,
    since: Option<&str>,
    until: Option<&str>,
    limit: usize,
) -> Result<Vec<TagCost>> {
    let filters = DimensionFilters::default();
    tag_stats_with_filters(conn, tag_key, since, until, &filters, limit)
}

pub fn tag_stats_with_filters(
    conn: &Connection,
    tag_key: Option<&str>,
    since: Option<&str>,
    until: Option<&str>,
    filters: &DimensionFilters,
    limit: usize,
) -> Result<Vec<TagCost>> {
    // Repo/branch attribution must come from message columns, not tag fanout.
    // This guarantees one message contributes its full cost to its real repo/branch,
    // even if a message carries extra tags with the same key.
    if let Some(key) = tag_key {
        match key {
            "repo" | "repo_id" => {
                return tag_stats_repo_from_messages(conn, key, since, until, filters, limit);
            }
            "branch" | "git_branch" => {
                return tag_stats_branch_from_messages(conn, key, since, until, filters, limit);
            }
            _ => {}
        }
    }

    let mut param_values: Vec<String> = Vec::new();
    let mut idx = 0usize;

    // Build WHERE conditions for the main query (tags t JOIN messages m)
    let mut where_parts = vec!["m.role = 'assistant'".to_string()];

    if let Some(k) = tag_key {
        idx += 1;
        param_values.push(k.to_string());
        where_parts.push(format!("t.key = ?{idx}"));
    }
    if let Some(s) = since {
        idx += 1;
        param_values.push(s.to_string());
        where_parts.push(format!("m.timestamp >= ?{idx}"));
    }
    if let Some(u) = until {
        idx += 1;
        param_values.push(u.to_string());
        where_parts.push(format!("m.timestamp < ?{idx}"));
    }
    let model_expr = normalized_model_expr("m.model");
    let project_expr = normalized_project_expr("m.repo_id");
    let branch_expr = normalized_branch_expr("m.git_branch");
    let surface_expr = normalized_surface_expr("m.surface");
    apply_dimension_filters(
        &mut where_parts,
        &mut param_values,
        filters,
        "COALESCE(m.provider, 'claude_code')",
        &model_expr,
        &project_expr,
        &branch_expr,
        &surface_expr,
    );
    param_values.push(limit.to_string());
    let limit_idx = param_values.len();

    let where_clause = format!("WHERE {}", where_parts.join(" AND "));

    // Build the untagged UNION clause for single-key queries.
    // Note: the UNION reuses the same positional params as the main query.
    // ?1 is always the tag key (first param pushed when tag_key is Some).
    let untagged_union = if let Some(k) = tag_key {
        let mut date_parts = Vec::new();
        {
            let mut uidx = 0usize;
            if tag_key.is_some() {
                uidx += 1; // ?1 = tag key
            }
            if since.is_some() {
                uidx += 1;
                date_parts.push(format!("m.timestamp >= ?{uidx}"));
            }
            if until.is_some() {
                uidx += 1;
                date_parts.push(format!("m.timestamp < ?{uidx}"));
            }
        }
        let date_filter = if date_parts.is_empty() {
            String::new()
        } else {
            format!("AND {}", date_parts.join(" AND "))
        };
        format!(
            "UNION ALL
             SELECT '{k}' as key, '(untagged)' as value, 0 as session_count,
                    COALESCE(SUM(m.cost_cents), 0.0) as total_cost_cents
             FROM messages m
             WHERE m.role = 'assistant' {date_filter}
               AND NOT EXISTS (
                 SELECT 1 FROM tags t2
                 WHERE t2.message_id = m.id AND t2.key = ?1
               )"
        )
    } else {
        String::new()
    };

    // When a specific key is requested, use proportional splitting so that
    // multi-value tags (e.g. two ticket IDs on one message) split cost fairly.
    // The all-keys overview uses a direct sum — 2x faster on 500K+ rows.
    // NOTE: the all-keys path shows per-key totals; since one message carries
    // multiple keys (provider, model, tool, …), cost appears under each key
    // independently. This is intentional — callers should filter by key for
    // accurate per-value cost attribution.
    let sql = if tag_key.is_some() {
        format!(
            "WITH msg_val_counts AS (
                 SELECT message_id, COUNT(*) as n_values
                 FROM tags
                 WHERE key = ?1
                 GROUP BY message_id
             )
             SELECT t.key, t.value,
                    COUNT(DISTINCT m.session_id) as session_count,
                    COALESCE(SUM(m.cost_cents / mvc.n_values), 0.0) as total_cost_cents
             FROM tags t
             JOIN msg_val_counts mvc ON mvc.message_id = t.message_id
             JOIN messages m ON t.message_id = m.id
             {where_clause}
             GROUP BY t.key, t.value
             {untagged_union}
             ORDER BY total_cost_cents DESC
             LIMIT ?{limit_idx}",
        )
    } else {
        format!(
            "SELECT t.key, t.value,
                    COUNT(DISTINCT m.session_id) as session_count,
                    COALESCE(SUM(m.cost_cents), 0.0) as total_cost_cents
             FROM tags t
             JOIN messages m ON t.message_id = m.id
             {where_clause}
             GROUP BY t.key, t.value
             ORDER BY total_cost_cents DESC
             LIMIT ?{limit_idx}",
        )
    };

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<TagCost> = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(TagCost {
                key: row.get(0)?,
                value: row.get(1)?,
                session_count: row.get(2)?,
                cost_cents: row.get(3)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(rows)
}

fn tag_stats_repo_from_messages(
    conn: &Connection,
    key_label: &str,
    since: Option<&str>,
    until: Option<&str>,
    filters: &DimensionFilters,
    limit: usize,
) -> Result<Vec<TagCost>> {
    let mut conditions = vec!["role = 'assistant'".to_string()];
    let mut param_values: Vec<String> = Vec::new();
    let mut idx = 0usize;

    if let Some(s) = since {
        idx += 1;
        conditions.push(format!("timestamp >= ?{idx}"));
        param_values.push(s.to_string());
    }
    if let Some(u) = until {
        idx += 1;
        conditions.push(format!("timestamp < ?{idx}"));
        param_values.push(u.to_string());
    }
    let model_expr = normalized_model_expr("model");
    let project_expr = normalized_project_expr("repo_id");
    let branch_expr = normalized_branch_expr("git_branch");
    let surface_expr = normalized_surface_expr("surface");
    apply_dimension_filters(
        &mut conditions,
        &mut param_values,
        filters,
        "COALESCE(provider, 'claude_code')",
        &model_expr,
        &project_expr,
        &branch_expr,
        &surface_expr,
    );
    param_values.push(limit.to_string());
    let limit_idx = param_values.len();

    let where_clause = format!("WHERE {}", conditions.join(" AND "));
    let sql = format!(
        "SELECT '{key_label}' as key,
                COALESCE(NULLIF(NULLIF(repo_id, ''), 'unknown'), '(untagged)') as value,
                COUNT(DISTINCT session_id) as session_count,
                COALESCE(SUM(cost_cents), 0.0) as total_cost_cents
         FROM messages
         {where_clause}
         GROUP BY value
         ORDER BY total_cost_cents DESC
         LIMIT ?{limit_idx}",
    );

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(TagCost {
                key: row.get(0)?,
                value: row.get(1)?,
                session_count: row.get(2)?,
                cost_cents: row.get(3)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

fn tag_stats_branch_from_messages(
    conn: &Connection,
    key_label: &str,
    since: Option<&str>,
    until: Option<&str>,
    filters: &DimensionFilters,
    limit: usize,
) -> Result<Vec<TagCost>> {
    let mut conditions = vec!["role = 'assistant'".to_string()];
    let mut param_values: Vec<String> = Vec::new();
    let mut idx = 0usize;

    if let Some(s) = since {
        idx += 1;
        conditions.push(format!("timestamp >= ?{idx}"));
        param_values.push(s.to_string());
    }
    if let Some(u) = until {
        idx += 1;
        conditions.push(format!("timestamp < ?{idx}"));
        param_values.push(u.to_string());
    }
    let model_expr = normalized_model_expr("model");
    let project_expr = normalized_project_expr("repo_id");
    let branch_expr = normalized_branch_expr("git_branch");
    let surface_expr = normalized_surface_expr("surface");
    apply_dimension_filters(
        &mut conditions,
        &mut param_values,
        filters,
        "COALESCE(provider, 'claude_code')",
        &model_expr,
        &project_expr,
        &branch_expr,
        &surface_expr,
    );
    param_values.push(limit.to_string());
    let limit_idx = param_values.len();

    let where_clause = format!("WHERE {}", conditions.join(" AND "));
    let sql = format!(
        "SELECT '{key_label}' as key,
                COALESCE(
                    NULLIF(
                        CASE
                            WHEN git_branch LIKE 'refs/heads/%' THEN SUBSTR(git_branch, 12)
                            ELSE git_branch
                        END,
                        ''
                    ),
                    '(untagged)'
                ) as value,
                COUNT(DISTINCT session_id) as session_count,
                COALESCE(SUM(cost_cents), 0.0) as total_cost_cents
         FROM messages
         {where_clause}
         GROUP BY value
         ORDER BY total_cost_cents DESC
         LIMIT ?{limit_idx}",
    );

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(TagCost {
                key: row.get(0)?,
                value: row.get(1)?,
                session_count: row.get(2)?,
                cost_cents: row.get(3)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

// ---------------------------------------------------------------------------
// Ticket Cost
// ---------------------------------------------------------------------------

/// Per-ticket aggregate cost row used by `GET /analytics/tickets`
/// and the `budi stats --tickets` CLI view.
///
/// Tickets are sourced from the `ticket_id` tag emitted by `GitEnricher`
/// when a recognised ID appears in `git_branch`. Messages with no `ticket_id`
/// tag collapse into a single `(untagged)` bucket so the total stays whole.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TicketCost {
    pub ticket_id: String,
    /// Prefix of the ticket id, e.g. `ENG` for `ENG-123`. Empty when
    /// the value has no `-` (covers the `(untagged)` row).
    pub ticket_prefix: String,
    pub session_count: u64,
    pub message_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cost_cents: f64,
    /// Dominant branch (highest cost) carrying this ticket. Empty for the
    /// `(untagged)` row.
    #[serde(default)]
    pub top_branch: String,
    /// Dominant repo (highest cost) carrying this ticket. Empty for the
    /// `(untagged)` row.
    #[serde(default)]
    pub top_repo_id: String,
    /// Where the ticket id was derived from — `"branch"` (alphanumeric
    /// pattern) or `"branch_numeric"` (ADR-0082 §9 fallback). Empty for
    /// the `(untagged)` row. Legacy rows with no `ticket_source` sibling
    /// tag fall back to `"branch"` so older DBs stay readable. See R1.3
    /// (#221).
    #[serde(default)]
    pub source: String,
}

/// Per-branch breakdown attached to a single ticket detail response.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TicketBranchBreakdown {
    pub git_branch: String,
    pub repo_id: String,
    pub message_count: u64,
    pub session_count: u64,
    pub cost_cents: f64,
}

/// Detail payload for `GET /analytics/tickets/{ticket_id}` and
/// `budi stats --ticket <ID>`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TicketCostDetail {
    pub ticket_id: String,
    pub ticket_prefix: String,
    pub session_count: u64,
    pub message_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cost_cents: f64,
    /// Dominant repo (or empty when ambiguous / unattributed).
    pub repo_id: String,
    /// Per-branch attribution for cost charged to this ticket.
    pub branches: Vec<TicketBranchBreakdown>,
    /// Where the ticket id was derived from. See `TicketCost::source`.
    #[serde(default)]
    pub source: String,
}

const TICKET_TAG_KEY: &str = "ticket_id";
const TICKET_SOURCE_TAG_KEY: &str = "ticket_source";

/// Canonical fallback source for legacy rows (pre-R1.3) that carry a
/// `ticket_id` tag but no sibling `ticket_source` tag. The alphanumeric
/// extractor was the only producer before R1.3; the numeric fallback
/// shipped later with the unified extractor. This default keeps older
/// analytics readable without a reindex.
pub const TICKET_SOURCE_BRANCH: &str = crate::pipeline::TICKET_SOURCE_BRANCH;

/// Query cost grouped by ticket, sorted by cost descending. Includes an
/// `(untagged)` bucket for assistant messages that have no `ticket_id` tag.
pub fn ticket_cost(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    limit: usize,
) -> Result<Vec<TicketCost>> {
    let filters = DimensionFilters::default();
    ticket_cost_with_filters(conn, since, until, &filters, limit)
}

pub fn ticket_cost_with_filters(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    filters: &DimensionFilters,
    limit: usize,
) -> Result<Vec<TicketCost>> {
    // Tagged path: join messages → tags(ticket_id) and split cost
    // proportionally when one message carries multiple ticket IDs (rare,
    // but matches the existing tag_stats behaviour for fairness).
    let mut conditions = vec!["m.role = 'assistant'".to_string()];
    let mut param_values: Vec<String> = Vec::new();
    let mut idx = 0usize;
    if let Some(s) = since {
        idx += 1;
        param_values.push(s.to_string());
        conditions.push(format!("m.timestamp >= ?{idx}"));
    }
    if let Some(u) = until {
        idx += 1;
        param_values.push(u.to_string());
        conditions.push(format!("m.timestamp < ?{idx}"));
    }
    let model_expr = normalized_model_expr("m.model");
    let project_expr = normalized_project_expr("m.repo_id");
    let branch_expr = normalized_branch_expr("m.git_branch");
    let surface_expr = normalized_surface_expr("m.surface");
    apply_dimension_filters(
        &mut conditions,
        &mut param_values,
        filters,
        "COALESCE(m.provider, 'claude_code')",
        &model_expr,
        &project_expr,
        &branch_expr,
        &surface_expr,
    );
    let where_clause = format!("WHERE {}", conditions.join(" AND "));

    // Build the (untagged) UNION clause — assistant messages that have no
    // ticket_id tag at all, after dimension/date filters.
    let untagged_conditions: Vec<String> = conditions
        .iter()
        .map(|c| c.replace("m.role = 'assistant'", "m2.role = 'assistant'"))
        .collect();
    // The above only renames the role predicate; date and dimension predicates
    // already reference `m.*` columns, so re-alias the table prefix as well.
    let untagged_conditions: Vec<String> = untagged_conditions
        .into_iter()
        .map(|c| c.replace("m.", "m2."))
        .collect();
    let untagged_where = format!("WHERE {}", untagged_conditions.join(" AND "));

    let limit_param_idx = param_values.len() + 1;
    param_values.push(limit.to_string());

    let sql = format!(
        "WITH msg_val_counts AS (
             SELECT message_id, COUNT(*) AS n_values
             FROM tags
             WHERE key = '{TICKET_TAG_KEY}'
             GROUP BY message_id
         ),
         msg_source AS (
             SELECT message_id, MIN(value) AS source_value
             FROM tags
             WHERE key = '{TICKET_SOURCE_TAG_KEY}'
             GROUP BY message_id
         ),
         tagged AS (
             SELECT t.value AS ticket_id,
                    m.session_id,
                    m.repo_id,
                    m.git_branch,
                    m.input_tokens,
                    m.output_tokens,
                    m.cache_read_tokens,
                    m.cache_creation_tokens,
                    m.cost_cents,
                    mvc.n_values,
                    COALESCE(ms.source_value, '') AS ticket_source
             FROM tags t
             JOIN msg_val_counts mvc ON mvc.message_id = t.message_id
             JOIN messages m ON m.id = t.message_id
             LEFT JOIN msg_source ms ON ms.message_id = t.message_id
             {where_clause}
             AND t.key = '{TICKET_TAG_KEY}'
         ),
         per_ticket AS (
             SELECT ticket_id,
                    COUNT(DISTINCT session_id) AS sess,
                    COUNT(*) AS cnt,
                    COALESCE(SUM(input_tokens / n_values), 0) AS inp,
                    COALESCE(SUM(output_tokens / n_values), 0) AS outp,
                    COALESCE(SUM(cache_read_tokens / n_values), 0) AS cache_r,
                    COALESCE(SUM(cache_creation_tokens / n_values), 0) AS cache_c,
                    COALESCE(SUM(cost_cents / n_values), 0.0) AS cost
             FROM tagged
             GROUP BY ticket_id
         ),
         top_branch AS (
             SELECT ticket_id,
                    CASE
                        WHEN COALESCE(git_branch, '') LIKE 'refs/heads/%'
                            THEN SUBSTR(COALESCE(git_branch, ''), 12)
                        ELSE COALESCE(git_branch, '')
                    END AS branch_value,
                    SUM(cost_cents / n_values) AS branch_cost
             FROM tagged
             GROUP BY ticket_id, branch_value
         ),
         top_branch_pick AS (
             SELECT ticket_id, branch_value
             FROM (
                 SELECT ticket_id, branch_value, branch_cost,
                        ROW_NUMBER() OVER (
                            PARTITION BY ticket_id
                            ORDER BY branch_cost DESC, branch_value ASC
                        ) AS rn
                 FROM top_branch
                 WHERE branch_value != ''
             )
             WHERE rn = 1
         ),
         top_repo AS (
             SELECT ticket_id,
                    COALESCE(repo_id, '') AS repo_value,
                    SUM(cost_cents / n_values) AS repo_cost
             FROM tagged
             GROUP BY ticket_id, repo_value
         ),
         top_repo_pick AS (
             SELECT ticket_id, repo_value
             FROM (
                 SELECT ticket_id, repo_value, repo_cost,
                        ROW_NUMBER() OVER (
                            PARTITION BY ticket_id
                            ORDER BY repo_cost DESC, repo_value ASC
                        ) AS rn
                 FROM top_repo
                 WHERE repo_value != '' AND repo_value != 'unknown'
             )
             WHERE rn = 1
         ),
         top_source AS (
             SELECT ticket_id,
                    ticket_source AS source_value,
                    SUM(cost_cents / n_values) AS source_cost
             FROM tagged
             GROUP BY ticket_id, source_value
         ),
         top_source_pick AS (
             SELECT ticket_id, source_value
             FROM (
                 SELECT ticket_id, source_value, source_cost,
                        ROW_NUMBER() OVER (
                            PARTITION BY ticket_id
                            ORDER BY source_cost DESC, source_value ASC
                        ) AS rn
                 FROM top_source
                 WHERE source_value != ''
             )
             WHERE rn = 1
         )
         SELECT pt.ticket_id,
                pt.sess, pt.cnt,
                pt.inp, pt.outp, pt.cache_r, pt.cache_c, pt.cost,
                COALESCE(tbp.branch_value, '') AS top_branch,
                COALESCE(trp.repo_value, '') AS top_repo,
                COALESCE(tsp.source_value, '{TICKET_SOURCE_BRANCH}') AS ticket_source
         FROM per_ticket pt
         LEFT JOIN top_branch_pick tbp ON tbp.ticket_id = pt.ticket_id
         LEFT JOIN top_repo_pick trp ON trp.ticket_id = pt.ticket_id
         LEFT JOIN top_source_pick tsp ON tsp.ticket_id = pt.ticket_id

         UNION ALL

         SELECT '{UNTAGGED_DIMENSION}' AS ticket_id,
                COUNT(DISTINCT m2.session_id) AS sess,
                COUNT(*) AS cnt,
                COALESCE(SUM(m2.input_tokens), 0) AS inp,
                COALESCE(SUM(m2.output_tokens), 0) AS outp,
                COALESCE(SUM(m2.cache_read_tokens), 0) AS cache_r,
                COALESCE(SUM(m2.cache_creation_tokens), 0) AS cache_c,
                COALESCE(SUM(m2.cost_cents), 0.0) AS cost,
                '' AS top_branch,
                '' AS top_repo,
                '' AS ticket_source
         FROM messages m2
         {untagged_where}
         AND NOT EXISTS (
             SELECT 1 FROM tags t2
             WHERE t2.message_id = m2.id AND t2.key = '{TICKET_TAG_KEY}'
         )

         ORDER BY cost DESC
         LIMIT ?{limit_param_idx}",
    );

    // (untagged) sub-query reuses the same positional date/dimension params,
    // so the param list is shared 1:1.
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<TicketCost> = stmt
        .query_map(param_refs.as_slice(), |row| {
            let ticket_id: String = row.get(0)?;
            let ticket_prefix = ticket_prefix_of(&ticket_id);
            Ok(TicketCost {
                ticket_id,
                ticket_prefix,
                session_count: row.get(1)?,
                message_count: row.get(2)?,
                input_tokens: row.get(3)?,
                output_tokens: row.get(4)?,
                cache_read_tokens: row.get(5)?,
                cache_creation_tokens: row.get(6)?,
                cost_cents: row.get(7)?,
                top_branch: row.get(8)?,
                top_repo_id: row.get(9)?,
                source: row.get(10)?,
            })
        })?
        .filter_map(|r| r.ok())
        // Drop the (untagged) row when it carries zero cost AND zero messages
        // to avoid noise on a freshly-imported DB.
        .filter(|tc| !(tc.ticket_id == UNTAGGED_DIMENSION && tc.message_count == 0))
        .collect();

    Ok(rows)
}

/// Detail view for a single ticket: totals, dominant repo, and per-branch
/// breakdown. Returns `None` when no assistant messages carry the ticket
/// in the requested window.
pub fn ticket_cost_single(
    conn: &Connection,
    ticket_id: &str,
    repo_id: Option<&str>,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Option<TicketCostDetail>> {
    let mut conditions = vec![
        "m.role = 'assistant'".to_string(),
        "t.key = ?1".to_string(),
        "t.value = ?2".to_string(),
    ];
    let mut param_values: Vec<String> = vec![TICKET_TAG_KEY.to_string(), ticket_id.to_string()];
    let mut idx = 2usize;

    if let Some(repo) = repo_id {
        idx += 1;
        param_values.push(repo.to_string());
        conditions.push(format!("COALESCE(m.repo_id, '') = ?{idx}"));
    }
    if let Some(s) = since {
        idx += 1;
        param_values.push(s.to_string());
        conditions.push(format!("m.timestamp >= ?{idx}"));
    }
    if let Some(u) = until {
        idx += 1;
        param_values.push(u.to_string());
        conditions.push(format!("m.timestamp < ?{idx}"));
    }
    let where_clause = format!("WHERE {}", conditions.join(" AND "));

    // Totals. Use the same proportional split on multi-value ticket tags.
    // `source` picks the dominant `ticket_source` sibling tag across the
    // selected messages (by cost, then name). Legacy rows without a
    // `ticket_source` tag fall back to the alphanumeric `branch` source in
    // the caller so the detail view always has something to print.
    let totals_sql = format!(
        "WITH msg_val_counts AS (
             SELECT message_id, COUNT(*) AS n_values
             FROM tags
             WHERE key = ?1
             GROUP BY message_id
         ),
         msg_source AS (
             SELECT message_id, MIN(value) AS source_value
             FROM tags
             WHERE key = '{TICKET_SOURCE_TAG_KEY}'
             GROUP BY message_id
         ),
         selected AS (
             SELECT m.id AS message_id,
                    m.session_id,
                    m.repo_id,
                    m.input_tokens,
                    m.output_tokens,
                    m.cache_read_tokens,
                    m.cache_creation_tokens,
                    m.cost_cents,
                    mvc.n_values,
                    COALESCE(ms.source_value, '') AS ticket_source
             FROM tags t
             JOIN msg_val_counts mvc ON mvc.message_id = t.message_id
             JOIN messages m ON m.id = t.message_id
             LEFT JOIN msg_source ms ON ms.message_id = t.message_id
             {where_clause}
         ),
         source_pick AS (
             SELECT ticket_source,
                    SUM(cost_cents / n_values) AS source_cost
             FROM selected
             WHERE ticket_source != ''
             GROUP BY ticket_source
             ORDER BY source_cost DESC, ticket_source ASC
             LIMIT 1
         )
         SELECT COUNT(DISTINCT session_id) AS sess,
                COUNT(*) AS cnt,
                COALESCE(SUM(input_tokens / n_values), 0) AS inp,
                COALESCE(SUM(output_tokens / n_values), 0) AS outp,
                COALESCE(SUM(cache_read_tokens / n_values), 0) AS cache_r,
                COALESCE(SUM(cache_creation_tokens / n_values), 0) AS cache_c,
                COALESCE(SUM(cost_cents / n_values), 0.0) AS cost,
                CASE WHEN COUNT(DISTINCT COALESCE(repo_id, '')) = 1
                     THEN COALESCE(MIN(repo_id), '')
                     ELSE '' END AS repo,
                COALESCE((SELECT ticket_source FROM source_pick), '') AS src
         FROM selected",
    );

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();
    let mut stmt = conn.prepare(&totals_sql)?;
    let totals = stmt.query_row(param_refs.as_slice(), |row| {
        Ok((
            row.get::<_, u64>(0)?,
            row.get::<_, u64>(1)?,
            row.get::<_, u64>(2)?,
            row.get::<_, u64>(3)?,
            row.get::<_, u64>(4)?,
            row.get::<_, u64>(5)?,
            row.get::<_, f64>(6)?,
            row.get::<_, String>(7)?,
            row.get::<_, String>(8)?,
        ))
    });
    let (sess, cnt, inp, outp, cache_r, cache_c, cost, repo, src) = match totals {
        Ok(row) => row,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if cnt == 0 {
        return Ok(None);
    }

    // Per-branch breakdown — same proportional split.
    let branches_sql = format!(
        "WITH msg_val_counts AS (
             SELECT message_id, COUNT(*) AS n_values
             FROM tags
             WHERE key = ?1
             GROUP BY message_id
         )
         SELECT COALESCE(NULLIF(
                    CASE
                        WHEN COALESCE(m.git_branch, '') LIKE 'refs/heads/%'
                            THEN SUBSTR(COALESCE(m.git_branch, ''), 12)
                        ELSE COALESCE(m.git_branch, '')
                    END,
                    ''
                ), '{UNTAGGED_DIMENSION}') AS branch_value,
                COALESCE(m.repo_id, '') AS repo_value,
                COUNT(DISTINCT m.session_id) AS sess,
                COUNT(*) AS cnt,
                COALESCE(SUM(m.cost_cents / mvc.n_values), 0.0) AS cost
         FROM tags t
         JOIN msg_val_counts mvc ON mvc.message_id = t.message_id
         JOIN messages m ON m.id = t.message_id
         {where_clause}
         GROUP BY branch_value, repo_value
         ORDER BY cost DESC, branch_value ASC",
    );
    let mut stmt = conn.prepare(&branches_sql)?;
    let branches: Vec<TicketBranchBreakdown> = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(TicketBranchBreakdown {
                git_branch: row.get(0)?,
                repo_id: row.get(1)?,
                session_count: row.get(2)?,
                message_count: row.get(3)?,
                cost_cents: row.get(4)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    // Legacy rows lack a `ticket_source` sibling tag; before R1.3
    // (#221) only the alphanumeric extractor produced `ticket_id` tags
    // in pipeline writes, so treat the empty source as `branch` for
    // the detail view.
    let source = if src.is_empty() {
        TICKET_SOURCE_BRANCH.to_string()
    } else {
        src
    };

    Ok(Some(TicketCostDetail {
        ticket_prefix: ticket_prefix_of(ticket_id),
        ticket_id: ticket_id.to_string(),
        session_count: sess,
        message_count: cnt,
        input_tokens: inp,
        output_tokens: outp,
        cache_read_tokens: cache_r,
        cache_creation_tokens: cache_c,
        cost_cents: cost,
        repo_id: repo,
        branches,
        source,
    }))
}

fn ticket_prefix_of(ticket: &str) -> String {
    ticket
        .split_once('-')
        .map(|(prefix, _)| prefix.to_string())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Activities — first-class CLI dimension wired in 8.1 (#305)
//
// Activities come from the `activity` tag emitted by the pipeline when
// `hooks::classify_prompt` recognises an intent in a user prompt (e.g.
// "bugfix", "refactor", "testing"). The intent is propagated across every
// assistant message in the session via `propagate_session_context`, so each
// assistant row either carries exactly one `activity` tag or none at all.
//
// R1.0 treats every aggregate as `source = "rule"` / `confidence = "medium"`
// because today the only producer is the rule-based classifier. R1.2 (#222)
// will extend the classifier and can update these fields per-aggregate
// without breaking the wire format.
// ---------------------------------------------------------------------------

pub(crate) const ACTIVITY_TAG_KEY: &str = crate::tag_keys::ACTIVITY;

/// Canonical classification source label for rule-derived activities.
/// Stays stable across the 8.1 release so dashboards can pin on it; R1.2
/// may introduce additional sources alongside this one.
pub const ACTIVITY_SOURCE_RULE: &str = "rule";

/// Baseline confidence for rule-derived activities in 8.1.
pub const ACTIVITY_CONFIDENCE_MEDIUM: &str = "medium";

/// Per-activity aggregate cost row used by `GET /analytics/activities` and
/// the `budi stats --activities` CLI view.
///
/// Activities are sourced from the `activity` tag emitted by the pipeline's
/// prompt classifier. Messages with no `activity` tag collapse into a single
/// `(untagged)` bucket so the total stays whole (same contract as
/// `ticket_cost`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ActivityCost {
    pub activity: String,
    pub session_count: u64,
    pub message_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cost_cents: f64,
    /// Dominant branch (highest cost) carrying this activity. Empty for
    /// the `(untagged)` row.
    #[serde(default)]
    pub top_branch: String,
    /// Dominant repo (highest cost) carrying this activity. Empty for the
    /// `(untagged)` row.
    #[serde(default)]
    pub top_repo_id: String,
    /// Where this activity label came from. `"rule"` in R1.0; reserved for
    /// future per-aggregate sources in R1.2 (#222).
    #[serde(default)]
    pub source: String,
    /// How confident the aggregate is in the label. `"medium"` baseline in
    /// R1.0; R1.2 may downgrade to `"low"` for ambiguous prompts or promote
    /// to `"high"` when a stronger signal lands. `""` for the
    /// `(untagged)` row to make the absence explicit.
    #[serde(default)]
    pub confidence: String,
}

/// Per-branch breakdown attached to a single activity detail response.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ActivityBranchBreakdown {
    pub git_branch: String,
    pub repo_id: String,
    pub message_count: u64,
    pub session_count: u64,
    pub cost_cents: f64,
}

/// Detail payload for `GET /analytics/activities/{name}` and
/// `budi stats --activity <name>`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ActivityCostDetail {
    pub activity: String,
    pub session_count: u64,
    pub message_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cost_cents: f64,
    /// Dominant repo (empty when ambiguous / unattributed).
    pub repo_id: String,
    /// Per-branch attribution for cost charged to this activity.
    pub branches: Vec<ActivityBranchBreakdown>,
    /// Classification source — see `ActivityCost::source`.
    #[serde(default)]
    pub source: String,
    /// Classification confidence — see `ActivityCost::confidence`.
    #[serde(default)]
    pub confidence: String,
}

/// Query cost grouped by activity, sorted by cost descending. Includes an
/// `(untagged)` bucket for assistant messages that have no `activity` tag.
pub fn activity_cost(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    limit: usize,
) -> Result<Vec<ActivityCost>> {
    let filters = DimensionFilters::default();
    activity_cost_with_filters(conn, since, until, &filters, limit)
}

pub fn activity_cost_with_filters(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    filters: &DimensionFilters,
    limit: usize,
) -> Result<Vec<ActivityCost>> {
    // Tagged path: join messages → tags(activity). An assistant message
    // should carry at most one activity tag (see pipeline contract), but
    // we still divide by n_values defensively so the total reconciles if
    // a future enricher emits more than one value.
    let mut conditions = vec!["m.role = 'assistant'".to_string()];
    let mut param_values: Vec<String> = Vec::new();
    let mut idx = 0usize;
    if let Some(s) = since {
        idx += 1;
        param_values.push(s.to_string());
        conditions.push(format!("m.timestamp >= ?{idx}"));
    }
    if let Some(u) = until {
        idx += 1;
        param_values.push(u.to_string());
        conditions.push(format!("m.timestamp < ?{idx}"));
    }
    let model_expr = normalized_model_expr("m.model");
    let project_expr = normalized_project_expr("m.repo_id");
    let branch_expr = normalized_branch_expr("m.git_branch");
    let surface_expr = normalized_surface_expr("m.surface");
    apply_dimension_filters(
        &mut conditions,
        &mut param_values,
        filters,
        "COALESCE(m.provider, 'claude_code')",
        &model_expr,
        &project_expr,
        &branch_expr,
        &surface_expr,
    );
    let where_clause = format!("WHERE {}", conditions.join(" AND "));

    let untagged_conditions: Vec<String> = conditions
        .iter()
        .map(|c| c.replace("m.role = 'assistant'", "m2.role = 'assistant'"))
        .collect();
    let untagged_conditions: Vec<String> = untagged_conditions
        .into_iter()
        .map(|c| c.replace("m.", "m2."))
        .collect();
    let untagged_where = format!("WHERE {}", untagged_conditions.join(" AND "));

    let limit_param_idx = param_values.len() + 1;
    param_values.push(limit.to_string());

    let sql = format!(
        "WITH msg_val_counts AS (
             SELECT message_id, COUNT(*) AS n_values
             FROM tags
             WHERE key = '{ACTIVITY_TAG_KEY}'
             GROUP BY message_id
         ),
         tagged AS (
             SELECT t.value AS activity,
                    m.session_id,
                    m.repo_id,
                    m.git_branch,
                    m.input_tokens,
                    m.output_tokens,
                    m.cache_read_tokens,
                    m.cache_creation_tokens,
                    m.cost_cents,
                    mvc.n_values
             FROM tags t
             JOIN msg_val_counts mvc ON mvc.message_id = t.message_id
             JOIN messages m ON m.id = t.message_id
             {where_clause}
             AND t.key = '{ACTIVITY_TAG_KEY}'
         ),
         per_activity AS (
             SELECT activity,
                    COUNT(DISTINCT session_id) AS sess,
                    COUNT(*) AS cnt,
                    COALESCE(SUM(input_tokens / n_values), 0) AS inp,
                    COALESCE(SUM(output_tokens / n_values), 0) AS outp,
                    COALESCE(SUM(cache_read_tokens / n_values), 0) AS cache_r,
                    COALESCE(SUM(cache_creation_tokens / n_values), 0) AS cache_c,
                    COALESCE(SUM(cost_cents / n_values), 0.0) AS cost
             FROM tagged
             GROUP BY activity
         ),
         top_branch AS (
             SELECT activity,
                    CASE
                        WHEN COALESCE(git_branch, '') LIKE 'refs/heads/%'
                            THEN SUBSTR(COALESCE(git_branch, ''), 12)
                        ELSE COALESCE(git_branch, '')
                    END AS branch_value,
                    SUM(cost_cents / n_values) AS branch_cost
             FROM tagged
             GROUP BY activity, branch_value
         ),
         top_branch_pick AS (
             SELECT activity, branch_value
             FROM (
                 SELECT activity, branch_value, branch_cost,
                        ROW_NUMBER() OVER (
                            PARTITION BY activity
                            ORDER BY branch_cost DESC, branch_value ASC
                        ) AS rn
                 FROM top_branch
                 WHERE branch_value != ''
             )
             WHERE rn = 1
         ),
         top_repo AS (
             SELECT activity,
                    COALESCE(repo_id, '') AS repo_value,
                    SUM(cost_cents / n_values) AS repo_cost
             FROM tagged
             GROUP BY activity, repo_value
         ),
         top_repo_pick AS (
             SELECT activity, repo_value
             FROM (
                 SELECT activity, repo_value, repo_cost,
                        ROW_NUMBER() OVER (
                            PARTITION BY activity
                            ORDER BY repo_cost DESC, repo_value ASC
                        ) AS rn
                 FROM top_repo
                 WHERE repo_value != '' AND repo_value != 'unknown'
             )
             WHERE rn = 1
         )
         SELECT pa.activity,
                pa.sess, pa.cnt,
                pa.inp, pa.outp, pa.cache_r, pa.cache_c, pa.cost,
                COALESCE(tbp.branch_value, '') AS top_branch,
                COALESCE(trp.repo_value, '') AS top_repo
         FROM per_activity pa
         LEFT JOIN top_branch_pick tbp ON tbp.activity = pa.activity
         LEFT JOIN top_repo_pick trp ON trp.activity = pa.activity

         UNION ALL

         SELECT '{UNTAGGED_DIMENSION}' AS activity,
                COUNT(DISTINCT m2.session_id) AS sess,
                COUNT(*) AS cnt,
                COALESCE(SUM(m2.input_tokens), 0) AS inp,
                COALESCE(SUM(m2.output_tokens), 0) AS outp,
                COALESCE(SUM(m2.cache_read_tokens), 0) AS cache_r,
                COALESCE(SUM(m2.cache_creation_tokens), 0) AS cache_c,
                COALESCE(SUM(m2.cost_cents), 0.0) AS cost,
                '' AS top_branch,
                '' AS top_repo
         FROM messages m2
         {untagged_where}
         AND NOT EXISTS (
             SELECT 1 FROM tags t2
             WHERE t2.message_id = m2.id AND t2.key = '{ACTIVITY_TAG_KEY}'
         )

         ORDER BY cost DESC
         LIMIT ?{limit_param_idx}",
    );

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();
    let label_lookup = load_activity_classification_labels(conn, since, until)?;
    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<ActivityCost> = stmt
        .query_map(param_refs.as_slice(), |row| {
            let activity: String = row.get(0)?;
            let (source, confidence) = activity_classification_labels(&activity, &label_lookup);
            Ok(ActivityCost {
                activity,
                session_count: row.get(1)?,
                message_count: row.get(2)?,
                input_tokens: row.get(3)?,
                output_tokens: row.get(4)?,
                cache_read_tokens: row.get(5)?,
                cache_creation_tokens: row.get(6)?,
                cost_cents: row.get(7)?,
                top_branch: row.get(8)?,
                top_repo_id: row.get(9)?,
                source: source.to_string(),
                confidence: confidence.to_string(),
            })
        })?
        .filter_map(|r| r.ok())
        .filter(|ac| !(ac.activity == UNTAGGED_DIMENSION && ac.message_count == 0))
        .collect();

    Ok(rows)
}

/// Detail view for a single activity: totals, dominant repo, and per-branch
/// breakdown. Returns `None` when no assistant messages carry the activity
/// in the requested window.
pub fn activity_cost_single(
    conn: &Connection,
    activity: &str,
    repo_id: Option<&str>,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Option<ActivityCostDetail>> {
    let mut conditions = vec![
        "m.role = 'assistant'".to_string(),
        "t.key = ?1".to_string(),
        "t.value = ?2".to_string(),
    ];
    let mut param_values: Vec<String> = vec![ACTIVITY_TAG_KEY.to_string(), activity.to_string()];
    let mut idx = 2usize;

    if let Some(repo) = repo_id {
        idx += 1;
        param_values.push(repo.to_string());
        conditions.push(format!("COALESCE(m.repo_id, '') = ?{idx}"));
    }
    if let Some(s) = since {
        idx += 1;
        param_values.push(s.to_string());
        conditions.push(format!("m.timestamp >= ?{idx}"));
    }
    if let Some(u) = until {
        idx += 1;
        param_values.push(u.to_string());
        conditions.push(format!("m.timestamp < ?{idx}"));
    }
    let where_clause = format!("WHERE {}", conditions.join(" AND "));

    let totals_sql = format!(
        "WITH msg_val_counts AS (
             SELECT message_id, COUNT(*) AS n_values
             FROM tags
             WHERE key = ?1
             GROUP BY message_id
         )
         SELECT COUNT(DISTINCT m.session_id) AS sess,
                COUNT(*) AS cnt,
                COALESCE(SUM(m.input_tokens / mvc.n_values), 0) AS inp,
                COALESCE(SUM(m.output_tokens / mvc.n_values), 0) AS outp,
                COALESCE(SUM(m.cache_read_tokens / mvc.n_values), 0) AS cache_r,
                COALESCE(SUM(m.cache_creation_tokens / mvc.n_values), 0) AS cache_c,
                COALESCE(SUM(m.cost_cents / mvc.n_values), 0.0) AS cost,
                CASE WHEN COUNT(DISTINCT COALESCE(m.repo_id, '')) = 1
                     THEN COALESCE(MIN(m.repo_id), '')
                     ELSE '' END AS repo
         FROM tags t
         JOIN msg_val_counts mvc ON mvc.message_id = t.message_id
         JOIN messages m ON m.id = t.message_id
         {where_clause}",
    );

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();
    let mut stmt = conn.prepare(&totals_sql)?;
    let totals = stmt.query_row(param_refs.as_slice(), |row| {
        Ok((
            row.get::<_, u64>(0)?,
            row.get::<_, u64>(1)?,
            row.get::<_, u64>(2)?,
            row.get::<_, u64>(3)?,
            row.get::<_, u64>(4)?,
            row.get::<_, u64>(5)?,
            row.get::<_, f64>(6)?,
            row.get::<_, String>(7)?,
        ))
    });
    let (sess, cnt, inp, outp, cache_r, cache_c, cost, repo) = match totals {
        Ok(row) => row,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if cnt == 0 {
        return Ok(None);
    }

    let branches_sql = format!(
        "WITH msg_val_counts AS (
             SELECT message_id, COUNT(*) AS n_values
             FROM tags
             WHERE key = ?1
             GROUP BY message_id
         )
         SELECT COALESCE(NULLIF(
                    CASE
                        WHEN COALESCE(m.git_branch, '') LIKE 'refs/heads/%'
                            THEN SUBSTR(COALESCE(m.git_branch, ''), 12)
                        ELSE COALESCE(m.git_branch, '')
                    END,
                    ''
                ), '{UNTAGGED_DIMENSION}') AS branch_value,
                COALESCE(m.repo_id, '') AS repo_value,
                COUNT(DISTINCT m.session_id) AS sess,
                COUNT(*) AS cnt,
                COALESCE(SUM(m.cost_cents / mvc.n_values), 0.0) AS cost
         FROM tags t
         JOIN msg_val_counts mvc ON mvc.message_id = t.message_id
         JOIN messages m ON m.id = t.message_id
         {where_clause}
         GROUP BY branch_value, repo_value
         ORDER BY cost DESC, branch_value ASC",
    );
    let mut stmt = conn.prepare(&branches_sql)?;
    let branches: Vec<ActivityBranchBreakdown> = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(ActivityBranchBreakdown {
                git_branch: row.get(0)?,
                repo_id: row.get(1)?,
                session_count: row.get(2)?,
                message_count: row.get(3)?,
                cost_cents: row.get(4)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    let label_lookup = load_activity_classification_labels(conn, since, until)?;
    let (source, confidence) = activity_classification_labels(activity, &label_lookup);
    Ok(Some(ActivityCostDetail {
        activity: activity.to_string(),
        session_count: sess,
        message_count: cnt,
        input_tokens: inp,
        output_tokens: outp,
        cache_read_tokens: cache_r,
        cache_creation_tokens: cache_c,
        cost_cents: cost,
        repo_id: repo,
        branches,
        source: source.to_string(),
        confidence: confidence.to_string(),
    }))
}

/// Build a `activity -> (source, confidence)` lookup for the current window
/// by reading the sibling `activity_source` and `activity_confidence` tags
/// emitted by the pipeline (R1.2, #222). When an aggregate has multiple
/// values for a label (e.g. a mix of `high` and `medium` confidence rows)
/// the dominant value wins, with ties broken by alphabetical order so the
/// result is deterministic across DBs.
///
/// Missing sibling tags fall back to the R1.0 defaults
/// (`source = "rule"`, `confidence = "medium"`) so legacy rows keep a
/// reasonable label without needing a reindex.
fn load_activity_classification_labels(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<std::collections::HashMap<String, (String, String)>> {
    use std::collections::HashMap;

    let mut conditions = vec!["m.role = 'assistant'".to_string()];
    let mut param_values: Vec<String> = Vec::new();
    if let Some(s) = since
        && is_valid_timestamp(s)
    {
        param_values.push(s.to_string());
        conditions.push(format!("m.timestamp >= ?{}", param_values.len()));
    }
    if let Some(u) = until
        && is_valid_timestamp(u)
    {
        param_values.push(u.to_string());
        conditions.push(format!("m.timestamp < ?{}", param_values.len()));
    }
    let where_clause = format!("WHERE {}", conditions.join(" AND "));

    let sql = format!(
        "WITH activity_per_msg AS (
             SELECT t.message_id, t.value AS activity
             FROM tags t
             JOIN messages m ON m.id = t.message_id
             {where_clause}
             AND t.key = '{ACTIVITY_TAG_KEY}'
         ),
         source_counts AS (
             SELECT ap.activity, COALESCE(ts.value, '{ACTIVITY_SOURCE_RULE}') AS source,
                    COUNT(*) AS c
             FROM activity_per_msg ap
             LEFT JOIN tags ts
               ON ts.message_id = ap.message_id AND ts.key = 'activity_source'
             GROUP BY ap.activity, source
         ),
         conf_counts AS (
             SELECT ap.activity, COALESCE(tc.value, '{ACTIVITY_CONFIDENCE_MEDIUM}') AS confidence,
                    COUNT(*) AS c
             FROM activity_per_msg ap
             LEFT JOIN tags tc
               ON tc.message_id = ap.message_id AND tc.key = 'activity_confidence'
             GROUP BY ap.activity, confidence
         ),
         dominant_source AS (
             SELECT activity, source
             FROM (
                 SELECT activity, source, c,
                        ROW_NUMBER() OVER (
                            PARTITION BY activity
                            ORDER BY c DESC, source ASC
                        ) AS rn
                 FROM source_counts
             )
             WHERE rn = 1
         ),
         dominant_conf AS (
             SELECT activity, confidence
             FROM (
                 SELECT activity, confidence, c,
                        ROW_NUMBER() OVER (
                            PARTITION BY activity
                            ORDER BY c DESC, confidence ASC
                        ) AS rn
                 FROM conf_counts
             )
             WHERE rn = 1
         )
         SELECT ds.activity, ds.source, dc.confidence
         FROM dominant_source ds
         JOIN dominant_conf dc ON dc.activity = ds.activity"
    );

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows: HashMap<String, (String, String)> = stmt
        .query_map(param_refs.as_slice(), |row| {
            let activity: String = row.get(0)?;
            let source: String = row.get(1)?;
            let confidence: String = row.get(2)?;
            Ok((activity, (source, confidence)))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

/// Pick the source/confidence labels for a given activity aggregate.
/// `(untagged)` reports empty strings so callers can render `--` without
/// special-casing it. Other activities fall back to the R1.0 defaults
/// (`rule` / `medium`) if no per-activity labels were loaded for this
/// window.
fn activity_classification_labels<'a>(
    activity: &str,
    lookup: &'a std::collections::HashMap<String, (String, String)>,
) -> (&'a str, &'a str) {
    if activity == UNTAGGED_DIMENSION {
        ("", "")
    } else if let Some((src, conf)) = lookup.get(activity) {
        (src.as_str(), conf.as_str())
    } else {
        (ACTIVITY_SOURCE_RULE, ACTIVITY_CONFIDENCE_MEDIUM)
    }
}

// ---------------------------------------------------------------------------
// Model Usage
// ---------------------------------------------------------------------------

/// Model usage breakdown: tokens grouped by model name.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ModelUsage {
    pub model: String,
    pub provider: String,
    pub message_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cost_cents: f64,
}

/// Query model usage stats, optionally filtered by date range.
pub fn model_usage(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    limit: usize,
) -> Result<Vec<ModelUsage>> {
    let filters = DimensionFilters::default();
    model_usage_with_filters(conn, since, until, &filters, limit)
}

fn model_usage_from_rollups(
    conn: &Connection,
    window: &RollupWindow,
    filters: &DimensionFilters,
    limit: usize,
) -> Result<Vec<ModelUsage>> {
    let mut conditions = vec!["role = 'assistant'".to_string()];
    let mut params: Vec<String> = Vec::new();
    append_rollup_time_filters(&mut conditions, &mut params, window);
    apply_dimension_filters(
        &mut conditions,
        &mut params,
        filters,
        "provider",
        "model",
        "repo_id",
        "git_branch",
        "surface",
    );
    params.push(limit.to_string());
    let limit_idx = params.len();
    let sql = format!(
        "SELECT model as m,
                provider as p,
                COALESCE(SUM(message_count), 0) as cnt,
                COALESCE(SUM(input_tokens), 0) as total_input,
                COALESCE(SUM(output_tokens), 0) as total_output,
                COALESCE(SUM(cache_read_tokens), 0),
                COALESCE(SUM(cache_creation_tokens), 0),
                COALESCE(SUM(cost_cents), 0.0)
         FROM {}
         WHERE {}
         GROUP BY m, p
         ORDER BY 8 DESC
         LIMIT ?{limit_idx}",
        rollup_table(window.level),
        conditions.join(" AND ")
    );
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<ModelUsage> = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(ModelUsage {
                model: row.get(0)?,
                provider: row.get(1)?,
                message_count: row.get(2)?,
                input_tokens: row.get(3)?,
                output_tokens: row.get(4)?,
                cache_read_tokens: row.get(5)?,
                cache_creation_tokens: row.get(6)?,
                cost_cents: row.get(7)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

pub fn model_usage_with_filters(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    filters: &DimensionFilters,
    limit: usize,
) -> Result<Vec<ModelUsage>> {
    if rollups_available(conn)
        && let Some(window) = choose_rollup_window(since, until, true)
    {
        return model_usage_from_rollups(conn, &window, filters, limit);
    }

    let mut conditions = vec!["role = 'assistant'".to_string()];
    let mut param_values: Vec<String> = Vec::new();
    if let Some(s) = since
        && is_valid_timestamp(s)
    {
        param_values.push(s.to_string());
        conditions.push(format!("timestamp >= ?{}", param_values.len()));
    }
    if let Some(u) = until
        && is_valid_timestamp(u)
    {
        param_values.push(u.to_string());
        conditions.push(format!("timestamp < ?{}", param_values.len()));
    }
    let model_expr = normalized_model_expr("model");
    let project_expr = normalized_project_expr("repo_id");
    let branch_expr = normalized_branch_expr("git_branch");
    let surface_expr = normalized_surface_expr("surface");
    apply_dimension_filters(
        &mut conditions,
        &mut param_values,
        filters,
        "COALESCE(provider, 'claude_code')",
        &model_expr,
        &project_expr,
        &branch_expr,
        &surface_expr,
    );
    let where_clause = format!("WHERE {}", conditions.join(" AND "));
    param_values.push(limit.to_string());
    let limit_idx = param_values.len();
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    // Single-query approach: COALESCE NULL/empty/template models into "(untagged)"
    let sql = format!(
        "SELECT CASE WHEN model IS NULL OR model = '' OR SUBSTR(model, 1, 1) = '<' THEN '(untagged)'
                     ELSE model END as m,
                COALESCE(provider, '') as p,
                COUNT(*) as cnt,
                COALESCE(SUM(input_tokens), 0) as total_input,
                COALESCE(SUM(output_tokens), 0) as total_output,
                COALESCE(SUM(cache_read_tokens), 0),
                COALESCE(SUM(cache_creation_tokens), 0),
                COALESCE(SUM(cost_cents), 0.0)
         FROM messages
         {where_clause}
         GROUP BY m, p
         ORDER BY 8 DESC
         LIMIT ?{limit_idx}",
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<ModelUsage> = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(ModelUsage {
                model: row.get(0)?,
                provider: row.get(1)?,
                message_count: row.get(2)?,
                input_tokens: row.get(3)?,
                output_tokens: row.get(4)?,
                cache_read_tokens: row.get(5)?,
                cache_creation_tokens: row.get(6)?,
                cost_cents: row.get(7)?,
            })
        })?
        .filter_map(|r| match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("skipping row: {e}");
                None
            }
        })
        .collect();

    Ok(rows)
}

// ---------------------------------------------------------------------------
// Statusline — shared provider-scoped status contract (ADR-0088 §4, #224).
// ---------------------------------------------------------------------------
//
// The JSON shape emitted by `/analytics/statusline` and `budi statusline
// --format json` is the single shared provider-scoped status contract. It is
// consumed by the CLI statusline, the Cursor extension (#232), and the cloud
// dashboard (#235). Provider is an explicit filter rather than a family of
// per-surface shapes. See `docs/statusline-contract.md`.

/// Compact stats for the status line display.
///
/// Primary windows are rolling `1d` / `7d` / `30d`, surfaced as
/// `cost_1d` / `cost_7d` / `cost_30d`. The legacy `today_cost` /
/// `week_cost` / `month_cost` fields are populated with the same rolling
/// values for one-release backward compatibility with downstream consumers
/// written against 8.0; they are deprecated and will be removed in 9.0.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StatuslineStats {
    /// Rolling 24h cost in dollars, optionally provider-scoped.
    pub cost_1d: f64,
    /// Rolling 7-day cost in dollars, optionally provider-scoped.
    pub cost_7d: f64,
    /// Rolling 30-day cost in dollars, optionally provider-scoped.
    pub cost_30d: f64,
    /// Provider this response was scoped to, or `None` for unscoped totals.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_scope: Option<String>,
    /// Deprecated alias for `cost_1d`. Removed in 9.0.
    pub today_cost: f64,
    /// Deprecated alias for `cost_7d`. Removed in 9.0.
    pub week_cost: f64,
    /// Deprecated alias for `cost_30d`. Removed in 9.0.
    pub month_cost: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_provider: Option<String>,
    /// Providers contributing to the aggregated totals when more than one
    /// provider was passed in the filter (host-scoped surface, ADR-0088 §7
    /// post-#648). Empty for unscoped requests and for single-provider
    /// requests, so the byte shape of the existing single-provider response
    /// is preserved.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contributing_providers: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health_tip: Option<String>,
    /// Per-user-prompt cost in dollars for the active session (for statusline
    /// rate display). #692: this is in dollars to match every other `*_cost`
    /// field in the response — pre-#692 it was in cents and the CLI divided.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_msg_cost: Option<f64>,
    /// Disclaimer for Cursor sessions that ended recently, as their cost data
    /// may lag up to ~10 minutes per the Usage API.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_lag_hint: Option<String>,
}

/// Parameters for requesting extra statusline data.
#[derive(Debug, Default, Clone, serde::Deserialize)]
pub struct StatuslineParams {
    pub session_id: Option<String>,
    pub branch: Option<String>,
    pub project_dir: Option<String>,
    /// Optional repo identity (as produced by `budi_core::repo_id`). When
    /// set together with `branch`, `branch_cost` is scoped to
    /// `(repo_id, branch)` so developers who sit on `main` / `master` in
    /// several repos see only the current repo's activity instead of a
    /// cross-repo sum. Left as `None` preserves the pre-#347 behavior for
    /// consumers that can't resolve a repo identity (no git, shell not in a
    /// repo, etc.). See issue #347.
    pub repo_id: Option<String>,
    /// Optional provider filter. Accepts a comma-separated list — e.g.
    /// `?provider=cursor` (provider-scoped) or `?provider=cursor,copilot_chat`
    /// (host-scoped, aggregates the listed providers). Single-value form is
    /// preserved for backward compatibility with budi-cursor 1.3.x and the
    /// 8.1+ provider-scoped statusline contract. When the filter is empty
    /// every numeric field is unscoped (all enabled providers).
    ///
    /// Repeated forms (`?provider=a&provider=b`) are not supported by
    /// axum's default `serde_urlencoded`-backed `Query` extractor — only the
    /// last value would survive. Callers that need multi-provider must use
    /// the comma-list form. See ADR-0088 §7 (post-#648).
    #[serde(default, deserialize_with = "deserialize_provider_filter")]
    pub provider: Vec<String>,
}

fn deserialize_provider_filter<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    let raw: Option<String> = Option::deserialize(deserializer)?;
    Ok(parse_provider_filter(raw.as_deref()))
}

/// Parse a comma-separated provider filter string into a normalized
/// `Vec<String>`. Empty / whitespace-only entries are dropped, duplicates
/// are removed in input order, and `None` collapses to an empty vec.
pub(crate) fn parse_provider_filter(raw: Option<&str>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    raw.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|s| {
            if seen.insert(s.to_string()) {
                Some(s.to_string())
            } else {
                None
            }
        })
        .collect()
}

fn assistant_cost_since_from_rollups(
    conn: &Connection,
    since: &str,
    providers: &[String],
) -> Option<f64> {
    if !rollups_available(conn) {
        return None;
    }
    let window = choose_rollup_window(Some(since), None, false)?;
    let mut conditions = vec!["role = 'assistant'".to_string()];
    let mut params: Vec<String> = Vec::new();
    append_rollup_time_filters(&mut conditions, &mut params, &window);
    if !providers.is_empty() {
        let placeholders = vec!["?"; providers.len()].join(", ");
        conditions.push(format!("provider IN ({placeholders})"));
        params.extend(providers.iter().cloned());
    }
    let sql = format!(
        "SELECT COALESCE(SUM(cost_cents), 0.0)
         FROM {}
         WHERE {}",
        rollup_table(window.level),
        conditions.join(" AND ")
    );
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();
    conn.query_row(&sql, param_refs.as_slice(), |r| r.get::<_, f64>(0))
        .ok()
}

/// Compute cost stats for rolling 1d / 7d / 30d, suitable for the CLI status
/// line and for the shared provider-scoped status contract consumed by the
/// Cursor extension and cloud dashboard (ADR-0088 §4, #224). Optionally
/// computes session / branch / project costs when params are provided, and
/// scopes every numeric field to `params.provider` when set.
pub fn statusline_stats(
    conn: &Connection,
    since_1d: &str,
    since_7d: &str,
    since_30d: &str,
    params: &StatuslineParams,
) -> Result<StatuslineStats> {
    let provider_filter: &[String] = &params.provider;

    // Helper: append `provider IN (?, ?, ...)` to `sql` and the matching
    // bindings, using whatever placeholder syntax the caller is already using.
    // Skipped when the filter is empty (unscoped — sums across every provider).
    let push_provider_in = |sql: &mut String, bindings: &mut Vec<String>| {
        if provider_filter.is_empty() {
            return;
        }
        let placeholders = vec!["?"; provider_filter.len()].join(", ");
        sql.push_str(&format!(" AND provider IN ({placeholders})"));
        bindings.extend(provider_filter.iter().cloned());
    };

    let cost_since = |since: &str| -> f64 {
        assistant_cost_since_from_rollups(conn, since, provider_filter).unwrap_or_else(|| {
            let mut sql = String::from(
                "SELECT COALESCE(SUM(cost_cents), 0.0) FROM messages \
                     WHERE timestamp >= ? AND role = 'assistant'",
            );
            let mut bindings: Vec<String> = vec![since.to_string()];
            push_provider_in(&mut sql, &mut bindings);
            let refs: Vec<&dyn rusqlite::types::ToSql> = bindings
                .iter()
                .map(|s| s as &dyn rusqlite::types::ToSql)
                .collect();
            conn.query_row(&sql, refs.as_slice(), |r| r.get::<_, f64>(0))
                .unwrap_or(0.0)
        }) / 100.0
    };

    let cost_1d = cost_since(since_1d);
    let cost_7d = cost_since(since_7d);
    let cost_30d = cost_since(since_30d);
    let normalized_session_id = params
        .session_id
        .as_deref()
        .map(crate::identity::normalize_session_id);

    // Session cost: total cost for a specific session (optionally provider-scoped).
    let session_cost = normalized_session_id.as_ref().map(|sid| {
        let mut sql = String::from(
            "SELECT COALESCE(SUM(cost_cents), 0.0) FROM messages \
             WHERE session_id = ? AND role = 'assistant'",
        );
        let mut bindings: Vec<String> = vec![sid.clone()];
        push_provider_in(&mut sql, &mut bindings);
        let refs: Vec<&dyn rusqlite::types::ToSql> = bindings
            .iter()
            .map(|s| s as &dyn rusqlite::types::ToSql)
            .collect();
        conn.query_row(&sql, refs.as_slice(), |r| r.get::<_, f64>(0))
            .unwrap_or(0.0)
            / 100.0
    });

    // Branch cost: total cost for messages on a specific branch.
    //
    // When `repo_id` is also provided, filter on `(repo_id, branch)` so
    // developers who keep several local repos checked out on `main`
    // (or `master` / `develop`) see only the current repo's branch spend
    // instead of a silent cross-repo sum. See #347.
    let branch_cost = params.branch.as_ref().map(|branch| {
        let mut sql = String::from(
            "SELECT COALESCE(SUM(cost_cents), 0.0) FROM messages \
             WHERE git_branch = ? AND role = 'assistant'",
        );
        let mut bindings: Vec<String> = vec![branch.clone()];
        if let Some(repo) = params.repo_id.as_deref() {
            sql.push_str(" AND COALESCE(repo_id, '') = ?");
            bindings.push(repo.to_string());
        }
        push_provider_in(&mut sql, &mut bindings);
        let refs: Vec<&dyn rusqlite::types::ToSql> = bindings
            .iter()
            .map(|s| s as &dyn rusqlite::types::ToSql)
            .collect();
        conn.query_row(&sql, refs.as_slice(), |r| r.get::<_, f64>(0))
            .unwrap_or(0.0)
            / 100.0
    });

    // Project cost: total cost for messages in a specific directory.
    let project_cost = params.project_dir.as_ref().map(|dir| {
        let mut sql = String::from(
            "SELECT COALESCE(SUM(cost_cents), 0.0) FROM messages \
             WHERE cwd = ? AND role = 'assistant'",
        );
        let mut bindings: Vec<String> = vec![dir.clone()];
        push_provider_in(&mut sql, &mut bindings);
        let refs: Vec<&dyn rusqlite::types::ToSql> = bindings
            .iter()
            .map(|s| s as &dyn rusqlite::types::ToSql)
            .collect();
        conn.query_row(&sql, refs.as_slice(), |r| r.get::<_, f64>(0))
            .unwrap_or(0.0)
            / 100.0
    });

    // Active provider: most recent provider seen in the 1d window, after the
    // provider filter is applied. Under multi-provider this is the provider
    // with the most recent traffic — host-scoped click-through routes to its
    // dashboard (ADR-0088 §7 post-#648).
    let active_provider: Option<String> = {
        let mut sql = String::from("SELECT provider FROM messages WHERE timestamp >= ?");
        let mut bindings: Vec<String> = vec![since_1d.to_string()];
        push_provider_in(&mut sql, &mut bindings);
        sql.push_str(" ORDER BY timestamp DESC LIMIT 1");
        let refs: Vec<&dyn rusqlite::types::ToSql> = bindings
            .iter()
            .map(|s| s as &dyn rusqlite::types::ToSql)
            .collect();
        conn.query_row(&sql, refs.as_slice(), |r| r.get(0)).ok()
    };

    let (health_state, health_tip, session_msg_cost) = normalized_session_id
        .as_ref()
        .and_then(|sid| super::health::session_health(conn, Some(sid)).ok())
        .map(|h| {
            // #691: average is session_cost / user-typed prompts. Subagent
            // fan-outs only emit assistant rows so a multi-call turn stays at
            // 1, and zero-cost unpriced rows contribute 0 to the numerator
            // without inflating the denominator. `user_prompt_count` carries
            // the copilot_chat fallback for sessions with no captured user
            // rows (see `compute_user_prompt_count`).
            //
            // #692: convert to dollars on the daemon side so every `*_cost`
            // field in the statusline response is in the same unit. CLI no
            // longer divides by 100.
            let avg = if h.user_prompt_count > 0 {
                Some((h.total_cost_cents / h.user_prompt_count as f64) / 100.0)
            } else {
                None
            };
            (Some(h.state), Some(h.tip), avg)
        })
        .unwrap_or((None, None, None));

    // Lag hint fires whenever `cursor` is part of the aggregated totals,
    // not just when it's the active provider — a host-scoped roll-up that
    // *includes* Cursor still shows lagging numbers even if Copilot Chat
    // happened to be the most recent traffic in the 1d window.
    let cursor_in_filter = provider_filter.iter().any(|p| p == "cursor");
    let cursor_active = active_provider.as_deref() == Some("cursor");
    let cost_lag_hint = if cursor_in_filter || cursor_active {
        Some(crate::analytics::CURSOR_LAG_HINT.to_string())
    } else {
        None
    };

    // `provider_scope` keeps its single-provider semantics: echoed back when
    // exactly one provider was filtered, omitted otherwise. Multi-provider
    // requests advertise their scope via `contributing_providers`. Single-
    // provider responses stay byte-identical to the 8.1 contract.
    let provider_scope = if provider_filter.len() == 1 {
        Some(provider_filter[0].clone())
    } else {
        None
    };
    let contributing_providers = if provider_filter.len() > 1 {
        provider_filter.to_vec()
    } else {
        Vec::new()
    };

    Ok(StatuslineStats {
        cost_1d,
        cost_7d,
        cost_30d,
        provider_scope,
        today_cost: cost_1d,
        week_cost: cost_7d,
        month_cost: cost_30d,
        session_cost,
        branch_cost,
        project_cost,
        active_provider,
        contributing_providers,
        health_state,
        health_tip,
        session_msg_cost,
        cost_lag_hint,
    })
}

// ---------------------------------------------------------------------------
// Provider Stats
// ---------------------------------------------------------------------------

/// Per-provider aggregate stats for the /analytics/providers endpoint.
///
/// ## Message counts (8.3.1 / #482)
///
/// Token and cost fields are assistant-only (a user turn has no LLM spend).
/// The three message-count fields disambiguate what a row counts:
///
/// - `assistant_messages` — assistant replies. Same unit every other breakdown
///   uses (`SessionStats.message_count`, `RepoUsage.message_count`, etc.).
/// - `user_messages` — user prompts.
/// - `total_messages` — user + assistant. Matches `UsageSummary.total_messages`
///   so the Agents block sums back to the grand Total row in `budi stats`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProviderStats {
    pub provider: String,
    pub display_name: String,
    /// Assistant-side message count. Pre-8.3.1 this was exposed as
    /// `message_count`; the alias keeps older deserializers working.
    #[serde(alias = "message_count")]
    pub assistant_messages: u64,
    /// User-side message count (8.3.1+, #482).
    pub user_messages: u64,
    /// User + assistant. Reconciles to `UsageSummary.total_messages`.
    pub total_messages: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub estimated_cost: f64,
    pub total_cost_cents: f64,
}

/// Query per-provider aggregate stats.
pub fn provider_stats(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Vec<ProviderStats>> {
    let filters = DimensionFilters::default();
    provider_stats_with_filters(conn, since, until, &filters)
}

fn provider_stats_from_rollups(
    conn: &Connection,
    window: &RollupWindow,
    filters: &DimensionFilters,
) -> Result<Vec<ProviderStats>> {
    // #482: count user + assistant rows and split via CASE so the Agents
    // block sums back to `UsageSummary.total_messages`. Tokens and cost
    // stay assistant-only because a user turn has no LLM spend.
    let mut conditions: Vec<String> = Vec::new();
    let mut params: Vec<String> = Vec::new();
    append_rollup_time_filters(&mut conditions, &mut params, window);
    apply_dimension_filters(
        &mut conditions,
        &mut params,
        filters,
        "provider",
        "model",
        "repo_id",
        "git_branch",
        "surface",
    );
    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };
    let sql = format!(
        "SELECT provider as p,
                COALESCE(SUM(message_count), 0) as total_msgs,
                COALESCE(SUM(CASE WHEN role = 'user' THEN message_count ELSE 0 END), 0) as user_msgs,
                COALESCE(SUM(CASE WHEN role = 'assistant' THEN message_count ELSE 0 END), 0) as asst_msgs,
                COALESCE(SUM(CASE WHEN role = 'assistant' THEN input_tokens ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN role = 'assistant' THEN output_tokens ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN role = 'assistant' THEN cache_creation_tokens ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN role = 'assistant' THEN cache_read_tokens ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN role = 'assistant' THEN cost_cents ELSE 0.0 END), 0.0)
         FROM {}
         {}
         GROUP BY p
         ORDER BY asst_msgs DESC",
        rollup_table(window.level),
        where_clause
    );
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, u64>(2)?,
                row.get::<_, u64>(3)?,
                row.get::<_, u64>(4)?,
                row.get::<_, u64>(5)?,
                row.get::<_, u64>(6)?,
                row.get::<_, u64>(7)?,
                row.get::<_, f64>(8)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect::<Vec<_>>();

    let providers = crate::provider::all_providers();
    let mut result = Vec::new();
    for (
        prov,
        total_msgs,
        user_msgs,
        asst_msgs,
        input,
        output,
        cache_create,
        cache_read,
        sum_cost_cents,
    ) in rows
    {
        let display_name = providers
            .iter()
            .find(|p| p.name() == prov)
            .map(|p| p.display_name().to_string())
            .unwrap_or_else(|| prov.clone());
        let estimated_cost = sum_cost_cents.round() / 100.0;
        result.push(ProviderStats {
            provider: prov,
            display_name,
            assistant_messages: asst_msgs,
            user_messages: user_msgs,
            total_messages: total_msgs,
            input_tokens: input,
            output_tokens: output,
            cache_creation_tokens: cache_create,
            cache_read_tokens: cache_read,
            estimated_cost,
            total_cost_cents: sum_cost_cents,
        });
    }
    Ok(result)
}

pub fn provider_stats_with_filters(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    filters: &DimensionFilters,
) -> Result<Vec<ProviderStats>> {
    if rollups_available(conn)
        && let Some(window) = choose_rollup_window(since, until, true)
    {
        return provider_stats_from_rollups(conn, &window, filters);
    }

    // #482: count user + assistant rows and split via CASE so the Agents
    // block sums back to `UsageSummary.total_messages`. Tokens and cost
    // stay assistant-only because a user turn has no LLM spend.
    let mut conditions: Vec<String> = Vec::new();
    let mut param_values = Vec::new();
    if let Some(s) = since
        && is_valid_timestamp(s)
    {
        param_values.push(s.to_string());
        conditions.push(format!("timestamp >= ?{}", param_values.len()));
    }
    if let Some(u) = until
        && is_valid_timestamp(u)
    {
        param_values.push(u.to_string());
        conditions.push(format!("timestamp < ?{}", param_values.len()));
    }
    let model_expr = normalized_model_expr("model");
    let project_expr = normalized_project_expr("repo_id");
    let branch_expr = normalized_branch_expr("git_branch");
    let surface_expr = normalized_surface_expr("surface");
    apply_dimension_filters(
        &mut conditions,
        &mut param_values,
        filters,
        "COALESCE(provider, 'claude_code')",
        &model_expr,
        &project_expr,
        &branch_expr,
        &surface_expr,
    );
    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();
    let sql = format!(
        "SELECT provider as p,
                COUNT(*) as total_msgs,
                COALESCE(SUM(CASE WHEN role = 'user' THEN 1 ELSE 0 END), 0) as user_msgs,
                COALESCE(SUM(CASE WHEN role = 'assistant' THEN 1 ELSE 0 END), 0) as asst_msgs,
                COALESCE(SUM(CASE WHEN role = 'assistant' THEN input_tokens ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN role = 'assistant' THEN output_tokens ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN role = 'assistant' THEN cache_creation_tokens ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN role = 'assistant' THEN cache_read_tokens ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN role = 'assistant' THEN cost_cents ELSE 0.0 END), 0.0)
         FROM messages {}
         GROUP BY p ORDER BY asst_msgs DESC",
        where_clause
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, u64>(2)?,
                row.get::<_, u64>(3)?,
                row.get::<_, u64>(4)?,
                row.get::<_, u64>(5)?,
                row.get::<_, u64>(6)?,
                row.get::<_, u64>(7)?,
                row.get::<_, f64>(8)?,
            ))
        })?
        .filter_map(|r| match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("skipping row: {e}");
                None
            }
        })
        .collect::<Vec<_>>();

    let providers = crate::provider::all_providers();
    let mut result = Vec::new();

    for (
        prov,
        total_msgs,
        user_msgs,
        asst_msgs,
        input,
        output,
        cache_create,
        cache_read,
        sum_cost_cents,
    ) in rows
    {
        let display_name = providers
            .iter()
            .find(|p| p.name() == prov)
            .map(|p| p.display_name().to_string())
            .unwrap_or_else(|| prov.clone());

        // sum_cost_cents is in cents; estimated_cost is in dollars (rounded to nearest cent).
        let estimated_cost = sum_cost_cents.round() / 100.0;

        result.push(ProviderStats {
            provider: prov,
            display_name,
            assistant_messages: asst_msgs,
            user_messages: user_msgs,
            total_messages: total_msgs,
            input_tokens: input,
            output_tokens: output,
            cache_creation_tokens: cache_create,
            cache_read_tokens: cache_read,
            estimated_cost,
            total_cost_cents: sum_cost_cents,
        });
    }

    Ok(result)
}

// ---------------------------------------------------------------------------
// Surface Stats (#702)
// ---------------------------------------------------------------------------

/// Per-surface aggregate stats. Mirror of [`ProviderStats`] keyed on the
/// `surface` axis (`vscode` / `cursor` / `jetbrains` / `terminal` /
/// `unknown`) introduced in #701. `surface` answers *which host* an AI
/// conversation happened in; `provider` answers *which agent*. Surfaced as
/// its own breakdown so a multi-IDE user can answer "how much am I
/// spending in JetBrains vs VS Code today?" without surface-aware scripts.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SurfaceStats {
    pub surface: String,
    /// Assistant-side message count.
    pub assistant_messages: u64,
    /// User-side message count.
    pub user_messages: u64,
    /// User + assistant. Reconciles to `UsageSummary.total_messages` when
    /// summed across surfaces.
    pub total_messages: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub estimated_cost: f64,
    pub total_cost_cents: f64,
}

/// Query per-surface aggregate stats. Empty surfaces (no rows in the
/// window) are excluded so a fresh user with only `terminal` rows does
/// not see four empty rows.
pub fn surface_stats(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Vec<SurfaceStats>> {
    let filters = DimensionFilters::default();
    surface_stats_with_filters(conn, since, until, &filters)
}

fn surface_stats_from_rollups(
    conn: &Connection,
    window: &RollupWindow,
    filters: &DimensionFilters,
) -> Result<Vec<SurfaceStats>> {
    let mut conditions: Vec<String> = Vec::new();
    let mut params: Vec<String> = Vec::new();
    append_rollup_time_filters(&mut conditions, &mut params, window);
    apply_dimension_filters(
        &mut conditions,
        &mut params,
        filters,
        "provider",
        "model",
        "repo_id",
        "git_branch",
        "surface",
    );
    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };
    let sql = format!(
        "SELECT COALESCE(NULLIF(LOWER(surface), ''), 'unknown') as s,
                COALESCE(SUM(message_count), 0) as total_msgs,
                COALESCE(SUM(CASE WHEN role = 'user' THEN message_count ELSE 0 END), 0) as user_msgs,
                COALESCE(SUM(CASE WHEN role = 'assistant' THEN message_count ELSE 0 END), 0) as asst_msgs,
                COALESCE(SUM(CASE WHEN role = 'assistant' THEN input_tokens ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN role = 'assistant' THEN output_tokens ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN role = 'assistant' THEN cache_creation_tokens ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN role = 'assistant' THEN cache_read_tokens ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN role = 'assistant' THEN cost_cents ELSE 0.0 END), 0.0)
         FROM {}
         {}
         GROUP BY s
         ORDER BY asst_msgs DESC, s ASC",
        rollup_table(window.level),
        where_clause
    );
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<SurfaceStats> = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(SurfaceStats {
                surface: row.get(0)?,
                total_messages: row.get(1)?,
                user_messages: row.get(2)?,
                assistant_messages: row.get(3)?,
                input_tokens: row.get(4)?,
                output_tokens: row.get(5)?,
                cache_creation_tokens: row.get(6)?,
                cache_read_tokens: row.get(7)?,
                total_cost_cents: row.get(8)?,
                estimated_cost: 0.0,
            })
        })?
        .filter_map(|r| r.ok())
        .map(|mut s| {
            s.estimated_cost = s.total_cost_cents.round() / 100.0;
            s
        })
        .collect();
    Ok(rows)
}

pub fn surface_stats_with_filters(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    filters: &DimensionFilters,
) -> Result<Vec<SurfaceStats>> {
    if rollups_available(conn)
        && let Some(window) = choose_rollup_window(since, until, true)
    {
        return surface_stats_from_rollups(conn, &window, filters);
    }

    let mut conditions: Vec<String> = Vec::new();
    let mut param_values: Vec<String> = Vec::new();
    if let Some(s) = since
        && is_valid_timestamp(s)
    {
        param_values.push(s.to_string());
        conditions.push(format!("timestamp >= ?{}", param_values.len()));
    }
    if let Some(u) = until
        && is_valid_timestamp(u)
    {
        param_values.push(u.to_string());
        conditions.push(format!("timestamp < ?{}", param_values.len()));
    }
    let model_expr = normalized_model_expr("model");
    let project_expr = normalized_project_expr("repo_id");
    let branch_expr = normalized_branch_expr("git_branch");
    let surface_expr = normalized_surface_expr("surface");
    apply_dimension_filters(
        &mut conditions,
        &mut param_values,
        filters,
        "COALESCE(provider, 'claude_code')",
        &model_expr,
        &project_expr,
        &branch_expr,
        &surface_expr,
    );
    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();
    let sql = format!(
        "SELECT COALESCE(NULLIF(LOWER(surface), ''), 'unknown') as s,
                COUNT(*) as total_msgs,
                COALESCE(SUM(CASE WHEN role = 'user' THEN 1 ELSE 0 END), 0) as user_msgs,
                COALESCE(SUM(CASE WHEN role = 'assistant' THEN 1 ELSE 0 END), 0) as asst_msgs,
                COALESCE(SUM(CASE WHEN role = 'assistant' THEN input_tokens ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN role = 'assistant' THEN output_tokens ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN role = 'assistant' THEN cache_creation_tokens ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN role = 'assistant' THEN cache_read_tokens ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN role = 'assistant' THEN cost_cents ELSE 0.0 END), 0.0)
         FROM messages {}
         GROUP BY s
         ORDER BY asst_msgs DESC, s ASC",
        where_clause
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<SurfaceStats> = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(SurfaceStats {
                surface: row.get(0)?,
                total_messages: row.get(1)?,
                user_messages: row.get(2)?,
                assistant_messages: row.get(3)?,
                input_tokens: row.get(4)?,
                output_tokens: row.get(5)?,
                cache_creation_tokens: row.get(6)?,
                cache_read_tokens: row.get(7)?,
                total_cost_cents: row.get(8)?,
                estimated_cost: 0.0,
            })
        })?
        .filter_map(|r| match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("skipping row: {e}");
                None
            }
        })
        .map(|mut s| {
            s.estimated_cost = s.total_cost_cents.round() / 100.0;
            s
        })
        .collect();
    Ok(rows)
}

// ---------------------------------------------------------------------------
// Status Snapshot (#619)
// ---------------------------------------------------------------------------

/// Single-connection snapshot of summary + cost + providers for the
/// `budi status` command.  Querying all three from one connection
/// eliminates the within-command race where the tailer commits between
/// the individual HTTP calls that `status` used to make.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StatusSnapshot {
    pub summary: UsageSummary,
    pub cost: crate::cost::CostEstimate,
    pub providers: Vec<ProviderStats>,
}

/// Query summary, cost, and providers from a single connection so
/// the `budi status` display is internally consistent.
pub fn status_snapshot(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    provider: Option<&str>,
) -> Result<StatusSnapshot> {
    let filters = DimensionFilters::default();
    let summary = usage_summary_with_filters(conn, since, until, provider, &filters)?;
    let cost = crate::cost::estimate_cost_with_filters(conn, since, until, provider, &filters)?;
    let providers = provider_stats_with_filters(conn, since, until, &filters)?;
    Ok(StatusSnapshot {
        summary,
        cost,
        providers,
    })
}

// ---------------------------------------------------------------------------
// Cache Efficiency
// ---------------------------------------------------------------------------

/// Cache efficiency stats for a date range.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CacheEfficiency {
    pub total_input_tokens: u64,
    pub total_cache_read_tokens: u64,
    pub total_cache_creation_tokens: u64,
    pub cache_hit_rate: f64,
    pub cache_savings_cents: f64,
}

/// Query cache efficiency stats, optionally filtered by date range.
pub fn cache_efficiency(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<CacheEfficiency> {
    let filters = DimensionFilters::default();
    cache_efficiency_with_filters(conn, since, until, &filters)
}

pub fn cache_efficiency_with_filters(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    filters: &DimensionFilters,
) -> Result<CacheEfficiency> {
    let mut conditions = vec!["role = 'assistant'".to_string()];
    let mut param_values = Vec::new();
    if let Some(s) = since
        && is_valid_timestamp(s)
    {
        param_values.push(s.to_string());
        conditions.push(format!("timestamp >= ?{}", param_values.len()));
    }
    if let Some(u) = until
        && is_valid_timestamp(u)
    {
        param_values.push(u.to_string());
        conditions.push(format!("timestamp < ?{}", param_values.len()));
    }
    let model_expr = normalized_model_expr("model");
    let project_expr = normalized_project_expr("repo_id");
    let branch_expr = normalized_branch_expr("git_branch");
    let surface_expr = normalized_surface_expr("surface");
    apply_dimension_filters(
        &mut conditions,
        &mut param_values,
        filters,
        "COALESCE(provider, 'claude_code')",
        &model_expr,
        &project_expr,
        &branch_expr,
        &surface_expr,
    );
    let where_clause = format!("WHERE {}", conditions.join(" AND "));
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    let sql = format!(
        "SELECT COALESCE(SUM(input_tokens), 0),
                COALESCE(SUM(cache_read_tokens), 0),
                COALESCE(SUM(cache_creation_tokens), 0),
                provider,
                COALESCE(model, 'unknown')
         FROM messages {where_clause}
         GROUP BY provider, COALESCE(model, 'unknown')",
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok((
                row.get::<_, u64>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, u64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect::<Vec<_>>();

    let mut total_input: u64 = 0;
    let mut total_cache_read: u64 = 0;
    let mut total_cache_creation: u64 = 0;
    let mut total_savings_cents: f64 = 0.0;

    for (input, cache_read, cache_creation, prov, model) in &rows {
        total_input += input;
        total_cache_read += cache_read;
        total_cache_creation += cache_creation;
        // ADR-0091: pricing flows through `pricing::lookup`. Unknown models
        // contribute 0 savings rather than borrowing a phantom default rate.
        let pricing = match crate::pricing::lookup(model, prov) {
            crate::pricing::PricingOutcome::Known { pricing, .. } => pricing,
            crate::pricing::PricingOutcome::Unknown { .. } => continue,
        };
        // Savings: what cache reads would have cost at full input price minus what they actually cost
        let savings = *cache_read as f64 * (pricing.input - pricing.cache_read) / 1_000_000.0;
        total_savings_cents += savings * 100.0;
    }

    let denominator = total_input + total_cache_read;
    let cache_hit_rate = if denominator > 0 {
        total_cache_read as f64 / denominator as f64
    } else {
        0.0
    };

    Ok(CacheEfficiency {
        total_input_tokens: total_input + total_cache_read,
        total_cache_read_tokens: total_cache_read,
        total_cache_creation_tokens: total_cache_creation,
        cache_hit_rate,
        cache_savings_cents: (total_savings_cents * 100.0).round() / 100.0,
    })
}

// ---------------------------------------------------------------------------
// Session Cost Curve
// ---------------------------------------------------------------------------

/// Session cost curve: average cost per message by session length bucket.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionCostBucket {
    pub bucket: String,
    pub session_count: u64,
    pub avg_messages: f64,
    pub avg_cost_per_message_cents: f64,
    pub total_cost_cents: f64,
}

/// Query session cost curve: average cost per message grouped by session length.
pub fn session_cost_curve(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Vec<SessionCostBucket>> {
    let filters = DimensionFilters::default();
    session_cost_curve_with_filters(conn, since, until, &filters)
}

pub fn session_cost_curve_with_filters(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    filters: &DimensionFilters,
) -> Result<Vec<SessionCostBucket>> {
    let mut conditions = vec!["role = 'assistant'".to_string()];
    let mut param_values: Vec<String> = Vec::new();
    if let Some(s) = since {
        param_values.push(s.to_string());
        conditions.push(format!("timestamp >= ?{}", param_values.len()));
    }
    if let Some(u) = until {
        param_values.push(u.to_string());
        conditions.push(format!("timestamp < ?{}", param_values.len()));
    }
    let model_expr = normalized_model_expr("model");
    let project_expr = normalized_project_expr("repo_id");
    let branch_expr = normalized_branch_expr("git_branch");
    let surface_expr = normalized_surface_expr("surface");
    apply_dimension_filters(
        &mut conditions,
        &mut param_values,
        filters,
        "COALESCE(provider, 'claude_code')",
        &model_expr,
        &project_expr,
        &branch_expr,
        &surface_expr,
    );
    let where_clause = format!("WHERE {}", conditions.join(" AND "));
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    // First compute per-session stats, then bucket by message count
    let sql = format!(
        "WITH session_stats AS (
             SELECT session_id,
                    COUNT(*) as msg_count,
                    COALESCE(SUM(cost_cents), 0.0) as total_cost
             FROM messages
             {where_clause}
             AND session_id IS NOT NULL
             GROUP BY session_id
         )
         SELECT CASE
                    WHEN msg_count <= 5 THEN '1-5'
                    WHEN msg_count <= 15 THEN '6-15'
                    WHEN msg_count <= 30 THEN '16-30'
                    WHEN msg_count <= 60 THEN '31-60'
                    WHEN msg_count <= 100 THEN '61-100'
                    ELSE '100+'
                END as bucket,
                COUNT(*) as session_count,
                AVG(msg_count) as avg_messages,
                AVG(total_cost / msg_count) as avg_cost_per_msg,
                SUM(total_cost) as total_cost
         FROM session_stats
         GROUP BY bucket
         ORDER BY MIN(msg_count)",
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(SessionCostBucket {
                bucket: row.get(0)?,
                session_count: row.get(1)?,
                avg_messages: row.get(2)?,
                avg_cost_per_message_cents: row.get(3)?,
                total_cost_cents: row.get(4)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(rows)
}

// ---------------------------------------------------------------------------
// Cost Confidence
// ---------------------------------------------------------------------------

/// Cost confidence breakdown: message count and cost by confidence level.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CostConfidenceStat {
    pub confidence: String,
    pub message_count: u64,
    pub cost_cents: f64,
}

/// Query cost grouped by cost_confidence level.
pub fn cost_confidence_stats(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Vec<CostConfidenceStat>> {
    let filters = DimensionFilters::default();
    cost_confidence_stats_with_filters(conn, since, until, &filters)
}

pub fn cost_confidence_stats_with_filters(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    filters: &DimensionFilters,
) -> Result<Vec<CostConfidenceStat>> {
    let mut conditions = vec!["role = 'assistant'".to_string()];
    let mut param_values = Vec::new();
    if let Some(s) = since
        && is_valid_timestamp(s)
    {
        param_values.push(s.to_string());
        conditions.push(format!("timestamp >= ?{}", param_values.len()));
    }
    if let Some(u) = until
        && is_valid_timestamp(u)
    {
        param_values.push(u.to_string());
        conditions.push(format!("timestamp < ?{}", param_values.len()));
    }
    let model_expr = normalized_model_expr("model");
    let project_expr = normalized_project_expr("repo_id");
    let branch_expr = normalized_branch_expr("git_branch");
    let surface_expr = normalized_surface_expr("surface");
    apply_dimension_filters(
        &mut conditions,
        &mut param_values,
        filters,
        "COALESCE(provider, 'claude_code')",
        &model_expr,
        &project_expr,
        &branch_expr,
        &surface_expr,
    );
    let where_clause = format!("WHERE {}", conditions.join(" AND "));
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    let sql = format!(
        "SELECT COALESCE(cost_confidence, 'estimated') as conf,
                COUNT(*) as cnt,
                COALESCE(SUM(cost_cents), 0.0) as cost
         FROM messages {where_clause}
         GROUP BY conf
         ORDER BY cost DESC",
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(CostConfidenceStat {
                confidence: row.get(0)?,
                message_count: row.get(1)?,
                cost_cents: row.get(2)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(rows)
}

// ---------------------------------------------------------------------------
// Subagent Cost
// ---------------------------------------------------------------------------

/// Subagent vs main conversation cost breakdown.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SubagentCostStat {
    pub category: String,
    pub message_count: u64,
    pub cost_cents: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Query cost split between main conversation and subagent messages.
pub fn subagent_cost_stats(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Vec<SubagentCostStat>> {
    let filters = DimensionFilters::default();
    subagent_cost_stats_with_filters(conn, since, until, &filters)
}

pub fn subagent_cost_stats_with_filters(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    filters: &DimensionFilters,
) -> Result<Vec<SubagentCostStat>> {
    let mut conditions = vec!["role = 'assistant'".to_string()];
    let mut param_values = Vec::new();
    if let Some(s) = since
        && is_valid_timestamp(s)
    {
        param_values.push(s.to_string());
        conditions.push(format!("timestamp >= ?{}", param_values.len()));
    }
    if let Some(u) = until
        && is_valid_timestamp(u)
    {
        param_values.push(u.to_string());
        conditions.push(format!("timestamp < ?{}", param_values.len()));
    }
    let model_expr = normalized_model_expr("model");
    let project_expr = normalized_project_expr("repo_id");
    let branch_expr = normalized_branch_expr("git_branch");
    let surface_expr = normalized_surface_expr("surface");
    apply_dimension_filters(
        &mut conditions,
        &mut param_values,
        filters,
        "COALESCE(provider, 'claude_code')",
        &model_expr,
        &project_expr,
        &branch_expr,
        &surface_expr,
    );
    let where_clause = format!("WHERE {}", conditions.join(" AND "));
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    let sql = format!(
        "SELECT CASE WHEN parent_uuid IS NOT NULL THEN 'subagent' ELSE 'main' END as category,
                COUNT(*) as cnt,
                COALESCE(SUM(cost_cents), 0.0) as cost,
                COALESCE(SUM(input_tokens), 0) as inp,
                COALESCE(SUM(output_tokens), 0) as outp
         FROM messages {where_clause}
         GROUP BY category
         ORDER BY cost DESC",
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(SubagentCostStat {
                category: row.get(0)?,
                message_count: row.get(1)?,
                cost_cents: row.get(2)?,
                input_tokens: row.get(3)?,
                output_tokens: row.get(4)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(rows)
}

pub fn filter_options(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    limit: Option<usize>,
) -> Result<FilterOptions> {
    if rollups_available(conn)
        && let Some(window) = choose_rollup_window(since, until, true)
    {
        return filter_options_from_rollups(conn, &window, limit);
    }

    let mut conditions = vec!["role = 'assistant'".to_string()];
    let mut params: Vec<String> = Vec::new();
    if let Some(s) = since
        && is_valid_timestamp(s)
    {
        params.push(s.to_string());
        conditions.push(format!("timestamp >= ?{}", params.len()));
    }
    if let Some(u) = until
        && is_valid_timestamp(u)
    {
        params.push(u.to_string());
        conditions.push(format!("timestamp < ?{}", params.len()));
    }
    let where_clause = format!("WHERE {}", conditions.join(" AND "));

    fn distinct_values(
        conn: &Connection,
        sql: &str,
        params: &[String],
        limit: Option<usize>,
    ) -> Result<Vec<String>> {
        let mut all_params = params.to_vec();
        if let Some(value) = limit {
            all_params.push(value.to_string());
        }
        let param_refs: Vec<&dyn rusqlite::types::ToSql> = all_params
            .iter()
            .map(|s| s as &dyn rusqlite::types::ToSql)
            .collect();
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt
            .query_map(param_refs.as_slice(), |row| row.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    let limit_clause = if limit.is_some() {
        format!("LIMIT ?{}", params.len() + 1)
    } else {
        String::new()
    };

    let agents_sql = format!(
        "SELECT COALESCE(provider, 'claude_code') as value
         FROM messages
         {where_clause}
         GROUP BY value
         ORDER BY COUNT(*) DESC, value ASC
         {limit_clause}",
    );
    let models_sql = format!(
        "SELECT {} as value
         FROM messages
         {where_clause}
         GROUP BY value
         ORDER BY COUNT(*) DESC, value ASC
         {limit_clause}",
        normalized_model_expr("model"),
    );
    let projects_sql = format!(
        "SELECT {} as value
         FROM messages
         {where_clause}
         GROUP BY value
         ORDER BY COUNT(*) DESC, value ASC
         {limit_clause}",
        normalized_project_expr("repo_id"),
    );
    let branches_sql = format!(
        "SELECT {} as value
         FROM messages
         {where_clause}
         GROUP BY value
         ORDER BY COUNT(*) DESC, value ASC
         {limit_clause}",
        normalized_branch_expr("git_branch"),
    );

    Ok(FilterOptions {
        agents: distinct_values(conn, &agents_sql, &params, limit)?,
        models: distinct_values(conn, &models_sql, &params, limit)?,
        projects: distinct_values(conn, &projects_sql, &params, limit)?,
        branches: distinct_values(conn, &branches_sql, &params, limit)?,
    })
}

fn filter_options_from_rollups(
    conn: &Connection,
    window: &RollupWindow,
    limit: Option<usize>,
) -> Result<FilterOptions> {
    fn distinct_rollup_values(
        conn: &Connection,
        window: &RollupWindow,
        value_col: &str,
        limit: Option<usize>,
    ) -> Result<Vec<String>> {
        let mut conditions = vec!["role = 'assistant'".to_string()];
        let mut params: Vec<String> = Vec::new();
        append_rollup_time_filters(&mut conditions, &mut params, window);
        let where_clause = format!("WHERE {}", conditions.join(" AND "));
        let mut limit_clause = String::new();
        if let Some(limit_value) = limit {
            params.push(limit_value.to_string());
            limit_clause = format!("LIMIT ?{}", params.len());
        }
        let sql = format!(
            "SELECT {value_col} as value
             FROM {}
             {where_clause}
             GROUP BY value
             ORDER BY SUM(message_count) DESC, value ASC
             {limit_clause}",
            rollup_table(window.level)
        );
        let param_refs: Vec<&dyn rusqlite::types::ToSql> = params
            .iter()
            .map(|s| s as &dyn rusqlite::types::ToSql)
            .collect();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(param_refs.as_slice(), |row| row.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    Ok(FilterOptions {
        agents: distinct_rollup_values(conn, window, "provider", limit)?,
        models: distinct_rollup_values(conn, window, "model", limit)?,
        projects: distinct_rollup_values(conn, window, "repo_id", limit)?,
        branches: distinct_rollup_values(conn, window, "git_branch", limit)?,
    })
}

// ---------------------------------------------------------------------------
// Files — per-file cost attribution (R1.4, #292)
//
// Files come from the `file_path` tag emitted by `FileEnricher` when an
// assistant message's tool-use arguments point at a file inside the
// resolved repo root. The analytics layer joins `messages → tags` and
// splits cost proportionally when a single message carries multiple
// files, mirroring the ticket / activity roll-ups so the three dimensions
// compose cleanly.
// ---------------------------------------------------------------------------

const FILE_TAG_KEY: &str = crate::tag_keys::FILE_PATH;
const FILE_SOURCE_TAG_KEY: &str = crate::tag_keys::FILE_PATH_SOURCE;
const FILE_CONFIDENCE_TAG_KEY: &str = crate::tag_keys::FILE_PATH_CONFIDENCE;

/// Per-file aggregate cost row used by `GET /analytics/files` and the
/// `budi stats --files` CLI view. Mirrors [`TicketCost`] — same shape,
/// swapped dimension — so clients can render one component for both.
///
/// The list always carries an `(untagged)` row (assistant messages with
/// no `file_path` tag) so users can see how much activity is *not*
/// attributed to a file; that bucket should shrink as tool-arg coverage
/// improves.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FileCost {
    pub file_path: String,
    pub session_count: u64,
    pub message_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cost_cents: f64,
    /// Dominant repo (highest cost) for this file. Empty for the
    /// `(untagged)` row or when provenance is ambiguous.
    #[serde(default)]
    pub top_repo_id: String,
    /// Dominant branch (highest cost) for this file. Empty for the
    /// `(untagged)` row.
    #[serde(default)]
    pub top_branch: String,
    /// Dominant ticket id (highest cost) for this file, derived from
    /// the same message's `ticket_id` tag. Empty when the file was not
    /// worked on a ticket-bearing branch.
    #[serde(default)]
    pub top_ticket_id: String,
    /// Dominant `file_path_source` (`tool_arg` or `cwd_relative`).
    #[serde(default)]
    pub source: String,
}

/// Per-branch breakdown attached to a single file detail response.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FileBranchBreakdown {
    pub git_branch: String,
    pub repo_id: String,
    pub message_count: u64,
    pub session_count: u64,
    pub cost_cents: f64,
}

/// Per-ticket breakdown attached to a single file detail response.
/// Separate struct from [`FileBranchBreakdown`] so the wire format can
/// evolve independently as ticket attribution gets richer.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FileTicketBreakdown {
    pub ticket_id: String,
    pub message_count: u64,
    pub session_count: u64,
    pub cost_cents: f64,
}

/// Detail payload for `GET /analytics/files/{path}` and `budi stats
/// --file <PATH>`. Mirrors [`TicketCostDetail`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FileCostDetail {
    pub file_path: String,
    pub session_count: u64,
    pub message_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cost_cents: f64,
    pub repo_id: String,
    pub branches: Vec<FileBranchBreakdown>,
    pub tickets: Vec<FileTicketBreakdown>,
    /// Dominant `file_path_source` for the selection.
    #[serde(default)]
    pub source: String,
    /// Dominant `file_path_confidence` for the selection.
    #[serde(default)]
    pub confidence: String,
}

/// Query cost grouped by file path, sorted by cost descending. Includes
/// an `(untagged)` bucket for assistant messages that have no `file_path`
/// tag. Same proportional-split semantics as [`ticket_cost`].
pub fn file_cost(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    limit: usize,
) -> Result<Vec<FileCost>> {
    let filters = DimensionFilters::default();
    file_cost_with_filters(conn, since, until, &filters, limit)
}

pub fn file_cost_with_filters(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    filters: &DimensionFilters,
    limit: usize,
) -> Result<Vec<FileCost>> {
    let mut conditions = vec!["m.role = 'assistant'".to_string()];
    let mut param_values: Vec<String> = Vec::new();
    let mut idx = 0usize;
    if let Some(s) = since {
        idx += 1;
        param_values.push(s.to_string());
        conditions.push(format!("m.timestamp >= ?{idx}"));
    }
    if let Some(u) = until {
        idx += 1;
        param_values.push(u.to_string());
        conditions.push(format!("m.timestamp < ?{idx}"));
    }
    let model_expr = normalized_model_expr("m.model");
    let project_expr = normalized_project_expr("m.repo_id");
    let branch_expr = normalized_branch_expr("m.git_branch");
    let surface_expr = normalized_surface_expr("m.surface");
    apply_dimension_filters(
        &mut conditions,
        &mut param_values,
        filters,
        "COALESCE(m.provider, 'claude_code')",
        &model_expr,
        &project_expr,
        &branch_expr,
        &surface_expr,
    );
    let where_clause = format!("WHERE {}", conditions.join(" AND "));

    // (untagged) clause re-aliases to m2.*.
    let untagged_conditions: Vec<String> = conditions
        .iter()
        .map(|c| c.replace("m.role = 'assistant'", "m2.role = 'assistant'"))
        .collect();
    let untagged_conditions: Vec<String> = untagged_conditions
        .into_iter()
        .map(|c| c.replace("m.", "m2."))
        .collect();
    let untagged_where = format!("WHERE {}", untagged_conditions.join(" AND "));

    let limit_param_idx = param_values.len() + 1;
    param_values.push(limit.to_string());

    let sql = format!(
        "WITH msg_val_counts AS (
             SELECT message_id, COUNT(*) AS n_values
             FROM tags
             WHERE key = '{FILE_TAG_KEY}'
             GROUP BY message_id
         ),
         msg_source AS (
             SELECT message_id, MIN(value) AS source_value
             FROM tags
             WHERE key = '{FILE_SOURCE_TAG_KEY}'
             GROUP BY message_id
         ),
         msg_ticket AS (
             SELECT message_id, MIN(value) AS ticket_value
             FROM tags
             WHERE key = '{TICKET_TAG_KEY}'
             GROUP BY message_id
         ),
         tagged AS (
             SELECT t.value AS file_path,
                    m.session_id,
                    m.repo_id,
                    m.git_branch,
                    m.input_tokens,
                    m.output_tokens,
                    m.cache_read_tokens,
                    m.cache_creation_tokens,
                    m.cost_cents,
                    mvc.n_values,
                    COALESCE(ms.source_value, '') AS file_source,
                    COALESCE(mt.ticket_value, '') AS ticket_value
             FROM tags t
             JOIN msg_val_counts mvc ON mvc.message_id = t.message_id
             JOIN messages m ON m.id = t.message_id
             LEFT JOIN msg_source ms ON ms.message_id = t.message_id
             LEFT JOIN msg_ticket mt ON mt.message_id = t.message_id
             {where_clause}
             AND t.key = '{FILE_TAG_KEY}'
         ),
         per_file AS (
             SELECT file_path,
                    COUNT(DISTINCT session_id) AS sess,
                    COUNT(*) AS cnt,
                    COALESCE(SUM(input_tokens / n_values), 0) AS inp,
                    COALESCE(SUM(output_tokens / n_values), 0) AS outp,
                    COALESCE(SUM(cache_read_tokens / n_values), 0) AS cache_r,
                    COALESCE(SUM(cache_creation_tokens / n_values), 0) AS cache_c,
                    COALESCE(SUM(cost_cents / n_values), 0.0) AS cost
             FROM tagged
             GROUP BY file_path
         ),
         top_repo AS (
             SELECT file_path,
                    COALESCE(repo_id, '') AS repo_value,
                    SUM(cost_cents / n_values) AS repo_cost
             FROM tagged
             GROUP BY file_path, repo_value
         ),
         top_repo_pick AS (
             SELECT file_path, repo_value
             FROM (
                 SELECT file_path, repo_value, repo_cost,
                        ROW_NUMBER() OVER (
                            PARTITION BY file_path
                            ORDER BY repo_cost DESC, repo_value ASC
                        ) AS rn
                 FROM top_repo
                 WHERE repo_value != '' AND repo_value != 'unknown'
             )
             WHERE rn = 1
         ),
         top_branch AS (
             SELECT file_path,
                    CASE
                        WHEN COALESCE(git_branch, '') LIKE 'refs/heads/%'
                            THEN SUBSTR(COALESCE(git_branch, ''), 12)
                        ELSE COALESCE(git_branch, '')
                    END AS branch_value,
                    SUM(cost_cents / n_values) AS branch_cost
             FROM tagged
             GROUP BY file_path, branch_value
         ),
         top_branch_pick AS (
             SELECT file_path, branch_value
             FROM (
                 SELECT file_path, branch_value, branch_cost,
                        ROW_NUMBER() OVER (
                            PARTITION BY file_path
                            ORDER BY branch_cost DESC, branch_value ASC
                        ) AS rn
                 FROM top_branch
                 WHERE branch_value != ''
             )
             WHERE rn = 1
         ),
         top_ticket AS (
             SELECT file_path,
                    ticket_value,
                    SUM(cost_cents / n_values) AS ticket_cost
             FROM tagged
             GROUP BY file_path, ticket_value
         ),
         top_ticket_pick AS (
             SELECT file_path, ticket_value
             FROM (
                 SELECT file_path, ticket_value, ticket_cost,
                        ROW_NUMBER() OVER (
                            PARTITION BY file_path
                            ORDER BY ticket_cost DESC, ticket_value ASC
                        ) AS rn
                 FROM top_ticket
                 WHERE ticket_value != ''
             )
             WHERE rn = 1
         ),
         top_source AS (
             SELECT file_path,
                    file_source AS source_value,
                    SUM(cost_cents / n_values) AS source_cost
             FROM tagged
             GROUP BY file_path, source_value
         ),
         top_source_pick AS (
             SELECT file_path, source_value
             FROM (
                 SELECT file_path, source_value, source_cost,
                        ROW_NUMBER() OVER (
                            PARTITION BY file_path
                            ORDER BY source_cost DESC, source_value ASC
                        ) AS rn
                 FROM top_source
                 WHERE source_value != ''
             )
             WHERE rn = 1
         )
         SELECT pf.file_path,
                pf.sess, pf.cnt,
                pf.inp, pf.outp, pf.cache_r, pf.cache_c, pf.cost,
                COALESCE(trp.repo_value, '') AS top_repo,
                COALESCE(tbp.branch_value, '') AS top_branch,
                COALESCE(ttp.ticket_value, '') AS top_ticket,
                COALESCE(tsp.source_value, '') AS file_source
         FROM per_file pf
         LEFT JOIN top_repo_pick trp ON trp.file_path = pf.file_path
         LEFT JOIN top_branch_pick tbp ON tbp.file_path = pf.file_path
         LEFT JOIN top_ticket_pick ttp ON ttp.file_path = pf.file_path
         LEFT JOIN top_source_pick tsp ON tsp.file_path = pf.file_path

         UNION ALL

         SELECT '{UNTAGGED_DIMENSION}' AS file_path,
                COUNT(DISTINCT m2.session_id) AS sess,
                COUNT(*) AS cnt,
                COALESCE(SUM(m2.input_tokens), 0) AS inp,
                COALESCE(SUM(m2.output_tokens), 0) AS outp,
                COALESCE(SUM(m2.cache_read_tokens), 0) AS cache_r,
                COALESCE(SUM(m2.cache_creation_tokens), 0) AS cache_c,
                COALESCE(SUM(m2.cost_cents), 0.0) AS cost,
                '' AS top_repo,
                '' AS top_branch,
                '' AS top_ticket,
                '' AS file_source
         FROM messages m2
         {untagged_where}
         AND NOT EXISTS (
             SELECT 1 FROM tags t2
             WHERE t2.message_id = m2.id AND t2.key = '{FILE_TAG_KEY}'
         )

         ORDER BY cost DESC
         LIMIT ?{limit_param_idx}",
    );

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<FileCost> = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(FileCost {
                file_path: row.get(0)?,
                session_count: row.get(1)?,
                message_count: row.get(2)?,
                input_tokens: row.get(3)?,
                output_tokens: row.get(4)?,
                cache_read_tokens: row.get(5)?,
                cache_creation_tokens: row.get(6)?,
                cost_cents: row.get(7)?,
                top_repo_id: row.get(8)?,
                top_branch: row.get(9)?,
                top_ticket_id: row.get(10)?,
                source: row.get(11)?,
            })
        })?
        .filter_map(|r| r.ok())
        // Drop the (untagged) row when empty to avoid noise on freshly-imported DBs.
        .filter(|fc| !(fc.file_path == UNTAGGED_DIMENSION && fc.message_count == 0))
        .collect();

    Ok(rows)
}

/// Detail view for a single file: totals + dominant repo + per-branch and
/// per-ticket breakdowns. Returns `None` when no assistant messages carry
/// the file in the requested window.
pub fn file_cost_single(
    conn: &Connection,
    file_path: &str,
    repo_id: Option<&str>,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Option<FileCostDetail>> {
    let mut conditions = vec![
        "m.role = 'assistant'".to_string(),
        "t.key = ?1".to_string(),
        "t.value = ?2".to_string(),
    ];
    let mut param_values: Vec<String> = vec![FILE_TAG_KEY.to_string(), file_path.to_string()];
    let mut idx = 2usize;
    if let Some(repo) = repo_id {
        idx += 1;
        param_values.push(repo.to_string());
        conditions.push(format!("COALESCE(m.repo_id, '') = ?{idx}"));
    }
    if let Some(s) = since {
        idx += 1;
        param_values.push(s.to_string());
        conditions.push(format!("m.timestamp >= ?{idx}"));
    }
    if let Some(u) = until {
        idx += 1;
        param_values.push(u.to_string());
        conditions.push(format!("m.timestamp < ?{idx}"));
    }
    let where_clause = format!("WHERE {}", conditions.join(" AND "));

    let totals_sql = format!(
        "WITH msg_val_counts AS (
             SELECT message_id, COUNT(*) AS n_values
             FROM tags
             WHERE key = ?1
             GROUP BY message_id
         ),
         msg_source AS (
             SELECT message_id, MIN(value) AS source_value
             FROM tags
             WHERE key = '{FILE_SOURCE_TAG_KEY}'
             GROUP BY message_id
         ),
         msg_confidence AS (
             SELECT message_id, MIN(value) AS confidence_value
             FROM tags
             WHERE key = '{FILE_CONFIDENCE_TAG_KEY}'
             GROUP BY message_id
         ),
         selected AS (
             SELECT m.id AS message_id,
                    m.session_id,
                    m.repo_id,
                    m.input_tokens,
                    m.output_tokens,
                    m.cache_read_tokens,
                    m.cache_creation_tokens,
                    m.cost_cents,
                    mvc.n_values,
                    COALESCE(ms.source_value, '') AS file_source,
                    COALESCE(mc.confidence_value, '') AS file_confidence
             FROM tags t
             JOIN msg_val_counts mvc ON mvc.message_id = t.message_id
             JOIN messages m ON m.id = t.message_id
             LEFT JOIN msg_source ms ON ms.message_id = t.message_id
             LEFT JOIN msg_confidence mc ON mc.message_id = t.message_id
             {where_clause}
         ),
         source_pick AS (
             SELECT file_source,
                    SUM(cost_cents / n_values) AS source_cost
             FROM selected
             WHERE file_source != ''
             GROUP BY file_source
             ORDER BY source_cost DESC, file_source ASC
             LIMIT 1
         ),
         confidence_pick AS (
             SELECT file_confidence,
                    SUM(cost_cents / n_values) AS confidence_cost
             FROM selected
             WHERE file_confidence != ''
             GROUP BY file_confidence
             ORDER BY confidence_cost DESC, file_confidence ASC
             LIMIT 1
         )
         SELECT COUNT(DISTINCT session_id) AS sess,
                COUNT(*) AS cnt,
                COALESCE(SUM(input_tokens / n_values), 0) AS inp,
                COALESCE(SUM(output_tokens / n_values), 0) AS outp,
                COALESCE(SUM(cache_read_tokens / n_values), 0) AS cache_r,
                COALESCE(SUM(cache_creation_tokens / n_values), 0) AS cache_c,
                COALESCE(SUM(cost_cents / n_values), 0.0) AS cost,
                CASE WHEN COUNT(DISTINCT COALESCE(repo_id, '')) = 1
                     THEN COALESCE(MIN(repo_id), '')
                     ELSE '' END AS repo,
                COALESCE((SELECT file_source FROM source_pick), '') AS src,
                COALESCE((SELECT file_confidence FROM confidence_pick), '') AS conf
         FROM selected"
    );

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();
    let mut stmt = conn.prepare(&totals_sql)?;
    let totals = stmt.query_row(param_refs.as_slice(), |row| {
        Ok((
            row.get::<_, u64>(0)?,
            row.get::<_, u64>(1)?,
            row.get::<_, u64>(2)?,
            row.get::<_, u64>(3)?,
            row.get::<_, u64>(4)?,
            row.get::<_, u64>(5)?,
            row.get::<_, f64>(6)?,
            row.get::<_, String>(7)?,
            row.get::<_, String>(8)?,
            row.get::<_, String>(9)?,
        ))
    });
    let (sess, cnt, inp, outp, cache_r, cache_c, cost, repo, src, conf) = match totals {
        Ok(row) => row,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if cnt == 0 {
        return Ok(None);
    }

    // Per-branch breakdown.
    let branches_sql = format!(
        "WITH msg_val_counts AS (
             SELECT message_id, COUNT(*) AS n_values
             FROM tags
             WHERE key = ?1
             GROUP BY message_id
         )
         SELECT COALESCE(NULLIF(
                    CASE
                        WHEN COALESCE(m.git_branch, '') LIKE 'refs/heads/%'
                            THEN SUBSTR(COALESCE(m.git_branch, ''), 12)
                        ELSE COALESCE(m.git_branch, '')
                    END,
                    ''
                ), '{UNTAGGED_DIMENSION}') AS branch_value,
                COALESCE(m.repo_id, '') AS repo_value,
                COUNT(DISTINCT m.session_id) AS sess,
                COUNT(*) AS cnt,
                COALESCE(SUM(m.cost_cents / mvc.n_values), 0.0) AS cost
         FROM tags t
         JOIN msg_val_counts mvc ON mvc.message_id = t.message_id
         JOIN messages m ON m.id = t.message_id
         {where_clause}
         GROUP BY branch_value, repo_value
         ORDER BY cost DESC, branch_value ASC"
    );
    let mut stmt = conn.prepare(&branches_sql)?;
    let branches: Vec<FileBranchBreakdown> = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(FileBranchBreakdown {
                git_branch: row.get(0)?,
                repo_id: row.get(1)?,
                session_count: row.get(2)?,
                message_count: row.get(3)?,
                cost_cents: row.get(4)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    // Per-ticket breakdown — joins the same selected rows to their
    // `ticket_id` sibling tag (when present).
    let tickets_sql = format!(
        "WITH msg_val_counts AS (
             SELECT message_id, COUNT(*) AS n_values
             FROM tags
             WHERE key = ?1
             GROUP BY message_id
         ),
         msg_ticket AS (
             SELECT message_id, MIN(value) AS ticket_value
             FROM tags
             WHERE key = '{TICKET_TAG_KEY}'
             GROUP BY message_id
         )
         SELECT COALESCE(NULLIF(mt.ticket_value, ''), '{UNTAGGED_DIMENSION}') AS ticket_value,
                COUNT(DISTINCT m.session_id) AS sess,
                COUNT(*) AS cnt,
                COALESCE(SUM(m.cost_cents / mvc.n_values), 0.0) AS cost
         FROM tags t
         JOIN msg_val_counts mvc ON mvc.message_id = t.message_id
         JOIN messages m ON m.id = t.message_id
         LEFT JOIN msg_ticket mt ON mt.message_id = m.id
         {where_clause}
         GROUP BY ticket_value
         ORDER BY cost DESC, ticket_value ASC"
    );
    let mut stmt = conn.prepare(&tickets_sql)?;
    let tickets: Vec<FileTicketBreakdown> = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(FileTicketBreakdown {
                ticket_id: row.get(0)?,
                session_count: row.get(1)?,
                message_count: row.get(2)?,
                cost_cents: row.get(3)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(Some(FileCostDetail {
        file_path: file_path.to_string(),
        session_count: sess,
        message_count: cnt,
        input_tokens: inp,
        output_tokens: outp,
        cache_read_tokens: cache_r,
        cache_creation_tokens: cache_c,
        cost_cents: cost,
        repo_id: repo,
        branches,
        tickets,
        source: src,
        confidence: conf,
    }))
}
