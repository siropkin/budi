use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use budi_core::{analytics, cost};
use serde_json::json;

use super::{bad_request, internal_error, not_found};
use crate::AppState;

struct BusyFlagGuard {
    flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl BusyFlagGuard {
    fn new(flag: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
        Self { flag }
    }
}

impl Drop for BusyFlagGuard {
    fn drop(&mut self) {
        self.flag.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

#[derive(serde::Deserialize)]
pub struct DimensionParams {
    #[serde(alias = "agent", alias = "providers")]
    pub agents: Option<String>,
    #[serde(alias = "model")]
    pub models: Option<String>,
    #[serde(alias = "project", alias = "repo", alias = "repo_id")]
    pub projects: Option<String>,
    #[serde(alias = "branch")]
    pub branches: Option<String>,
}

fn parse_filter_values(value: Option<&str>) -> Vec<String> {
    value
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

fn parse_dimension_filters(params: &DimensionParams) -> analytics::DimensionFilters {
    analytics::DimensionFilters {
        agents: parse_filter_values(params.agents.as_deref()),
        models: parse_filter_values(params.models.as_deref()),
        projects: parse_filter_values(params.projects.as_deref()),
        branches: parse_filter_values(params.branches.as_deref()),
    }
    .normalize()
}

#[derive(serde::Deserialize)]
pub struct DateRangeParams {
    pub since: Option<String>,
    pub until: Option<String>,
    #[serde(flatten)]
    pub filters: DimensionParams,
}

#[derive(serde::Deserialize)]
pub struct BranchDetailParams {
    pub since: Option<String>,
    pub until: Option<String>,
    pub repo_id: Option<String>,
}

#[derive(serde::Deserialize)]
pub struct SummaryParams {
    pub since: Option<String>,
    pub until: Option<String>,
    pub provider: Option<String>,
    #[serde(flatten)]
    pub filters: DimensionParams,
}

pub async fn analytics_summary(
    Query(params): Query<SummaryParams>,
) -> Result<Json<analytics::UsageSummary>, (StatusCode, Json<serde_json::Value>)> {
    let filters = parse_dimension_filters(&params.filters);
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::usage_summary_with_filters(
            &conn,
            params.since.as_deref(),
            params.until.as_deref(),
            params.provider.as_deref(),
            &filters,
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    Ok(Json(result))
}

#[derive(serde::Deserialize)]
pub struct MessagesParams {
    pub since: Option<String>,
    pub until: Option<String>,
    pub search: Option<String>,
    pub sort_by: Option<String>,
    pub sort_asc: Option<bool>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

const VALID_MESSAGE_SORT_BY: &[&str] = &[
    "timestamp",
    "cost",
    "model",
    "tokens",
    "provider",
    "branch",
    "git_branch",
    "ticket",
    "repo_id",
];

const VALID_SESSION_SORT_BY: &[&str] = &[
    "started_at",
    "title",
    "duration",
    "cost",
    "model",
    "tokens",
    "provider",
    "repo_id",
    "branch",
    "git_branch",
];

const VALID_ACTIVITY_GRANULARITY: &[&str] = &["hour", "day", "week", "month"];

pub async fn analytics_messages(
    Query(params): Query<MessagesParams>,
) -> Result<Json<analytics::PaginatedMessages>, (StatusCode, Json<serde_json::Value>)> {
    if let Some(ref sort) = params.sort_by
        && !VALID_MESSAGE_SORT_BY.contains(&sort.as_str())
    {
        return Err(bad_request(format!(
            "invalid sort_by '{}'; valid values: {}",
            sort,
            VALID_MESSAGE_SORT_BY.join(", ")
        )));
    }
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::message_list(
            &conn,
            &analytics::MessageListParams {
                since: params.since.as_deref(),
                until: params.until.as_deref(),
                search: params.search.as_deref(),
                sort_by: params.sort_by.as_deref(),
                sort_asc: params.sort_asc.unwrap_or(false),
                limit: params.limit.unwrap_or(50).min(200),
                offset: params.offset.unwrap_or(0),
            },
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    Ok(Json(result))
}

#[derive(serde::Deserialize)]
pub struct ListParams {
    pub since: Option<String>,
    pub until: Option<String>,
    pub limit: Option<usize>,
    #[serde(flatten)]
    pub filters: DimensionParams,
}

/// Resolve the CLI-requested `--limit N` (0 = unlimited) to a capped value
/// for pagination. `None` falls back to the default breakdown cap of 30.
fn resolve_breakdown_limit(requested: Option<usize>) -> usize {
    let raw = requested.unwrap_or(30);
    if raw == 0 { 0 } else { raw.min(100_000) }
}

pub async fn analytics_projects(
    Query(params): Query<ListParams>,
) -> Result<
    Json<analytics::BreakdownPage<analytics::RepoUsage>>,
    (StatusCode, Json<serde_json::Value>),
> {
    let limit = resolve_breakdown_limit(params.limit);
    let filters = parse_dimension_filters(&params.filters);
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::repo_usage_with_filters(
            &conn,
            params.since.as_deref(),
            params.until.as_deref(),
            &filters,
            analytics::BREAKDOWN_FETCH_ALL_LIMIT,
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    Ok(Json(analytics::paginate_breakdown(result, limit)))
}

/// #442 `--include-non-repo`: per-cwd-basename breakdown of rows whose
/// `repo_id` is NULL. Returned as a flat `Vec<RepoUsage>` (no paginated
/// `(other)` bucket) because the expected cardinality is small — any
/// single user's non-repo scratch-dir set tops out in the low dozens.
pub async fn analytics_non_repo(
    Query(params): Query<ListParams>,
) -> Result<Json<Vec<analytics::RepoUsage>>, (StatusCode, Json<serde_json::Value>)> {
    let limit = resolve_breakdown_limit(params.limit);
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::non_repo_usage(
            &conn,
            params.since.as_deref(),
            params.until.as_deref(),
            limit,
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    Ok(Json(result))
}

pub async fn analytics_models(
    Query(params): Query<ListParams>,
) -> Result<
    Json<analytics::BreakdownPage<analytics::ModelUsage>>,
    (StatusCode, Json<serde_json::Value>),
> {
    let limit = resolve_breakdown_limit(params.limit);
    let filters = parse_dimension_filters(&params.filters);
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::model_usage_with_filters(
            &conn,
            params.since.as_deref(),
            params.until.as_deref(),
            &filters,
            analytics::BREAKDOWN_FETCH_ALL_LIMIT,
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    Ok(Json(analytics::paginate_breakdown(result, limit)))
}

pub async fn analytics_branches(
    Query(params): Query<ListParams>,
) -> Result<
    Json<analytics::BreakdownPage<analytics::BranchCost>>,
    (StatusCode, Json<serde_json::Value>),
> {
    let limit = resolve_breakdown_limit(params.limit);
    let filters = parse_dimension_filters(&params.filters);
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::branch_cost_with_filters(
            &conn,
            params.since.as_deref(),
            params.until.as_deref(),
            &filters,
            analytics::BREAKDOWN_FETCH_ALL_LIMIT,
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    Ok(Json(analytics::paginate_breakdown(result, limit)))
}

pub async fn analytics_cost(
    Query(params): Query<SummaryParams>,
) -> Result<Json<cost::CostEstimate>, (StatusCode, Json<serde_json::Value>)> {
    let filters = parse_dimension_filters(&params.filters);
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        cost::estimate_cost_with_filters(
            &conn,
            params.since.as_deref(),
            params.until.as_deref(),
            params.provider.as_deref(),
            &filters,
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    Ok(Json(result))
}

#[derive(serde::Deserialize)]
pub struct ActivityChartParams {
    pub since: Option<String>,
    pub until: Option<String>,
    pub granularity: Option<String>,
    pub tz_offset: Option<i32>,
    #[serde(flatten)]
    pub filters: DimensionParams,
}

pub async fn analytics_activity(
    Query(params): Query<ActivityChartParams>,
) -> Result<Json<Vec<analytics::ActivityBucket>>, (StatusCode, Json<serde_json::Value>)> {
    let granularity = params.granularity.unwrap_or_else(|| "day".to_string());
    if !VALID_ACTIVITY_GRANULARITY.contains(&granularity.as_str()) {
        return Err(bad_request(format!(
            "invalid granularity '{}'; valid values: {}",
            granularity,
            VALID_ACTIVITY_GRANULARITY.join(", ")
        )));
    }
    let tz_offset = params.tz_offset.unwrap_or(0);
    let filters = parse_dimension_filters(&params.filters);
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::activity_chart_with_filters(
            &conn,
            params.since.as_deref(),
            params.until.as_deref(),
            &filters,
            &granularity,
            tz_offset,
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;
    Ok(Json(result))
}

pub async fn analytics_providers(
    Query(params): Query<DateRangeParams>,
) -> Result<Json<Vec<analytics::ProviderStats>>, (StatusCode, Json<serde_json::Value>)> {
    let filters = parse_dimension_filters(&params.filters);
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::provider_stats_with_filters(
            &conn,
            params.since.as_deref(),
            params.until.as_deref(),
            &filters,
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    Ok(Json(result))
}

#[derive(serde::Serialize)]
pub(crate) struct ProviderInfo {
    name: String,
    display_name: String,
}

#[derive(serde::Serialize)]
pub struct SchemaVersionResponse {
    pub current: u32,
    pub target: u32,
    pub exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub needs_migration: Option<bool>,
}

#[derive(serde::Serialize)]
pub struct RepairResponse {
    pub from_version: u32,
    pub to_version: u32,
    pub migrated: bool,
    pub repaired: bool,
    pub added_columns: Vec<String>,
    pub added_indexes: Vec<String>,
    pub removed_tables: Vec<String>,
}

#[derive(serde::Serialize)]
pub struct IntegrationsResponse {
    pub cursor_extension: bool,
    pub statusline: bool,
    pub database: DatabaseStats,
    pub paths: IntegrationPaths,
}

#[derive(serde::Serialize)]
pub struct DatabaseStats {
    pub size_mb: f64,
    pub records: i64,
    pub first_record: Option<String>,
}

#[derive(serde::Serialize)]
pub struct IntegrationPaths {
    pub database: String,
    pub config: String,
    pub claude_settings: String,
}

#[derive(serde::Serialize)]
pub struct CheckUpdateResponse {
    pub current: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub up_to_date: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub async fn analytics_registered_providers()
-> Result<Json<Vec<ProviderInfo>>, (StatusCode, Json<serde_json::Value>)> {
    let providers = budi_core::provider::all_providers();
    let list: Vec<ProviderInfo> = providers
        .iter()
        .map(|p| ProviderInfo {
            name: p.name().to_string(),
            display_name: p.display_name().to_string(),
        })
        .collect();
    Ok(Json(list))
}

pub async fn analytics_statusline(
    Query(params): Query<analytics::StatuslineParams>,
) -> Result<Json<analytics::StatuslineStats>, (StatusCode, Json<serde_json::Value>)> {
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        // Primary windows are rolling 1d / 7d / 30d from `now`, per ADR-0088
        // §4 and issue #224. Calendar today/week/month have been retired
        // from this endpoint to align with the shared provider-scoped
        // status contract.
        let now = chrono::Utc::now();
        let since_1d = (now - chrono::Duration::days(1)).to_rfc3339();
        let since_7d = (now - chrono::Duration::days(7)).to_rfc3339();
        let since_30d = (now - chrono::Duration::days(30)).to_rfc3339();
        analytics::statusline_stats(&conn, &since_1d, &since_7d, &since_30d, &params)
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    Ok(Json(result))
}

#[derive(serde::Deserialize)]
pub struct TagParams {
    pub since: Option<String>,
    pub until: Option<String>,
    pub key: Option<String>,
    pub limit: Option<usize>,
    #[serde(flatten)]
    pub filters: DimensionParams,
}

pub async fn analytics_tags(
    Query(params): Query<TagParams>,
) -> Result<Json<analytics::BreakdownPage<analytics::TagCost>>, (StatusCode, Json<serde_json::Value>)>
{
    let limit = resolve_breakdown_limit(params.limit);
    let filters = parse_dimension_filters(&params.filters);
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::tag_stats_with_filters(
            &conn,
            params.key.as_deref(),
            params.since.as_deref(),
            params.until.as_deref(),
            &filters,
            analytics::BREAKDOWN_FETCH_ALL_LIMIT,
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;
    Ok(Json(analytics::paginate_breakdown(result, limit)))
}

pub async fn analytics_branch_detail(
    Path(branch): Path<String>,
    Query(params): Query<BranchDetailParams>,
) -> Result<Json<analytics::BranchCost>, (StatusCode, Json<serde_json::Value>)> {
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::branch_cost_single(
            &conn,
            &branch,
            params.repo_id.as_deref(),
            params.since.as_deref(),
            params.until.as_deref(),
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    match result {
        Some(detail) => Ok(Json(detail)),
        None => Err(not_found("branch not found")),
    }
}

// ---------------------------------------------------------------------------
// Tickets — first-class CLI dimension wired in 8.1 (#304)
// ---------------------------------------------------------------------------

/// `GET /analytics/tickets` query params. Mirrors `ListParams` so the same
/// `--provider`/`--model`/`--repo` slicing offered by `--branches` is also
/// available for `--tickets`.
#[derive(serde::Deserialize)]
pub struct TicketListParams {
    pub since: Option<String>,
    pub until: Option<String>,
    pub limit: Option<usize>,
    #[serde(flatten)]
    pub filters: DimensionParams,
}

#[derive(serde::Deserialize)]
pub struct TicketDetailParams {
    pub since: Option<String>,
    pub until: Option<String>,
    pub repo_id: Option<String>,
}

pub async fn analytics_tickets(
    Query(params): Query<TicketListParams>,
) -> Result<
    Json<analytics::BreakdownPage<analytics::TicketCost>>,
    (StatusCode, Json<serde_json::Value>),
> {
    let limit = resolve_breakdown_limit(params.limit);
    let filters = parse_dimension_filters(&params.filters);
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::ticket_cost_with_filters(
            &conn,
            params.since.as_deref(),
            params.until.as_deref(),
            &filters,
            analytics::BREAKDOWN_FETCH_ALL_LIMIT,
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    Ok(Json(analytics::paginate_breakdown(result, limit)))
}

pub async fn analytics_ticket_detail(
    Path(ticket_id): Path<String>,
    Query(params): Query<TicketDetailParams>,
) -> Result<Json<analytics::TicketCostDetail>, (StatusCode, Json<serde_json::Value>)> {
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::ticket_cost_single(
            &conn,
            &ticket_id,
            params.repo_id.as_deref(),
            params.since.as_deref(),
            params.until.as_deref(),
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    match result {
        Some(detail) => Ok(Json(detail)),
        None => Err(not_found("ticket not found")),
    }
}

// ---------------------------------------------------------------------------
// Activities — first-class CLI dimension wired in 8.1 (#305)
//
// Same shape as the ticket endpoints so the CLI can mirror `--tickets` /
// `--ticket` with `--activities` / `--activity` and operators don't need
// to learn a second query surface. Activities come from the `activity`
// tag emitted by the prompt classifier (`hooks::classify_prompt`).
// ---------------------------------------------------------------------------

/// `GET /analytics/activities` query params. Mirrors `TicketListParams`.
#[derive(serde::Deserialize)]
pub struct ActivityListParams {
    pub since: Option<String>,
    pub until: Option<String>,
    pub limit: Option<usize>,
    #[serde(flatten)]
    pub filters: DimensionParams,
}

#[derive(serde::Deserialize)]
pub struct ActivityDetailParams {
    pub since: Option<String>,
    pub until: Option<String>,
    pub repo_id: Option<String>,
}

pub async fn analytics_activities(
    Query(params): Query<ActivityListParams>,
) -> Result<
    Json<analytics::BreakdownPage<analytics::ActivityCost>>,
    (StatusCode, Json<serde_json::Value>),
> {
    let limit = resolve_breakdown_limit(params.limit);
    let filters = parse_dimension_filters(&params.filters);
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::activity_cost_with_filters(
            &conn,
            params.since.as_deref(),
            params.until.as_deref(),
            &filters,
            analytics::BREAKDOWN_FETCH_ALL_LIMIT,
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    Ok(Json(analytics::paginate_breakdown(result, limit)))
}

pub async fn analytics_activity_detail(
    Path(activity): Path<String>,
    Query(params): Query<ActivityDetailParams>,
) -> Result<Json<analytics::ActivityCostDetail>, (StatusCode, Json<serde_json::Value>)> {
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::activity_cost_single(
            &conn,
            &activity,
            params.repo_id.as_deref(),
            params.since.as_deref(),
            params.until.as_deref(),
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    match result {
        Some(detail) => Ok(Json(detail)),
        None => Err(not_found("activity not found")),
    }
}

// ---------------------------------------------------------------------------
// Files — per-file attribution wired in 8.1 R1.4 (#292)
//
// Mirrors the ticket/activity endpoints so the CLI exposes one consistent
// shape: `--files` lists top files, `--file <PATH>` shows a single file's
// detail with per-branch and per-ticket breakdowns.
//
// The path segment is URL-encoded by callers because repo-relative paths
// routinely contain slashes. We validate it in the handler to avoid
// surprises in paths that include path traversal tokens.
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
pub struct FileListParams {
    pub since: Option<String>,
    pub until: Option<String>,
    pub limit: Option<usize>,
    #[serde(flatten)]
    pub filters: DimensionParams,
}

#[derive(serde::Deserialize)]
pub struct FileDetailParams {
    pub since: Option<String>,
    pub until: Option<String>,
    pub repo_id: Option<String>,
}

pub async fn analytics_files(
    Query(params): Query<FileListParams>,
) -> Result<
    Json<analytics::BreakdownPage<analytics::FileCost>>,
    (StatusCode, Json<serde_json::Value>),
> {
    let limit = resolve_breakdown_limit(params.limit);
    let filters = parse_dimension_filters(&params.filters);
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::file_cost_with_filters(
            &conn,
            params.since.as_deref(),
            params.until.as_deref(),
            &filters,
            analytics::BREAKDOWN_FETCH_ALL_LIMIT,
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    Ok(Json(analytics::paginate_breakdown(result, limit)))
}

pub async fn analytics_file_detail(
    Path(file_path): Path<String>,
    Query(params): Query<FileDetailParams>,
) -> Result<Json<analytics::FileCostDetail>, (StatusCode, Json<serde_json::Value>)> {
    // Reject absolute paths and traversal tokens early. `FileEnricher`
    // never stores such values, so clients asking for them can't match a
    // row anyway; returning 400 is clearer than a silent 404.
    if file_path.starts_with('/')
        || file_path.contains("..")
        || file_path.contains('\\')
        || file_path.contains("://")
    {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "file path must be repo-relative, forward-slashed, and inside the repo root"
            })),
        ));
    }

    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::file_cost_single(
            &conn,
            &file_path,
            params.repo_id.as_deref(),
            params.since.as_deref(),
            params.until.as_deref(),
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    match result {
        Some(detail) => Ok(Json(detail)),
        None => Err(not_found("file not found")),
    }
}

pub async fn analytics_schema_version()
-> Result<Json<SchemaVersionResponse>, (StatusCode, Json<serde_json::Value>)> {
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<SchemaVersionResponse> {
        let db_path = analytics::db_path()?;
        if !db_path.exists() {
            return Ok(SchemaVersionResponse {
                current: 0,
                target: budi_core::migration::SCHEMA_VERSION,
                exists: false,
                needs_migration: None,
            });
        }
        let conn = analytics::open_db(&db_path)?;
        let current = budi_core::migration::current_version(&conn);
        let target = budi_core::migration::SCHEMA_VERSION;
        Ok(SchemaVersionResponse {
            current,
            target,
            exists: true,
            needs_migration: Some(budi_core::migration::needs_migration(&conn)),
        })
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;
    Ok(Json(result))
}

pub async fn analytics_cache_efficiency(
    Query(params): Query<DateRangeParams>,
) -> Result<Json<analytics::CacheEfficiency>, (StatusCode, Json<serde_json::Value>)> {
    let filters = parse_dimension_filters(&params.filters);
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::cache_efficiency_with_filters(
            &conn,
            params.since.as_deref(),
            params.until.as_deref(),
            &filters,
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;
    Ok(Json(result))
}

pub async fn analytics_session_cost_curve(
    Query(params): Query<DateRangeParams>,
) -> Result<Json<Vec<analytics::SessionCostBucket>>, (StatusCode, Json<serde_json::Value>)> {
    let filters = parse_dimension_filters(&params.filters);
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::session_cost_curve_with_filters(
            &conn,
            params.since.as_deref(),
            params.until.as_deref(),
            &filters,
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;
    Ok(Json(result))
}

pub async fn analytics_cost_confidence(
    Query(params): Query<DateRangeParams>,
) -> Result<Json<Vec<analytics::CostConfidenceStat>>, (StatusCode, Json<serde_json::Value>)> {
    let filters = parse_dimension_filters(&params.filters);
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::cost_confidence_stats_with_filters(
            &conn,
            params.since.as_deref(),
            params.until.as_deref(),
            &filters,
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;
    Ok(Json(result))
}

pub async fn analytics_subagent_cost(
    Query(params): Query<DateRangeParams>,
) -> Result<Json<Vec<analytics::SubagentCostStat>>, (StatusCode, Json<serde_json::Value>)> {
    let filters = parse_dimension_filters(&params.filters);
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::subagent_cost_stats_with_filters(
            &conn,
            params.since.as_deref(),
            params.until.as_deref(),
            &filters,
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;
    Ok(Json(result))
}

#[derive(serde::Deserialize)]
pub struct FilterOptionsParams {
    pub limit: Option<usize>,
}

pub async fn analytics_filter_options(
    Query(params): Query<FilterOptionsParams>,
) -> Result<Json<analytics::FilterOptions>, (StatusCode, Json<serde_json::Value>)> {
    let limit = params.limit.map(|value| value.min(5000));
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::filter_options(&conn, None, None, limit)
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;
    Ok(Json(result))
}

#[derive(serde::Deserialize)]
pub struct SessionsQueryParams {
    pub since: Option<String>,
    pub until: Option<String>,
    pub search: Option<String>,
    pub sort_by: Option<String>,
    pub sort_asc: Option<bool>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    /// Filter to sessions tagged with the given `ticket_id` (e.g. `ENG-123`).
    /// Wired in by 8.1 so `budi sessions --ticket <ID>` mirrors `--branch`.
    pub ticket: Option<String>,
    /// Filter to sessions tagged with the given `activity` (e.g. `bugfix`).
    /// Wired in by 8.1 (#305) so `budi sessions --activity bugfix` mirrors
    /// `--ticket`.
    pub activity: Option<String>,
    #[serde(flatten)]
    pub filters: DimensionParams,
}

pub async fn analytics_sessions(
    Query(params): Query<SessionsQueryParams>,
) -> Result<Json<analytics::PaginatedSessions>, (StatusCode, Json<serde_json::Value>)> {
    if let Some(ref sort) = params.sort_by
        && !VALID_SESSION_SORT_BY.contains(&sort.as_str())
    {
        return Err(bad_request(format!(
            "invalid sort_by '{}'; valid values: {}",
            sort,
            VALID_SESSION_SORT_BY.join(", ")
        )));
    }
    let filters = parse_dimension_filters(&params.filters);
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        let mut paginated = analytics::session_list_with_filters(
            &conn,
            &analytics::SessionListParams {
                since: params.since.as_deref(),
                until: params.until.as_deref(),
                search: params.search.as_deref(),
                sort_by: params.sort_by.as_deref(),
                sort_asc: params.sort_asc.unwrap_or(false),
                limit: params.limit.unwrap_or(50).min(200),
                offset: params.offset.unwrap_or(0),
                ticket: params.ticket.as_deref(),
                activity: params.activity.as_deref(),
            },
            &filters,
        )?;

        let sids: Vec<&str> = paginated.sessions.iter().map(|s| s.id.as_str()).collect();
        if let Ok(health_map) = analytics::session_health_batch(&conn, &sids) {
            for session in &mut paginated.sessions {
                session.health_state = health_map.get(&session.id).cloned();
            }
        }

        Ok::<_, anyhow::Error>(paginated)
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;
    Ok(Json(result))
}

/// Query params for `GET /analytics/sessions/resolve` (#603). The CLI
/// passes its process cwd so the daemon can encode it to Claude
/// Code's `~/.claude/projects/<encoded>/` form and walk for the
/// most-recent transcript.
#[derive(serde::Deserialize)]
pub struct ResolveSessionParams {
    pub token: String,
    pub cwd: Option<String>,
}

/// `GET /analytics/sessions/resolve?token=<token>&cwd=<path>` —
/// server-side resolution for the `current` and `latest` literal
/// session tokens.
///
/// Response shape:
/// ```json
/// {
///   "session_id": "<uuid>",
///   "source": "current" | "latest",
///   "fallback_reason": null | "<one-line stderr-friendly note>"
/// }
/// ```
///
/// - `token=current` walks `~/.claude/projects/<encoded-cwd>/` for
///   the newest `*.jsonl` and returns its filename stem. If that
///   directory is missing or empty, falls back to `latest` and sets
///   `fallback_reason`. Per #603 the CLI surfaces that string on
///   stderr so non-Claude users still get something useful.
/// - `token=latest` returns the newest session id from the DB.
/// - Any other token → 400 Bad Request.
/// - Empty workspace (no sessions at all anywhere) → 404 Not Found.
pub async fn analytics_resolve_session(
    Query(params): Query<ResolveSessionParams>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let token = params.token.trim().to_lowercase();
    if token != "current" && token != "latest" {
        return Err(bad_request(format!(
            "unknown session token '{}'; expected 'current' or 'latest'",
            params.token
        )));
    }

    let mut fallback_reason: Option<String> = None;
    let mut source = token.clone();

    if token == "current" {
        let home = budi_core::config::home_dir().map_err(internal_error)?;
        let cwd_str = params.cwd.unwrap_or_default();
        if cwd_str.is_empty() {
            fallback_reason = Some("no cwd provided — falling back to latest session".to_string());
            source = "latest".to_string();
        } else {
            let cwd = std::path::PathBuf::from(&cwd_str);
            if let Some(sid) = budi_core::session_resolve::find_current_session_id(&home, &cwd) {
                return Ok(Json(json!({
                    "session_id": sid,
                    "source": "current",
                    "fallback_reason": serde_json::Value::Null,
                })));
            }
            fallback_reason = Some(format!(
                "no Claude Code transcripts under ~/.claude/projects/ for cwd {cwd_str} — falling back to latest session",
            ));
            source = "latest".to_string();
        }
    }

    // Either the caller asked for `latest`, or the `current` lookup
    // came up dry and we fell back. Hit the DB for the newest session.
    let latest = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        let paginated = analytics::session_list(
            &conn,
            &analytics::SessionListParams {
                since: None,
                until: None,
                search: None,
                sort_by: Some("started_at"),
                sort_asc: false,
                limit: 1,
                offset: 0,
                ticket: None,
                activity: None,
            },
        )?;
        Ok::<_, anyhow::Error>(paginated.sessions.into_iter().next().map(|s| s.id))
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    let Some(sid) = latest else {
        return Err(not_found("no sessions found"));
    };

    Ok(Json(json!({
        "session_id": sid,
        "source": source,
        "fallback_reason": fallback_reason,
    })))
}

/// Resolve a session ID prefix to its full ID, returning appropriate HTTP errors.
///
/// Error mapping (#519):
/// - Ambiguous prefix → **400 Bad Request** with the daemon's
///   "ambiguous session prefix '<X>'" message surfaced verbatim. The
///   "use more characters" text is actionable operator input-shape
///   guidance, not internal state — safe (and more useful) to expose.
/// - No match → 404 Not Found (`session '<X>' not found`).
/// - Everything else (DB open failure, task panic, unexpected error
///   kind) → 500 Internal Server Error via the generic `internal_error`
///   wrapper.
///
/// Pre-8.3.2 the ambiguous path swallowed the message into a 500
/// `internal server error`, which read as a server fault instead of
/// a "try again with more characters" nudge. Observed during the
/// 8.3.1 post-tag smoke when `budi vitals --session 6` surfaced the
/// generic 500.
async fn resolve_sid(prefix: String) -> Result<String, (StatusCode, Json<serde_json::Value>)> {
    let pfx = prefix.clone();
    let spawn_outcome = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::resolve_session_id(&conn, &pfx)
    })
    .await;
    let resolved = match spawn_outcome {
        Ok(result) => match result {
            Ok(ok) => ok,
            Err(e) => {
                // String-match on the anyhow chain rather than widening
                // `resolve_session_id`'s return-type contract — the
                // ambiguous-prefix anyhow is the only error variant the
                // function produces today (see
                // `crates/budi-core/src/analytics/sessions.rs:619`).
                // Widening to a typed enum is tracked by #519 as a
                // nicer long-term shape; this string-match is the
                // minimal fix that unblocks the CLI render.
                let chain = format!("{e:#}");
                if chain.contains("ambiguous session prefix") {
                    return Err(bad_request(chain));
                }
                return Err(internal_error(e));
            }
        },
        Err(join_err) => return Err(internal_error(anyhow::anyhow!("{join_err}"))),
    };
    match resolved {
        Some(full_id) => Ok(full_id),
        None => Err(not_found(format!("session '{prefix}' not found"))),
    }
}

pub async fn analytics_session_detail(
    Path(session_id): Path<String>,
) -> Result<Json<analytics::SessionListEntry>, (StatusCode, Json<serde_json::Value>)> {
    let sid = resolve_sid(session_id.clone()).await?;
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::session_detail(&conn, &sid)
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    match result {
        Some(session) => Ok(Json(session)),
        None => Err(not_found(format!("session '{session_id}' not found"))),
    }
}

pub async fn analytics_session_tags(
    Path(session_id): Path<String>,
) -> Result<Json<Vec<analytics::SessionTag>>, (StatusCode, Json<serde_json::Value>)> {
    let sid = resolve_sid(session_id).await?;
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        let tags = analytics::session_tags(&conn, &sid)?;
        Ok::<_, anyhow::Error>(
            tags.into_iter()
                .map(|(k, v)| analytics::SessionTag { key: k, value: v })
                .collect::<Vec<_>>(),
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;
    Ok(Json(result))
}

pub async fn analytics_session_messages(
    Path(session_id): Path<String>,
    Query(params): Query<SessionMessagesQueryParams>,
) -> Result<Json<analytics::PaginatedMessages>, (StatusCode, Json<serde_json::Value>)> {
    let sid = resolve_sid(session_id).await?;
    let roles = match params.roles.as_deref() {
        None => analytics::SessionMessageRoles::Assistant,
        Some(raw) => raw
            .parse::<analytics::SessionMessageRoles>()
            .map_err(bad_request)?,
    };
    if let Some(ref sort) = params.sort_by
        && !VALID_MESSAGE_SORT_BY.contains(&sort.as_str())
    {
        return Err(bad_request(format!(
            "invalid sort_by '{}'; valid values: {}",
            sort,
            VALID_MESSAGE_SORT_BY.join(", ")
        )));
    }
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::session_message_list(
            &conn,
            &sid,
            &analytics::SessionMessageListParams {
                roles,
                sort_by: params.sort_by.as_deref(),
                sort_asc: params.sort_asc.unwrap_or(false),
                limit: params.limit.unwrap_or(50).min(200),
                offset: params.offset.unwrap_or(0),
            },
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;
    Ok(Json(result))
}

pub async fn analytics_session_message_curve(
    Path(session_id): Path<String>,
) -> Result<Json<Vec<analytics::SessionMessageCurvePoint>>, (StatusCode, Json<serde_json::Value>)> {
    let sid = resolve_sid(session_id).await?;
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::session_message_curve(&conn, &sid)
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;
    Ok(Json(result))
}

#[derive(serde::Deserialize)]
pub struct SessionMessagesQueryParams {
    pub roles: Option<String>,
    pub sort_by: Option<String>,
    pub sort_asc: Option<bool>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

pub async fn analytics_message_detail(
    Path(message_uuid): Path<String>,
) -> Result<Json<analytics::MessageDetail>, (StatusCode, Json<serde_json::Value>)> {
    let lookup_uuid = message_uuid.clone();
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::message_detail(&conn, &lookup_uuid)
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;
    match result {
        Some(detail) => Ok(Json(detail)),
        None => Err(not_found(format!("message '{message_uuid}' not found"))),
    }
}

#[derive(serde::Deserialize)]
pub struct SessionHealthParams {
    pub session_id: Option<String>,
}

pub async fn analytics_session_health(
    Query(params): Query<SessionHealthParams>,
) -> Result<Json<analytics::SessionHealth>, (StatusCode, Json<serde_json::Value>)> {
    // #496 (D-3): resolve an 8-char session prefix (or any prefix) the
    // same way `GET /analytics/sessions/{id}` does so `budi vitals
    // --session <short-uuid>` accepts the same id a user copied out of
    // `budi sessions`. Pre-fix the prefix flowed through unresolved and
    // `LEFT JOIN` matched zero rows → silent INSUFFICIENT DATA.
    let sid = match params.session_id {
        Some(ref s) if !s.is_empty() => Some(resolve_sid(s.clone()).await?),
        _ => None,
    };
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::session_health(&conn, sid.as_deref())
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;
    Ok(Json(result))
}

pub async fn analytics_check() -> Result<Json<RepairResponse>, (StatusCode, Json<serde_json::Value>)>
{
    // Read-only diagnostic: opens the DB, asks `migration::check` what
    // would change, and returns the same `RepairResponse` shape as
    // `/admin/repair` so the CLI can render either with one renderer.
    // No `syncing` guard — this never writes.
    let result = tokio::task::spawn_blocking(move || {
        (|| -> anyhow::Result<RepairResponse> {
            let db_path = analytics::db_path()?;
            let conn = analytics::open_db(&db_path)?;
            let report = budi_core::migration::check(&conn)?;
            Ok(RepairResponse {
                from_version: report.from_version,
                to_version: report.to_version,
                migrated: report.migrated,
                repaired: !report.added_columns.is_empty()
                    || !report.added_indexes.is_empty()
                    || !report.removed_tables.is_empty(),
                added_columns: report.added_columns,
                added_indexes: report.added_indexes,
                removed_tables: report.removed_tables,
            })
        })()
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;
    Ok(Json(result))
}

pub async fn analytics_repair(
    State(state): State<AppState>,
) -> Result<Json<RepairResponse>, (StatusCode, Json<serde_json::Value>)> {
    if state
        .syncing
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .is_err()
    {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({ "ok": false, "error": "another operation is in progress" })),
        ));
    }
    let flag = state.syncing.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _busy = BusyFlagGuard::new(flag);
        (|| -> anyhow::Result<RepairResponse> {
            let db_path = analytics::db_path()?;
            let conn = analytics::open_db(&db_path)?;
            let report = budi_core::migration::repair(&conn)?;
            Ok(RepairResponse {
                from_version: report.from_version,
                to_version: report.to_version,
                migrated: report.migrated,
                repaired: !report.added_columns.is_empty()
                    || !report.added_indexes.is_empty()
                    || !report.removed_tables.is_empty(),
                added_columns: report.added_columns,
                added_indexes: report.added_indexes,
                removed_tables: report.removed_tables,
            })
        })()
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;
    Ok(Json(result))
}

pub async fn analytics_session_audit()
-> Result<Json<analytics::SessionAudit>, (StatusCode, Json<serde_json::Value>)> {
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::session_audit(&conn)
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;
    Ok(Json(result))
}
