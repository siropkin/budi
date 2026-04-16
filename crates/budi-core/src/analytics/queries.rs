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
}

impl DimensionFilters {
    pub fn normalize(mut self) -> Self {
        self.agents = normalize_values(&self.agents);
        self.models = normalize_values(&self.models);
        self.projects = normalize_values(&self.projects);
        self.branches = normalize_branches(&self.branches);
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

fn apply_dimension_filters(
    conditions: &mut Vec<String>,
    param_values: &mut Vec<String>,
    filters: &DimensionFilters,
    provider_expr: &str,
    model_expr: &str,
    project_expr: &str,
    branch_expr: &str,
) {
    append_in_condition(conditions, param_values, provider_expr, &filters.agents);
    append_in_condition(conditions, param_values, model_expr, &filters.models);
    append_in_condition(conditions, param_values, project_expr, &filters.projects);
    append_in_condition(conditions, param_values, branch_expr, &filters.branches);
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
    apply_dimension_filters(
        &mut conditions,
        &mut params,
        filters,
        "COALESCE(provider, 'claude_code')",
        &model_expr,
        &project_expr,
        &branch_expr,
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
                COALESCE(messages.git_branch, s.git_branch)
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
    apply_dimension_filters(
        &mut conditions,
        &mut param_values,
        filters,
        "COALESCE(provider, 'claude_code')",
        &model_expr,
        &project_expr,
        &branch_expr,
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
    apply_dimension_filters(
        &mut conditions,
        &mut param_values,
        filters,
        "COALESCE(provider, 'claude_code')",
        &model_expr,
        &project_expr,
        &branch_expr,
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
    apply_dimension_filters(
        &mut conditions,
        &mut param_values,
        filters,
        "COALESCE(provider, 'claude_code')",
        &model_expr,
        &project_expr,
        &branch_expr,
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
    apply_dimension_filters(
        &mut where_parts,
        &mut param_values,
        filters,
        "COALESCE(m.provider, 'claude_code')",
        &model_expr,
        &project_expr,
        &branch_expr,
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
    apply_dimension_filters(
        &mut conditions,
        &mut param_values,
        filters,
        "COALESCE(provider, 'claude_code')",
        &model_expr,
        &project_expr,
        &branch_expr,
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
    apply_dimension_filters(
        &mut conditions,
        &mut param_values,
        filters,
        "COALESCE(provider, 'claude_code')",
        &model_expr,
        &project_expr,
        &branch_expr,
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
    apply_dimension_filters(
        &mut conditions,
        &mut param_values,
        filters,
        "COALESCE(provider, 'claude_code')",
        &model_expr,
        &project_expr,
        &branch_expr,
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
// Statusline
// ---------------------------------------------------------------------------

/// Compact stats for the status line display.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StatuslineStats {
    pub today_cost: f64,
    pub week_cost: f64,
    pub month_cost: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health_tip: Option<String>,
    /// Per-message cost in cents for the active session (for statusline rate display).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_msg_cost: Option<f64>,
}

/// Parameters for requesting extra statusline data.
#[derive(Debug, Default, Clone, serde::Deserialize)]
pub struct StatuslineParams {
    pub session_id: Option<String>,
    pub branch: Option<String>,
    pub project_dir: Option<String>,
}

fn assistant_cost_since_from_rollups(conn: &Connection, since: &str) -> Option<f64> {
    if !rollups_available(conn) {
        return None;
    }
    let window = choose_rollup_window(Some(since), None, false)?;
    let mut conditions = vec!["role = 'assistant'".to_string()];
    let mut params: Vec<String> = Vec::new();
    append_rollup_time_filters(&mut conditions, &mut params, &window);
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

/// Compute cost stats for today/week/month, suitable for the CLI status line.
/// Optionally computes session/branch/project costs when params are provided.
pub fn statusline_stats(
    conn: &Connection,
    today: &str,
    week_start: &str,
    month_start: &str,
    params: &StatuslineParams,
) -> Result<StatuslineStats> {
    fn cost_since(conn: &Connection, since: &str) -> f64 {
        assistant_cost_since_from_rollups(conn, since)
            .unwrap_or_else(|| {
                conn.query_row(
                    "SELECT COALESCE(SUM(cost_cents), 0.0) FROM messages WHERE timestamp >= ?1 AND role = 'assistant'",
                    [since],
                    |r| r.get::<_, f64>(0),
                )
                .unwrap_or(0.0)
            })
            / 100.0
    }

    let today_cost = cost_since(conn, today);
    let week_cost = cost_since(conn, week_start);
    let month_cost = cost_since(conn, month_start);
    let normalized_session_id = params
        .session_id
        .as_deref()
        .map(crate::identity::normalize_session_id);

    // Session cost: total cost for a specific session
    let session_cost = normalized_session_id.as_ref().map(|sid| {
        conn.query_row(
            "SELECT COALESCE(SUM(cost_cents), 0.0) FROM messages WHERE session_id = ?1 AND role = 'assistant'",
            [sid],
            |r| r.get::<_, f64>(0),
        )
        .unwrap_or(0.0)
            / 100.0
    });

    // Branch cost: total cost for messages on a specific branch
    let branch_cost = params.branch.as_ref().map(|branch| {
        conn.query_row(
            "SELECT COALESCE(SUM(m.cost_cents), 0.0) \
             FROM messages m \
             WHERE m.git_branch = ?1 AND m.role = 'assistant'",
            [branch],
            |r| r.get::<_, f64>(0),
        )
        .unwrap_or(0.0)
            / 100.0
    });

    // Project cost: total cost for messages in a specific directory
    let project_cost = params.project_dir.as_ref().map(|dir| {
        conn.query_row(
            "SELECT COALESCE(SUM(cost_cents), 0.0) FROM messages WHERE cwd = ?1 AND role = 'assistant'",
            [dir],
            |r| r.get::<_, f64>(0),
        )
        .unwrap_or(0.0)
            / 100.0
    });

    // Active provider: most recent provider used today
    let active_provider: Option<String> = conn
        .query_row(
            "SELECT provider FROM messages \
             WHERE timestamp >= ?1 ORDER BY timestamp DESC LIMIT 1",
            [today],
            |r| r.get(0),
        )
        .ok();

    let (health_state, health_tip, session_msg_cost) = normalized_session_id
        .as_ref()
        .and_then(|sid| super::health::session_health(conn, Some(sid)).ok())
        .map(|h| {
            let avg = if h.message_count > 0 {
                Some(h.total_cost_cents / h.message_count as f64)
            } else {
                None
            };
            (Some(h.state), Some(h.tip), avg)
        })
        .unwrap_or((None, None, None));

    Ok(StatuslineStats {
        today_cost,
        week_cost,
        month_cost,
        session_cost,
        branch_cost,
        project_cost,
        active_provider,
        health_state,
        health_tip,
        session_msg_cost,
    })
}

// ---------------------------------------------------------------------------
// Provider Stats
// ---------------------------------------------------------------------------

/// Per-provider aggregate stats for the /analytics/providers endpoint.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProviderStats {
    pub provider: String,
    pub display_name: String,
    pub message_count: u64,
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
    );
    let sql = format!(
        "SELECT provider as p,
                COALESCE(SUM(message_count), 0) as msgs,
                COALESCE(SUM(input_tokens), 0),
                COALESCE(SUM(output_tokens), 0),
                COALESCE(SUM(cache_creation_tokens), 0),
                COALESCE(SUM(cache_read_tokens), 0),
                COALESCE(SUM(cost_cents), 0.0)
         FROM {}
         WHERE {}
         GROUP BY p
         ORDER BY msgs DESC",
        rollup_table(window.level),
        conditions.join(" AND ")
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
                row.get::<_, f64>(6)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect::<Vec<_>>();

    let providers = crate::provider::all_providers();
    let mut result = Vec::new();
    for (prov, messages, input, output, cache_create, cache_read, sum_cost_cents) in rows {
        let display_name = providers
            .iter()
            .find(|p| p.name() == prov)
            .map(|p| p.display_name().to_string())
            .unwrap_or_else(|| prov.clone());
        let estimated_cost = sum_cost_cents.round() / 100.0;
        result.push(ProviderStats {
            provider: prov,
            display_name,
            message_count: messages,
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
    apply_dimension_filters(
        &mut conditions,
        &mut param_values,
        filters,
        "COALESCE(provider, 'claude_code')",
        &model_expr,
        &project_expr,
        &branch_expr,
    );
    let where_clause = format!("WHERE {}", conditions.join(" AND "));
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();
    let sql = format!(
        "SELECT provider as p,
                COUNT(*) as msgs,
                COALESCE(SUM(input_tokens), 0),
                COALESCE(SUM(output_tokens), 0),
                COALESCE(SUM(cache_creation_tokens), 0),
                COALESCE(SUM(cache_read_tokens), 0),
                COALESCE(SUM(cost_cents), 0.0)
         FROM messages {}
         GROUP BY p ORDER BY msgs DESC",
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
                row.get::<_, f64>(6)?,
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

    for (prov, messages, input, output, cache_create, cache_read, sum_cost_cents) in rows {
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
            message_count: messages,
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
    apply_dimension_filters(
        &mut conditions,
        &mut param_values,
        filters,
        "COALESCE(provider, 'claude_code')",
        &model_expr,
        &project_expr,
        &branch_expr,
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
        let pricing = match prov.as_str() {
            "cursor" => crate::providers::cursor::cursor_pricing_for_model(model),
            _ => crate::providers::claude_code::claude_pricing_for_model(model),
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
    apply_dimension_filters(
        &mut conditions,
        &mut param_values,
        filters,
        "COALESCE(provider, 'claude_code')",
        &model_expr,
        &project_expr,
        &branch_expr,
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
    apply_dimension_filters(
        &mut conditions,
        &mut param_values,
        filters,
        "COALESCE(provider, 'claude_code')",
        &model_expr,
        &project_expr,
        &branch_expr,
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
    apply_dimension_filters(
        &mut conditions,
        &mut param_values,
        filters,
        "COALESCE(provider, 'claude_code')",
        &model_expr,
        &project_expr,
        &branch_expr,
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
