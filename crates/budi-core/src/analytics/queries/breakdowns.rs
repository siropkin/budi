//! Tag / ticket / activity-cost / model breakdowns.

use anyhow::Result;
use rusqlite::Connection;

use super::helpers::*;

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
                    COALESCE(SUM(m.cost_cents_effective), 0.0) as total_cost_cents
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
                    COALESCE(SUM(m.cost_cents_effective / mvc.n_values), 0.0) as total_cost_cents
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
                    COALESCE(SUM(m.cost_cents_effective), 0.0) as total_cost_cents
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
                COALESCE(SUM(cost_cents_effective), 0.0) as total_cost_cents
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
                COALESCE(SUM(cost_cents_effective), 0.0) as total_cost_cents
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

pub(super) const TICKET_TAG_KEY: &str = "ticket_id";
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
                    m.cost_cents_effective AS cost_cents,
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
                COALESCE(SUM(m2.cost_cents_effective), 0.0) AS cost,
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
                    m.cost_cents_effective AS cost_cents,
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
                COALESCE(SUM(m.cost_cents_effective / mvc.n_values), 0.0) AS cost
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
                    m.cost_cents_effective AS cost_cents,
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
                COALESCE(SUM(m2.cost_cents_effective), 0.0) AS cost,
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
                COALESCE(SUM(m.cost_cents_effective / mvc.n_values), 0.0) AS cost,
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
                COALESCE(SUM(m.cost_cents_effective / mvc.n_values), 0.0) AS cost
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
                COALESCE(SUM(cost_cents_effective), 0.0)
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
                COALESCE(SUM(cost_cents_effective), 0.0)
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
