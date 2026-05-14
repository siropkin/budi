//! Usage summary, message list, repo / activity / branch breakdowns.

use anyhow::Result;
use rusqlite::Connection;

use super::super::MessageRow;
use super::helpers::*;

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
                COALESCE(SUM(cost_cents_effective), 0.0)
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
            COALESCE(SUM(cost_cents_effective), 0.0)
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
                COALESCE(SUM(cost_cents_effective), 0.0)
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
        "cost" => format!("COALESCE(messages.cost_cents_effective, 0.0) {dir}"),
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
                COALESCE(messages.cost_cents_effective, 0.0),
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
                    COALESCE(SUM(cost_cents_effective), 0.0) as cost
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
                COALESCE(SUM(cost_cents_effective), 0.0) as cost
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
                COALESCE(SUM(cost_cents_effective), 0.0) AS cost
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
                COALESCE(SUM(cost_cents_effective), 0.0) as cost
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
                COALESCE(SUM(cost_cents_effective), 0.0) as cost
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
                COALESCE(SUM(cost_cents_effective), 0.0) as cost
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
                    COALESCE(SUM(cost_cents_effective), 0.0) as cost
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
                    COALESCE(SUM(cost_cents_effective), 0.0) as cost
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
