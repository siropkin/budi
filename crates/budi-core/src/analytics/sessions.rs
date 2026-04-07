//! Session-specific queries: listing, tags, messages, and audit diagnostics.

use anyhow::Result;
use rusqlite::{Connection, params};

use super::{DimensionFilters, MessageRow, UNTAGGED_DIMENSION};

// ---------------------------------------------------------------------------
// Session Audit
// ---------------------------------------------------------------------------

/// Session attribution diagnostics (ingestion quality).
///
/// Summarizes: assistant messages missing `session_id`, sessions with no messages,
/// and per-provider share of assistant rows that have a session. Intended for
/// debugging provider/hook coverage — exposed over HTTP as `GET /analytics/session-audit`
/// (not used by the dashboard or CLI today).
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionAudit {
    pub assistant_rows_total: u64,
    pub assistant_rows_no_session: u64,
    pub sessions_total: u64,
    pub sessions_orphaned: u64,
    pub provider_coverage: Vec<ProviderCoverage>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ProviderCoverage {
    pub provider: String,
    pub assistant_total: u64,
    pub with_session: u64,
    pub coverage_pct: f64,
}

pub fn session_audit(conn: &Connection) -> Result<SessionAudit> {
    let assistant_rows_total: u64 = conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE role = 'assistant'",
        [],
        |r| r.get(0),
    )?;
    let assistant_rows_no_session: u64 = conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE role = 'assistant' AND session_id IS NULL",
        [],
        |r| r.get(0),
    )?;
    let sessions_total: u64 = conn.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))?;
    let sessions_orphaned: u64 = conn.query_row(
        "SELECT COUNT(*) FROM sessions s
         WHERE NOT EXISTS (SELECT 1 FROM messages m WHERE m.session_id = s.id)",
        [],
        |r| r.get(0),
    )?;

    let mut stmt = conn.prepare(
        "SELECT COALESCE(provider, 'claude_code'),
                COUNT(*),
                SUM(CASE WHEN session_id IS NOT NULL THEN 1 ELSE 0 END)
         FROM messages WHERE role = 'assistant'
         GROUP BY COALESCE(provider, 'claude_code')",
    )?;
    let provider_coverage = stmt
        .query_map([], |row| {
            let total: u64 = row.get(1)?;
            let with_session: u64 = row.get(2)?;
            Ok(ProviderCoverage {
                provider: row.get(0)?,
                assistant_total: total,
                with_session,
                coverage_pct: if total > 0 {
                    with_session as f64 / total as f64 * 100.0
                } else {
                    0.0
                },
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(SessionAudit {
        assistant_rows_total,
        assistant_rows_no_session,
        sessions_total,
        sessions_orphaned,
        provider_coverage,
    })
}

// ---------------------------------------------------------------------------
// Session List
// ---------------------------------------------------------------------------

/// Session list entry for the Sessions page.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionListEntry {
    #[serde(alias = "session_id")]
    pub id: String,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub duration_ms: Option<i64>,
    pub message_count: u64,
    pub cost_cents: f64,
    pub models: Vec<String>,
    pub provider: String,
    pub repo_ids: Vec<String>,
    pub git_branches: Vec<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// Paginated session list result.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PaginatedSessions {
    pub sessions: Vec<SessionListEntry>,
    pub total_count: u64,
}

/// Parameters for session list queries.
pub struct SessionListParams<'a> {
    pub since: Option<&'a str>,
    pub until: Option<&'a str>,
    pub search: Option<&'a str>,
    pub sort_by: Option<&'a str>,
    pub sort_asc: bool,
    pub limit: usize,
    pub offset: usize,
}

fn parse_models_csv(raw: Option<String>) -> Vec<String> {
    parse_string_list_csv(raw)
}

fn parse_string_list_csv(raw: Option<String>) -> Vec<String> {
    raw.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(|m| m.to_string())
        .collect()
}

fn append_in_condition(
    conditions: &mut Vec<String>,
    param_values: &mut Vec<String>,
    expression: &str,
    values: &[String],
) {
    let mut placeholders = Vec::new();
    for value in values {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            continue;
        }
        param_values.push(trimmed.to_string());
        placeholders.push(format!("?{}", param_values.len()));
    }
    if !placeholders.is_empty() {
        conditions.push(format!("{expression} IN ({})", placeholders.join(", ")));
    }
}

fn apply_session_dimension_filters(
    conditions: &mut Vec<String>,
    param_values: &mut Vec<String>,
    filters: &DimensionFilters,
) {
    append_in_condition(
        conditions,
        param_values,
        "COALESCE(m.provider, 'claude_code')",
        &filters.agents,
    );
    append_in_condition(
        conditions,
        param_values,
        &format!(
            "CASE WHEN m.model IS NULL OR m.model = '' OR SUBSTR(m.model, 1, 1) = '<' THEN '{UNTAGGED_DIMENSION}' ELSE m.model END"
        ),
        &filters.models,
    );
    append_in_condition(
        conditions,
        param_values,
        &format!(
            "COALESCE(NULLIF(NULLIF(COALESCE(m.repo_id, s.repo_id), ''), 'unknown'), '{UNTAGGED_DIMENSION}')"
        ),
        &filters.projects,
    );
    let normalized_branches = filters
        .branches
        .iter()
        .map(|value| {
            let trimmed = value.trim();
            let normalized = trimmed.strip_prefix("refs/heads/").unwrap_or(trimmed);
            if normalized.is_empty() {
                UNTAGGED_DIMENSION.to_string()
            } else {
                normalized.to_string()
            }
        })
        .collect::<Vec<_>>();
    append_in_condition(
        conditions,
        param_values,
        &format!(
            "COALESCE(NULLIF(CASE WHEN COALESCE(m.git_branch, s.git_branch, '') LIKE 'refs/heads/%' THEN SUBSTR(COALESCE(m.git_branch, s.git_branch, ''), 12) ELSE COALESCE(m.git_branch, s.git_branch, '') END, ''), '{UNTAGGED_DIMENSION}')"
        ),
        &normalized_branches,
    );
}

/// Query sessions with cost aggregated from messages.
pub fn session_list(conn: &Connection, p: &SessionListParams) -> Result<PaginatedSessions> {
    let filters = DimensionFilters::default();
    session_list_with_filters(conn, p, &filters)
}

pub fn session_list_with_filters(
    conn: &Connection,
    p: &SessionListParams,
    filters: &DimensionFilters,
) -> Result<PaginatedSessions> {
    let mut conditions = vec!["m.role = 'assistant'".to_string()];
    let mut param_values: Vec<String> = Vec::new();
    if let Some(s) = p.since {
        param_values.push(s.to_string());
        conditions.push(format!("m.timestamp >= ?{}", param_values.len()));
    }
    if let Some(u) = p.until {
        param_values.push(u.to_string());
        conditions.push(format!("m.timestamp < ?{}", param_values.len()));
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
            "(m.model LIKE ?{idx} ESCAPE '\\' OR m.repo_id LIKE ?{idx} ESCAPE '\\' OR m.provider LIKE ?{idx} ESCAPE '\\' OR COALESCE(m.git_branch, s.git_branch) LIKE ?{idx} ESCAPE '\\' OR s.title LIKE ?{idx} ESCAPE '\\')"
        ));
    }
    apply_session_dimension_filters(&mut conditions, &mut param_values, filters);
    let where_clause = format!("WHERE {}", conditions.join(" AND "));

    // Count total matching sessions using only the filter params (no limit/offset)
    let count_param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();
    let count_sql = format!(
        "WITH session_agg AS (
             SELECT m.session_id
             FROM messages m
             LEFT JOIN sessions s ON s.id = m.session_id
             {where_clause}
             AND m.session_id IS NOT NULL
             GROUP BY m.session_id
         )
         SELECT COUNT(*) FROM session_agg"
    );
    let total_count: u64 =
        conn.query_row(&count_sql, count_param_refs.as_slice(), |row| row.get(0))?;

    param_values.push(p.limit.to_string());
    let limit_idx = param_values.len();
    param_values.push(p.offset.to_string());
    let offset_idx = param_values.len();

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    let dir = if p.sort_asc { "ASC" } else { "DESC" };
    let order_expr = match p.sort_by.unwrap_or("started_at") {
        "started_at" => format!("sa.started_at {dir}"),
        "duration" => {
            // duration_ms from hooks, fallback to computed from timestamps
            let col = "COALESCE(sa.duration_ms, (julianday(sa.ended_at) - julianday(sa.started_at)) * 86400000)";
            if p.sort_asc {
                format!("({col} IS NULL) ASC, {col} {dir}")
            } else {
                format!("{col} {dir}")
            }
        }
        "title" => {
            if p.sort_asc {
                format!("(sa.title IS NULL OR sa.title = '') ASC, sa.title {dir}")
            } else {
                format!("sa.title {dir}")
            }
        }
        "model" => format!("sa.models_csv {dir}"),
        "provider" => format!("sa.provider {dir}"),
        "repo_id" => {
            if p.sort_asc {
                format!("(sa.repo_id IS NULL OR sa.repo_id = '') ASC, sa.repo_id {dir}")
            } else {
                format!("sa.repo_id {dir}")
            }
        }
        "git_branch" | "branch" => {
            if p.sort_asc {
                format!("(sa.git_branch IS NULL OR sa.git_branch = '') ASC, sa.git_branch {dir}")
            } else {
                format!("sa.git_branch {dir}")
            }
        }
        "tokens" => format!("(sa.inp + sa.outp) {dir}"),
        _ => format!("sa.cost {dir}"),
    };
    let sql = format!(
        "WITH session_agg AS (
             SELECT m.session_id,
                    MIN(m.timestamp) as started_at,
                    MAX(m.timestamp) as ended_at,
                    COUNT(*) as msg_count,
                    COALESCE(SUM(m.cost_cents), 0.0) as cost,
                    (SELECT GROUP_CONCAT(sub.model, ',') FROM (
                         SELECT m2.model FROM messages m2
                         WHERE m2.session_id = m.session_id AND m2.role = 'assistant'
                           AND m2.model IS NOT NULL AND m2.model != '' AND SUBSTR(m2.model, 1, 1) != '<'
                         GROUP BY m2.model ORDER BY SUM(m2.cost_cents) DESC
                     ) sub) as models_csv,
                    COALESCE(MAX(m.provider), 'claude_code') as provider,
                    COALESCE(
                        (
                            SELECT m2.repo_id
                            FROM messages m2
                            WHERE m2.session_id = m.session_id
                              AND m2.role = 'assistant'
                              AND m2.repo_id IS NOT NULL
                              AND m2.repo_id != ''
                              AND m2.repo_id != 'unknown'
                            GROUP BY m2.repo_id
                            ORDER BY SUM(m2.cost_cents) DESC, COUNT(*) DESC, m2.repo_id ASC
                            LIMIT 1
                        ),
                        MAX(s.repo_id)
                    ) as repo_id,
                    (SELECT GROUP_CONCAT(sub.repo_id, ',') FROM (
                         SELECT m2.repo_id
                         FROM messages m2
                         WHERE m2.session_id = m.session_id
                           AND m2.role = 'assistant'
                           AND m2.repo_id IS NOT NULL
                           AND m2.repo_id != ''
                           AND m2.repo_id != 'unknown'
                         GROUP BY m2.repo_id
                         ORDER BY SUM(m2.cost_cents) DESC, COUNT(*) DESC, m2.repo_id ASC
                     ) sub) as repo_ids_csv,
                    COALESCE(
                        (
                            SELECT branch_value
                            FROM (
                                SELECT
                                    CASE
                                        WHEN m2.git_branch LIKE 'refs/heads/%' THEN SUBSTR(m2.git_branch, 12)
                                        ELSE m2.git_branch
                                    END as branch_value,
                                    SUM(m2.cost_cents) as branch_cost,
                                    COUNT(*) as branch_count
                                FROM messages m2
                                WHERE m2.session_id = m.session_id
                                  AND m2.role = 'assistant'
                                  AND m2.git_branch IS NOT NULL
                                  AND m2.git_branch != ''
                                GROUP BY branch_value
                                ORDER BY branch_cost DESC, branch_count DESC, branch_value ASC
                                LIMIT 1
                            ) dominant_branch
                        ),
                        MAX(s.git_branch)
                    ) as git_branch,
                    (SELECT GROUP_CONCAT(sub.branch_value, ',') FROM (
                         SELECT
                             CASE
                                 WHEN m2.git_branch LIKE 'refs/heads/%' THEN SUBSTR(m2.git_branch, 12)
                                 ELSE m2.git_branch
                             END as branch_value
                         FROM messages m2
                         WHERE m2.session_id = m.session_id
                           AND m2.role = 'assistant'
                           AND m2.git_branch IS NOT NULL
                           AND m2.git_branch != ''
                         GROUP BY branch_value
                         ORDER BY SUM(m2.cost_cents) DESC, COUNT(*) DESC, branch_value ASC
                     ) sub) as git_branches_csv,
                    COALESCE(SUM(m.input_tokens), 0) as inp,
                    COALESCE(SUM(m.output_tokens), 0) as outp,
                    COALESCE(s.duration_ms,
                        CAST((julianday(MAX(m.timestamp)) - julianday(MIN(m.timestamp))) * 86400000 AS INTEGER)
                    ) as duration_ms,
                    s.title
             FROM messages m
             LEFT JOIN sessions s ON s.id = m.session_id
             {where_clause}
             AND m.session_id IS NOT NULL
             GROUP BY m.session_id
         )
         SELECT COUNT(*) OVER() as total,
                sa.session_id, sa.started_at, sa.ended_at, sa.duration_ms,
                sa.msg_count, sa.cost, sa.models_csv, sa.provider,
                sa.repo_ids_csv, sa.git_branches_csv, sa.inp, sa.outp, sa.title
         FROM session_agg sa
         ORDER BY {order_expr}
         LIMIT ?{limit_idx} OFFSET ?{offset_idx}",
    );

    let mut stmt = conn.prepare(&sql)?;
    let sessions: Vec<SessionListEntry> = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(SessionListEntry {
                id: row.get(1)?,
                started_at: row.get(2)?,
                ended_at: row.get(3)?,
                duration_ms: row.get(4)?,
                message_count: row.get(5)?,
                cost_cents: row.get(6)?,
                models: parse_models_csv(row.get(7)?),
                provider: row.get::<_, String>(8)?,
                repo_ids: parse_string_list_csv(row.get(9)?),
                git_branches: parse_string_list_csv(row.get(10)?),
                input_tokens: row.get(11)?,
                output_tokens: row.get(12)?,
                health_state: None,
                title: row.get(13)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(PaginatedSessions {
        sessions,
        total_count,
    })
}

/// Get a single session summary row for session detail metadata.
pub fn session_detail(conn: &Connection, session_id: &str) -> Result<Option<SessionListEntry>> {
    let row = conn.query_row(
        "WITH session_agg AS (
             SELECT sid.session_id,
                    COALESCE(MIN(m.timestamp), MAX(s.started_at)) as started_at,
                    COALESCE(MAX(m.timestamp), MAX(s.ended_at)) as ended_at,
                    COALESCE(
                        MAX(s.duration_ms),
                        CASE
                            WHEN MIN(m.timestamp) IS NOT NULL AND MAX(m.timestamp) IS NOT NULL
                            THEN CAST((julianday(MAX(m.timestamp)) - julianday(MIN(m.timestamp))) * 86400000 AS INTEGER)
                            ELSE NULL
                        END
                    ) as duration_ms,
                    COUNT(m.id) as msg_count,
                    COALESCE(SUM(m.cost_cents), 0.0) as cost,
                    (SELECT GROUP_CONCAT(sub.model, ',') FROM (
                         SELECT m2.model FROM messages m2
                         WHERE m2.session_id = sid.session_id AND m2.role = 'assistant'
                           AND m2.model IS NOT NULL AND m2.model != '' AND SUBSTR(m2.model, 1, 1) != '<'
                         GROUP BY m2.model ORDER BY SUM(m2.cost_cents) DESC
                     ) sub) as models_csv,
                    COALESCE(MAX(m.provider), MAX(s.provider), 'claude_code') as provider,
                    COALESCE(
                        (
                            SELECT m2.repo_id
                            FROM messages m2
                            WHERE m2.session_id = sid.session_id
                              AND m2.role = 'assistant'
                              AND m2.repo_id IS NOT NULL
                              AND m2.repo_id != ''
                              AND m2.repo_id != 'unknown'
                            GROUP BY m2.repo_id
                            ORDER BY SUM(m2.cost_cents) DESC, COUNT(*) DESC, m2.repo_id ASC
                            LIMIT 1
                        ),
                        MAX(s.repo_id)
                    ) as repo_id,
                    (SELECT GROUP_CONCAT(sub.repo_id, ',') FROM (
                         SELECT m2.repo_id
                         FROM messages m2
                         WHERE m2.session_id = sid.session_id
                           AND m2.role = 'assistant'
                           AND m2.repo_id IS NOT NULL
                           AND m2.repo_id != ''
                           AND m2.repo_id != 'unknown'
                         GROUP BY m2.repo_id
                         ORDER BY SUM(m2.cost_cents) DESC, COUNT(*) DESC, m2.repo_id ASC
                     ) sub) as repo_ids_csv,
                    COALESCE(
                        (
                            SELECT branch_value
                            FROM (
                                SELECT
                                    CASE
                                        WHEN m2.git_branch LIKE 'refs/heads/%' THEN SUBSTR(m2.git_branch, 12)
                                        ELSE m2.git_branch
                                    END as branch_value,
                                    SUM(m2.cost_cents) as branch_cost,
                                    COUNT(*) as branch_count
                                FROM messages m2
                                WHERE m2.session_id = sid.session_id
                                  AND m2.role = 'assistant'
                                  AND m2.git_branch IS NOT NULL
                                  AND m2.git_branch != ''
                                GROUP BY branch_value
                                ORDER BY branch_cost DESC, branch_count DESC, branch_value ASC
                                LIMIT 1
                            ) dominant_branch
                        ),
                        MAX(s.git_branch)
                    ) as git_branch,
                    (SELECT GROUP_CONCAT(sub.branch_value, ',') FROM (
                         SELECT
                             CASE
                                 WHEN m2.git_branch LIKE 'refs/heads/%' THEN SUBSTR(m2.git_branch, 12)
                                 ELSE m2.git_branch
                             END as branch_value
                         FROM messages m2
                         WHERE m2.session_id = sid.session_id
                           AND m2.role = 'assistant'
                           AND m2.git_branch IS NOT NULL
                           AND m2.git_branch != ''
                         GROUP BY branch_value
                         ORDER BY SUM(m2.cost_cents) DESC, COUNT(*) DESC, branch_value ASC
                     ) sub) as git_branches_csv,
                    COALESCE(SUM(m.input_tokens), 0) as inp,
                    COALESCE(SUM(m.output_tokens), 0) as outp,
                    MAX(s.title) as title
             FROM (SELECT ?1 AS session_id) sid
             LEFT JOIN sessions s ON s.id = sid.session_id
             LEFT JOIN messages m ON m.session_id = sid.session_id AND m.role = 'assistant'
             GROUP BY sid.session_id
             HAVING COUNT(m.id) > 0 OR MAX(s.id) IS NOT NULL
         )
         SELECT session_id, started_at, ended_at, duration_ms, msg_count, cost,
                models_csv, provider, repo_ids_csv, git_branches_csv,
                inp, outp, title
         FROM session_agg",
        params![session_id],
        |row| {
            Ok(SessionListEntry {
                id: row.get(0)?,
                started_at: row.get(1)?,
                ended_at: row.get(2)?,
                duration_ms: row.get(3)?,
                message_count: row.get(4)?,
                cost_cents: row.get(5)?,
                models: parse_models_csv(row.get(6)?),
                provider: row.get(7)?,
                repo_ids: parse_string_list_csv(row.get(8)?),
                git_branches: parse_string_list_csv(row.get(9)?),
                input_tokens: row.get(10)?,
                output_tokens: row.get(11)?,
                health_state: None,
                title: row.get(12)?,
            })
        },
    );

    match row {
        Ok(entry) => Ok(Some(entry)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

// ---------------------------------------------------------------------------
// Session Tags, Messages, Hook Events, Message Detail
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionMessageRoles {
    Assistant,
    All,
}

impl SessionMessageRoles {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Assistant => "assistant",
            Self::All => "all",
        }
    }
}

impl std::str::FromStr for SessionMessageRoles {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "assistant" => Ok(Self::Assistant),
            "all" => Ok(Self::All),
            other => Err(format!(
                "invalid roles '{other}'; valid values: assistant, all"
            )),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionHookEventRow {
    pub id: i64,
    pub timestamp: String,
    pub event: String,
    pub provider: String,
    pub session_id: Option<String>,
    pub message_id: Option<String>,
    pub link_confidence: Option<String>,
    pub tool_name: Option<String>,
    pub tool_use_id: Option<String>,
    pub tool_duration_ms: Option<i64>,
    pub mcp_server: Option<String>,
    pub message_request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_json: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SessionHookEventsParams<'a> {
    pub linked_only: bool,
    pub event: Option<&'a str>,
    pub limit: usize,
    pub offset: usize,
    pub include_raw: bool,
}

#[derive(Debug, Clone)]
pub struct SessionOtelEventsParams {
    pub linked_only: bool,
    pub limit: usize,
    pub offset: usize,
    pub include_raw: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OtelEventRow {
    pub id: i64,
    pub event_name: String,
    pub timestamp: String,
    pub timestamp_nano: Option<String>,
    pub session_id: Option<String>,
    pub message_id: Option<String>,
    pub model: Option<String>,
    pub cost_usd_reported: Option<f64>,
    pub cost_cents_computed: Option<f64>,
    pub processed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_json: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MessageDetail {
    pub message: MessageRow,
    pub tags: Vec<super::SessionTag>,
    pub tools: Vec<String>,
    pub hook_events: Vec<SessionHookEventRow>,
    pub otel_events: Vec<OtelEventRow>,
}

fn message_tools(conn: &Connection, message_uuid: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT value
         FROM tags
         WHERE message_id = ?1
           AND key = 'tool'
         ORDER BY value ASC",
    )?;
    let tools = stmt
        .query_map(params![message_uuid], |row| row.get::<_, String>(0))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(tools)
}

fn message_tags(conn: &Connection, message_id: &str) -> Result<Vec<super::SessionTag>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT key, value
         FROM tags
         WHERE message_id = ?1
           AND key NOT IN ('tool_use_id')
         ORDER BY key, value",
    )?;
    let tags = stmt
        .query_map(params![message_id], |row| {
            Ok(super::SessionTag {
                key: row.get(0)?,
                value: row.get(1)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(tags)
}

fn enrich_message_rows(
    conn: &Connection,
    rows: &mut [MessageRow],
    include_tags: bool,
) -> Result<()> {
    for row in rows {
        row.tools = message_tools(conn, &row.id)?;
        if include_tags {
            row.tags = message_tags(conn, &row.id)?;
        }
    }
    Ok(())
}

/// Get distinct tags for a session.
pub fn session_tags(conn: &Connection, session_id: &str) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT t.key, t.value
         FROM tags t
         JOIN messages m ON t.message_id = m.id
         WHERE m.session_id = ?1
           AND t.key NOT IN ('repo', 'branch', 'dominant_tool', 'tool_use_id', 'provider', 'model', 'cost_confidence')
         ORDER BY key, value",
    )?;
    let rows = stmt
        .query_map(params![session_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

/// Messages within a specific session for drill-down (assistant-only default).
pub fn session_messages(conn: &Connection, session_id: &str) -> Result<Vec<MessageRow>> {
    session_messages_with_roles(conn, session_id, SessionMessageRoles::Assistant)
}

pub fn session_messages_with_roles(
    conn: &Connection,
    session_id: &str,
    roles: SessionMessageRoles,
) -> Result<Vec<MessageRow>> {
    let mut sql = String::from(
        "SELECT id, timestamp, role, model,
                COALESCE(provider, 'claude_code'),
                repo_id,
                input_tokens, output_tokens,
                cache_creation_tokens, cache_read_tokens,
                COALESCE(cost_cents, 0.0),
                COALESCE(cost_confidence, 'estimated'),
                git_branch,
                request_id
         FROM messages
         WHERE session_id = ?1",
    );
    if roles == SessionMessageRoles::Assistant {
        sql.push_str(" AND role = 'assistant'");
    }
    sql.push_str(" ORDER BY timestamp ASC");

    let mut stmt = conn.prepare(&sql)?;
    let mut rows: Vec<MessageRow> = stmt
        .query_map(params![session_id], |row| {
            Ok(MessageRow {
                id: row.get(0)?,
                session_id: Some(session_id.to_string()),
                timestamp: row.get(1)?,
                role: row.get(2)?,
                model: row.get(3)?,
                provider: row.get(4)?,
                repo_id: row.get(5)?,
                input_tokens: row.get(6)?,
                output_tokens: row.get(7)?,
                cache_creation_tokens: row.get(8)?,
                cache_read_tokens: row.get(9)?,
                cost_cents: row.get(10)?,
                cost_confidence: row.get(11)?,
                git_branch: row.get(12)?,
                request_id: row.get(13)?,
                assistant_sequence: None,
                tools: Vec::new(),
                tags: Vec::new(),
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    enrich_message_rows(conn, &mut rows, true)?;

    Ok(rows)
}

#[derive(Debug, Clone)]
pub struct SessionMessageListParams<'a> {
    pub roles: SessionMessageRoles,
    pub sort_by: Option<&'a str>,
    pub sort_asc: bool,
    pub limit: usize,
    pub offset: usize,
}

pub fn session_message_list(
    conn: &Connection,
    session_id: &str,
    p: &SessionMessageListParams<'_>,
) -> Result<super::PaginatedMessages> {
    let mut conditions = vec!["m.session_id = ?1".to_string()];
    if p.roles == SessionMessageRoles::Assistant {
        conditions.push("m.role = 'assistant'".to_string());
    }
    let where_clause = format!("WHERE {}", conditions.join(" AND "));

    let dir = if p.sort_asc { "ASC" } else { "DESC" };
    let order_expr = match p.sort_by.unwrap_or("timestamp") {
        "provider" => format!("m.provider {dir}"),
        "model" => {
            if p.sort_asc {
                format!("(m.model IS NULL OR m.model = '') ASC, m.model {dir}")
            } else {
                format!("m.model {dir}")
            }
        }
        "tokens" => format!("(m.input_tokens + m.output_tokens) {dir}"),
        "cost" => format!("COALESCE(m.cost_cents, 0.0) {dir}"),
        "repo_id" => {
            if p.sort_asc {
                format!("(m.repo_id IS NULL OR m.repo_id = '') ASC, m.repo_id {dir}")
            } else {
                format!("m.repo_id {dir}")
            }
        }
        "git_branch" | "branch" => {
            if p.sort_asc {
                format!("(m.git_branch IS NULL OR m.git_branch = '') ASC, m.git_branch {dir}")
            } else {
                format!("m.git_branch {dir}")
            }
        }
        _ => format!("m.timestamp {dir}"),
    };

    let count_sql = format!("SELECT COUNT(*) FROM messages m {where_clause}");
    let total_count: u64 = conn.query_row(&count_sql, params![session_id], |row| row.get(0))?;

    let sql = format!(
        "WITH assistant_sequence AS (
             SELECT id, ROW_NUMBER() OVER (ORDER BY timestamp ASC, id ASC) as assistant_sequence
             FROM messages
             WHERE session_id = ?1 AND role = 'assistant'
         )
         SELECT m.id, m.timestamp, m.role, m.model,
                COALESCE(m.provider, 'claude_code'),
                m.repo_id,
                m.input_tokens, m.output_tokens,
                m.cache_creation_tokens, m.cache_read_tokens,
                COALESCE(m.cost_cents, 0.0),
                COALESCE(m.cost_confidence, 'estimated'),
                m.git_branch,
                m.request_id,
                seq.assistant_sequence
         FROM messages m
         LEFT JOIN assistant_sequence seq ON seq.id = m.id
         {where_clause}
         ORDER BY {order_expr}
         LIMIT ?2 OFFSET ?3"
    );

    let mut stmt = conn.prepare(&sql)?;
    let mut rows: Vec<MessageRow> = stmt
        .query_map(params![session_id, p.limit.min(200), p.offset], |row| {
            Ok(MessageRow {
                id: row.get(0)?,
                session_id: Some(session_id.to_string()),
                timestamp: row.get(1)?,
                role: row.get(2)?,
                model: row.get(3)?,
                provider: row.get(4)?,
                repo_id: row.get(5)?,
                input_tokens: row.get(6)?,
                output_tokens: row.get(7)?,
                cache_creation_tokens: row.get(8)?,
                cache_read_tokens: row.get(9)?,
                cost_cents: row.get(10)?,
                cost_confidence: row.get(11)?,
                git_branch: row.get(12)?,
                request_id: row.get(13)?,
                assistant_sequence: row.get::<_, Option<u64>>(14)?,
                tools: Vec::new(),
                tags: Vec::new(),
            })
        })?
        .filter_map(|r| r.ok())
        .collect();
    enrich_message_rows(conn, &mut rows, true)?;

    Ok(super::PaginatedMessages {
        messages: rows,
        total_count,
    })
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionMessageCurvePoint {
    pub assistant_sequence: u64,
    pub tokens: u64,
    pub cumulative_cost_cents: f64,
}

pub fn session_message_curve(
    conn: &Connection,
    session_id: &str,
) -> Result<Vec<SessionMessageCurvePoint>> {
    let mut stmt = conn.prepare(
        "WITH ordered AS (
             SELECT id,
                    ROW_NUMBER() OVER (ORDER BY timestamp ASC, id ASC) as assistant_sequence,
                    (input_tokens + output_tokens) as tokens,
                    COALESCE(cost_cents, 0.0) as cost_cents
             FROM messages
             WHERE session_id = ?1 AND role = 'assistant'
         )
         SELECT assistant_sequence,
                tokens,
                SUM(cost_cents) OVER (
                    ORDER BY assistant_sequence
                    ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
                ) as cumulative_cost_cents
         FROM ordered
         ORDER BY assistant_sequence ASC",
    )?;

    let rows = stmt
        .query_map(params![session_id], |row| {
            Ok(SessionMessageCurvePoint {
                assistant_sequence: row.get(0)?,
                tokens: row.get(1)?,
                cumulative_cost_cents: row.get(2)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

pub fn session_hook_events(
    conn: &Connection,
    session_id: &str,
    params: &SessionHookEventsParams<'_>,
) -> Result<Vec<SessionHookEventRow>> {
    let mut conditions = vec!["session_id = ?1".to_string()];
    let mut bindings: Vec<String> = vec![session_id.to_string()];
    if params.linked_only {
        conditions.push("message_id IS NOT NULL".to_string());
    }
    if let Some(event) = params.event
        && !event.trim().is_empty()
    {
        bindings.push(event.trim().to_string());
        conditions.push(format!("event = ?{}", bindings.len()));
    }
    let where_clause = format!("WHERE {}", conditions.join(" AND "));
    bindings.push(params.limit.min(500).to_string());
    let limit_idx = bindings.len();
    bindings.push(params.offset.to_string());
    let offset_idx = bindings.len();

    let raw_select = if params.include_raw {
        "raw_json"
    } else {
        "NULL AS raw_json"
    };
    let sql = format!(
        "SELECT id, timestamp, event, provider, session_id,
                message_id, link_confidence, tool_name,
                tool_use_id, tool_duration_ms, mcp_server,
                message_request_id, {raw_select}
         FROM hook_events
         {where_clause}
         ORDER BY timestamp DESC
         LIMIT ?{limit_idx} OFFSET ?{offset_idx}"
    );
    let bind_refs: Vec<&dyn rusqlite::types::ToSql> = bindings
        .iter()
        .map(|v| v as &dyn rusqlite::types::ToSql)
        .collect();

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(bind_refs.as_slice(), |row| {
            Ok(SessionHookEventRow {
                id: row.get(0)?,
                timestamp: row.get(1)?,
                event: row.get(2)?,
                provider: row.get(3)?,
                session_id: row.get(4)?,
                message_id: row.get(5)?,
                link_confidence: row.get(6)?,
                tool_name: row.get(7)?,
                tool_use_id: row.get(8)?,
                tool_duration_ms: row.get(9)?,
                mcp_server: row.get(10)?,
                message_request_id: row.get(11)?,
                raw_json: row.get(12)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

pub fn session_otel_events(
    conn: &Connection,
    session_id: &str,
    params: &SessionOtelEventsParams,
) -> Result<Vec<OtelEventRow>> {
    let mut conditions = vec!["session_id = ?1".to_string()];
    let mut bindings: Vec<String> = vec![session_id.to_string()];
    if params.linked_only {
        conditions.push("message_id IS NOT NULL".to_string());
    }
    let where_clause = format!("WHERE {}", conditions.join(" AND "));
    bindings.push(params.limit.min(500).to_string());
    let limit_idx = bindings.len();
    bindings.push(params.offset.to_string());
    let offset_idx = bindings.len();

    let raw_select = if params.include_raw {
        "raw_json"
    } else {
        "NULL AS raw_json"
    };
    let sql = format!(
        "SELECT id, event_name, timestamp, timestamp_nano, session_id,
                message_id, model, cost_usd_reported, cost_cents_computed,
                processed, {raw_select}
         FROM otel_events
         {where_clause}
         ORDER BY timestamp DESC, id DESC
         LIMIT ?{limit_idx} OFFSET ?{offset_idx}"
    );
    let bind_refs: Vec<&dyn rusqlite::types::ToSql> = bindings
        .iter()
        .map(|v| v as &dyn rusqlite::types::ToSql)
        .collect();

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(bind_refs.as_slice(), |row| {
            let processed: i64 = row.get(9)?;
            Ok(OtelEventRow {
                id: row.get(0)?,
                event_name: row.get(1)?,
                timestamp: row.get(2)?,
                timestamp_nano: row.get(3)?,
                session_id: row.get(4)?,
                message_id: row.get(5)?,
                model: row.get(6)?,
                cost_usd_reported: row.get(7)?,
                cost_cents_computed: row.get(8)?,
                processed: processed != 0,
                raw_json: row.get(10)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

pub fn message_detail(conn: &Connection, message_id: &str) -> Result<Option<MessageDetail>> {
    let message_result = conn.query_row(
        "SELECT id, session_id, timestamp, role, model,
                COALESCE(provider, 'claude_code'),
                repo_id,
                input_tokens, output_tokens,
                cache_creation_tokens, cache_read_tokens,
                COALESCE(cost_cents, 0.0),
                COALESCE(cost_confidence, 'estimated'),
                git_branch,
                request_id
         FROM messages
         WHERE id = ?1",
        params![message_id],
        |row| {
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
                request_id: row.get(14)?,
                assistant_sequence: None,
                tools: Vec::new(),
                tags: Vec::new(),
            })
        },
    );

    let mut message = match message_result {
        Ok(row) => row,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    message.tools = message_tools(conn, message_id)?;
    let tags = message_tags(conn, message_id)?;
    message.tags = tags.clone();

    let hook_events: Vec<SessionHookEventRow> = {
        let mut stmt = conn.prepare(
            "SELECT id, timestamp, event, provider, session_id,
                    message_id, link_confidence, tool_name,
                    tool_use_id, tool_duration_ms, mcp_server,
                    message_request_id, raw_json
             FROM hook_events
             WHERE message_id = ?1
             ORDER BY timestamp ASC, id ASC",
        )?;
        stmt.query_map(params![message_id], |row| {
            Ok(SessionHookEventRow {
                id: row.get(0)?,
                timestamp: row.get(1)?,
                event: row.get(2)?,
                provider: row.get(3)?,
                session_id: row.get(4)?,
                message_id: row.get(5)?,
                link_confidence: row.get(6)?,
                tool_name: row.get(7)?,
                tool_use_id: row.get(8)?,
                tool_duration_ms: row.get(9)?,
                mcp_server: row.get(10)?,
                message_request_id: row.get(11)?,
                raw_json: row.get(12)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect()
    };

    let otel_events: Vec<OtelEventRow> = {
        let mut stmt = conn.prepare(
            "SELECT id, event_name, timestamp, timestamp_nano, session_id,
                    message_id, model, cost_usd_reported, cost_cents_computed,
                    processed, raw_json
             FROM otel_events
             WHERE message_id = ?1
             ORDER BY timestamp ASC, id ASC",
        )?;
        stmt.query_map(params![message_id], |row| {
            let processed: i64 = row.get(9)?;
            Ok(OtelEventRow {
                id: row.get(0)?,
                event_name: row.get(1)?,
                timestamp: row.get(2)?,
                timestamp_nano: row.get(3)?,
                session_id: row.get(4)?,
                message_id: row.get(5)?,
                model: row.get(6)?,
                cost_usd_reported: row.get(7)?,
                cost_cents_computed: row.get(8)?,
                processed: processed != 0,
                raw_json: row.get(10)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect()
    };

    Ok(Some(MessageDetail {
        tools: message.tools.clone(),
        message,
        tags,
        hook_events,
        otel_events,
    }))
}
