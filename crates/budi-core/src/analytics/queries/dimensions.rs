//! Statusline / provider / surface / status snapshot / cache / cost-curve /
//! confidence / subagent / filter-options / file breakdowns.

use anyhow::Result;
use rusqlite::Connection;

use super::breakdowns::TICKET_TAG_KEY;
use super::helpers::*;
use super::summary::usage_summary_with_filters;

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
    /// Host environment filter — `vscode`, `cursor`, `jetbrains`, `terminal`,
    /// `unknown`. Comma-separated, same shape as `provider`. Added in #748
    /// after `/analytics/statusline` was missed by the original #702 surface
    /// rollout — the JetBrains widget was reading the global rollup instead
    /// of `surface=jetbrains` rows. Empty filter is unscoped (all surfaces).
    #[serde(default, deserialize_with = "deserialize_surface_filter")]
    pub surface: Vec<String>,
}

fn deserialize_surface_filter<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    let raw: Option<String> = Option::deserialize(deserializer)?;
    let parsed: Vec<String> = raw
        .as_deref()
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    Ok(normalize_surfaces(&parsed))
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
    surfaces: &[String],
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
    if !surfaces.is_empty() {
        let placeholders = vec!["?"; surfaces.len()].join(", ");
        // Mirror the COALESCE/lowercase normalization the rest of the
        // analytics layer uses (see `normalized_surface_expr`) so a row
        // with `surface = NULL` or `''` still matches `?surface=unknown`.
        let expr = normalized_surface_expr("surface");
        conditions.push(format!("{expr} IN ({placeholders})"));
        params.extend(surfaces.iter().cloned());
    }
    let sql = format!(
        "SELECT COALESCE(SUM(cost_cents_effective), 0.0)
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
    let surface_filter: &[String] = &params.surface;

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

    // #748: surface filter mirrors the provider filter. The COALESCE-
    // normalized expression matches `?surface=unknown` against NULL / empty
    // rows, consistent with the rest of the analytics layer.
    let push_surface_in = |sql: &mut String, bindings: &mut Vec<String>| {
        if surface_filter.is_empty() {
            return;
        }
        let placeholders = vec!["?"; surface_filter.len()].join(", ");
        let expr = normalized_surface_expr("surface");
        sql.push_str(&format!(" AND {expr} IN ({placeholders})"));
        bindings.extend(surface_filter.iter().cloned());
    };

    let cost_since = |since: &str| -> f64 {
        assistant_cost_since_from_rollups(conn, since, provider_filter, surface_filter)
            .unwrap_or_else(|| {
                let mut sql = String::from(
                    "SELECT COALESCE(SUM(cost_cents_effective), 0.0) FROM messages \
                     WHERE timestamp >= ? AND role = 'assistant'",
                );
                let mut bindings: Vec<String> = vec![since.to_string()];
                push_provider_in(&mut sql, &mut bindings);
                push_surface_in(&mut sql, &mut bindings);
                let refs: Vec<&dyn rusqlite::types::ToSql> = bindings
                    .iter()
                    .map(|s| s as &dyn rusqlite::types::ToSql)
                    .collect();
                conn.query_row(&sql, refs.as_slice(), |r| r.get::<_, f64>(0))
                    .unwrap_or(0.0)
            })
            / 100.0
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
            "SELECT COALESCE(SUM(cost_cents_effective), 0.0) FROM messages \
             WHERE session_id = ? AND role = 'assistant'",
        );
        let mut bindings: Vec<String> = vec![sid.clone()];
        push_provider_in(&mut sql, &mut bindings);
        push_surface_in(&mut sql, &mut bindings);
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
            "SELECT COALESCE(SUM(cost_cents_effective), 0.0) FROM messages \
             WHERE git_branch = ? AND role = 'assistant'",
        );
        let mut bindings: Vec<String> = vec![branch.clone()];
        if let Some(repo) = params.repo_id.as_deref() {
            sql.push_str(" AND COALESCE(repo_id, '') = ?");
            bindings.push(repo.to_string());
        }
        push_provider_in(&mut sql, &mut bindings);
        push_surface_in(&mut sql, &mut bindings);
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
            "SELECT COALESCE(SUM(cost_cents_effective), 0.0) FROM messages \
             WHERE cwd = ? AND role = 'assistant'",
        );
        let mut bindings: Vec<String> = vec![dir.clone()];
        push_provider_in(&mut sql, &mut bindings);
        push_surface_in(&mut sql, &mut bindings);
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
    // dashboard (ADR-0088 §7 post-#648). #748: also honor surface filter so
    // a JetBrains-scoped statusline doesn't surface a Claude Code CLI as
    // "active" when the JetBrains rollup is empty.
    let active_provider: Option<String> = {
        let mut sql = String::from("SELECT provider FROM messages WHERE timestamp >= ?");
        let mut bindings: Vec<String> = vec![since_1d.to_string()];
        push_provider_in(&mut sql, &mut bindings);
        push_surface_in(&mut sql, &mut bindings);
        sql.push_str(" ORDER BY timestamp DESC LIMIT 1");
        let refs: Vec<&dyn rusqlite::types::ToSql> = bindings
            .iter()
            .map(|s| s as &dyn rusqlite::types::ToSql)
            .collect();
        conn.query_row(&sql, refs.as_slice(), |r| r.get(0)).ok()
    };

    let (health_state, health_tip, session_msg_cost) = normalized_session_id
        .as_ref()
        .and_then(|sid| super::super::health::session_health(conn, Some(sid)).ok())
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
                COALESCE(SUM(CASE WHEN role = 'assistant' THEN cost_cents_effective ELSE 0.0 END), 0.0)
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
                COALESCE(SUM(CASE WHEN role = 'assistant' THEN cost_cents_effective ELSE 0.0 END), 0.0)
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
                COALESCE(SUM(CASE WHEN role = 'assistant' THEN cost_cents_effective ELSE 0.0 END), 0.0)
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
                COALESCE(SUM(CASE WHEN role = 'assistant' THEN cost_cents_effective ELSE 0.0 END), 0.0)
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
                    COALESCE(SUM(cost_cents_effective), 0.0) as total_cost
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
                COALESCE(SUM(cost_cents_effective), 0.0) as cost
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
                COALESCE(SUM(cost_cents_effective), 0.0) as cost,
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
                    m.cost_cents_effective AS cost_cents,
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
                COALESCE(SUM(m2.cost_cents_effective), 0.0) AS cost,
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
                    m.cost_cents_effective AS cost_cents,
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
                COALESCE(SUM(m.cost_cents_effective / mvc.n_values), 0.0) AS cost
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
                COALESCE(SUM(m.cost_cents_effective / mvc.n_values), 0.0) AS cost
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
