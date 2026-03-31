//! Session-specific queries: listing, tags, messages, and audit diagnostics.

use anyhow::Result;
use rusqlite::{Connection, params};

use super::MessageRow;

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
         WHERE NOT EXISTS (SELECT 1 FROM messages m WHERE m.session_id = s.session_id)",
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
    pub session_id: String,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub duration_ms: Option<i64>,
    pub message_count: u64,
    pub cost_cents: f64,
    pub model: Option<String>,
    pub provider: String,
    pub repo_id: Option<String>,
    pub git_branch: Option<String>,
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

/// Query sessions with cost aggregated from messages.
pub fn session_list(conn: &Connection, p: &SessionListParams) -> Result<PaginatedSessions> {
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
             LEFT JOIN sessions s ON s.session_id = m.session_id
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
        "model" => format!("sa.models_by_cost {dir}"),
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
                     ) sub) as models_by_cost,
                    COALESCE(MAX(m.provider), 'claude_code') as provider,
                    COALESCE(MAX(m.repo_id), MAX(s.repo_id)) as repo_id,
                    COALESCE(MAX(m.git_branch), MAX(s.git_branch)) as git_branch,
                    COALESCE(SUM(m.input_tokens), 0) as inp,
                    COALESCE(SUM(m.output_tokens), 0) as outp,
                    COALESCE(s.duration_ms,
                        CAST((julianday(MAX(m.timestamp)) - julianday(MIN(m.timestamp))) * 86400000 AS INTEGER)
                    ) as duration_ms,
                    s.title
             FROM messages m
             LEFT JOIN sessions s ON s.session_id = m.session_id
             {where_clause}
             AND m.session_id IS NOT NULL
             GROUP BY m.session_id
         )
         SELECT COUNT(*) OVER() as total,
                sa.session_id, sa.started_at, sa.ended_at, sa.duration_ms,
                sa.msg_count, sa.cost, sa.models_by_cost, sa.provider, sa.repo_id, sa.git_branch,
                sa.inp, sa.outp, sa.title
         FROM session_agg sa
         ORDER BY {order_expr}
         LIMIT ?{limit_idx} OFFSET ?{offset_idx}",
    );

    let mut stmt = conn.prepare(&sql)?;
    let sessions: Vec<SessionListEntry> = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(SessionListEntry {
                session_id: row.get(1)?,
                started_at: row.get(2)?,
                ended_at: row.get(3)?,
                duration_ms: row.get(4)?,
                message_count: row.get(5)?,
                cost_cents: row.get(6)?,
                model: row.get(7)?,
                provider: row.get::<_, String>(8)?,
                repo_id: row.get(9)?,
                git_branch: row.get(10)?,
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

// ---------------------------------------------------------------------------
// Session Tags & Messages
// ---------------------------------------------------------------------------

/// Get distinct tags for a session.
pub fn session_tags(conn: &Connection, session_id: &str) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT t.key, t.value
         FROM tags t
         JOIN messages m ON t.message_uuid = m.uuid
         WHERE m.session_id = ?1
         ORDER BY t.key, t.value",
    )?;
    let rows = stmt
        .query_map(params![session_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

/// Messages within a specific session for drill-down.
pub fn session_messages(conn: &Connection, session_id: &str) -> Result<Vec<MessageRow>> {
    let mut stmt = conn.prepare(
        "SELECT uuid, timestamp, role, model,
                COALESCE(provider, 'claude_code'),
                repo_id,
                input_tokens, output_tokens,
                cache_creation_tokens, cache_read_tokens,
                COALESCE(cost_cents, 0.0),
                COALESCE(cost_confidence, 'estimated'),
                git_branch
         FROM messages
         WHERE session_id = ?1 AND role = 'assistant'
         ORDER BY timestamp ASC",
    )?;

    let rows = stmt
        .query_map(params![session_id], |row| {
            Ok(MessageRow {
                uuid: row.get(0)?,
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
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(rows)
}
