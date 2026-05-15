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
    /// `?surface=<name>` / `?surfaces=<csv>` host-environment filter (#702).
    /// Mirrors the `agents`/`providers` shape: lowercase canonical names
    /// (`vscode` / `cursor` / `jetbrains` / `terminal` / `unknown`),
    /// CSV-joined when multiple. Singular `surface=` and plural `surfaces=`
    /// both land here so a host extension and a curl one-liner share the
    /// same query string.
    #[serde(alias = "surface")]
    pub surfaces: Option<String>,
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
        surfaces: parse_filter_values(params.surfaces.as_deref()),
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
    /// Singular `?provider=<name>` filter, mirroring `SummaryParams`.
    /// Multi-value `?providers=a,b` flows through the flattened
    /// `DimensionParams.agents` field below (which has `providers` /
    /// `agent` aliases).
    pub provider: Option<String>,
    #[serde(flatten)]
    pub filters: DimensionParams,
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
    let filters = parse_dimension_filters(&params.filters);
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
                provider: params.provider.as_deref(),
                filters: &filters,
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

/// `GET /analytics/status_snapshot` — summary + cost + providers from one
/// connection so `budi status` sees a single consistent snapshot (#619).
pub async fn analytics_status_snapshot(
    Query(params): Query<SummaryParams>,
) -> Result<Json<analytics::StatusSnapshot>, (StatusCode, Json<serde_json::Value>)> {
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::status_snapshot(
            &conn,
            params.since.as_deref(),
            params.until.as_deref(),
            params.provider.as_deref(),
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

/// `GET /analytics/surfaces` — per-host-environment breakdown (#702).
/// Mirror of `/analytics/providers` keyed on the `surface` axis from #701
/// (`vscode` / `cursor` / `jetbrains` / `terminal` / `unknown`). Empty
/// surfaces (no rows in window) are excluded so a single-host install
/// never sees three empty rows.
pub async fn analytics_surfaces(
    Query(params): Query<DateRangeParams>,
) -> Result<Json<Vec<analytics::SurfaceStats>>, (StatusCode, Json<serde_json::Value>)> {
    let filters = parse_dimension_filters(&params.filters);
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::surface_stats_with_filters(
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

// ─── #816 handler coverage tests ─────────────────────────────────────────
//
// Baseline coverage on `routes/analytics.rs` was 1.27% (the lowest line-
// coverage figure in the workspace per #804). The query/SQL layer is well
// covered by `analytics/queries/*` tests; what was missing was direct
// exercise of the axum handler layer: query-extractor 400 paths,
// pagination clamps, host-header rejection, and the empty-DB success
// shape. These tests close that gap. They run each handler in a fresh
// tempdir-scoped HOME with a freshly-migrated empty DB so they stay
// hermetic and never observe data from a developer's actual budi home.
#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::{Method, Request, StatusCode};
    use axum::middleware::from_fn_with_state;
    use axum::routing::get;
    use http_body_util::BodyExt;
    use std::net::SocketAddr;
    use std::sync::Mutex;
    use tower::ServiceExt;

    /// Process-global `HOME` / `BUDI_HOME` mutations need a mutex —
    /// cargo runs tests in parallel and unsynchronized env writes are
    /// unsound on macOS (see #366 PR history).
    static HOME_MUTEX: Mutex<()> = Mutex::new(());

    struct HomeGuard {
        prev_home: Option<String>,
        prev_budi_home: Option<String>,
        _tmp: tempfile::TempDir,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl HomeGuard {
        fn new() -> Self {
            let lock = HOME_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            let tmp = tempfile::tempdir().expect("tempdir for HomeGuard");
            let prev_home = std::env::var("HOME").ok();
            let prev_budi_home = std::env::var("BUDI_HOME").ok();
            // SAFETY: serialized by HOME_MUTEX above; no other thread
            // reads HOME / BUDI_HOME for the duration of the guard.
            unsafe { std::env::set_var("HOME", tmp.path()) };
            unsafe { std::env::remove_var("BUDI_HOME") };
            Self {
                prev_home,
                prev_budi_home,
                _tmp: tmp,
                _lock: lock,
            }
        }

        /// Materialize the analytics DB at the redirected home so handler
        /// success paths don't fall over on `open_db` for a missing schema.
        fn init_db(&self) {
            let db_path = analytics::db_path().expect("db_path");
            analytics::open_db_with_migration(&db_path).expect("migrate empty db");
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match &self.prev_home {
                Some(h) => unsafe { std::env::set_var("HOME", h) },
                None => unsafe { std::env::remove_var("HOME") },
            }
            match &self.prev_budi_home {
                Some(h) => unsafe { std::env::set_var("BUDI_HOME", h) },
                None => unsafe { std::env::remove_var("BUDI_HOME") },
            }
        }
    }

    // ─── Direct-handler validation tests (no DB / pre-DB validation) ────

    #[tokio::test]
    async fn analytics_messages_rejects_unknown_sort_by_with_400() {
        let _guard = HomeGuard::new();
        let params = MessagesParams {
            since: None,
            until: None,
            search: None,
            sort_by: Some("not_a_sort_column".to_string()),
            sort_asc: None,
            limit: None,
            offset: None,
            provider: None,
            filters: DimensionParams {
                agents: None,
                models: None,
                projects: None,
                branches: None,
                surfaces: None,
            },
        };
        let err = analytics_messages(axum::extract::Query(params))
            .await
            .expect_err("unknown sort_by must 400");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        let body = err.1.0;
        assert_eq!(body["ok"], false);
        assert!(
            body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("invalid sort_by"),
            "error must mention invalid sort_by: {body}"
        );
    }

    #[tokio::test]
    async fn analytics_sessions_rejects_unknown_sort_by_with_400() {
        let _guard = HomeGuard::new();
        let params = SessionsQueryParams {
            since: None,
            until: None,
            search: None,
            sort_by: Some("garbage".to_string()),
            sort_asc: None,
            limit: None,
            offset: None,
            ticket: None,
            activity: None,
            filters: DimensionParams {
                agents: None,
                models: None,
                projects: None,
                branches: None,
                surfaces: None,
            },
        };
        let err = analytics_sessions(axum::extract::Query(params))
            .await
            .expect_err("unknown sort_by must 400");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn analytics_activity_rejects_unknown_granularity_with_400() {
        let _guard = HomeGuard::new();
        let params = ActivityChartParams {
            since: None,
            until: None,
            granularity: Some("fortnight".to_string()),
            tz_offset: None,
            filters: DimensionParams {
                agents: None,
                models: None,
                projects: None,
                branches: None,
                surfaces: None,
            },
        };
        let err = analytics_activity(axum::extract::Query(params))
            .await
            .expect_err("unknown granularity must 400");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(
            err.1.0["error"]
                .as_str()
                .unwrap_or_default()
                .contains("invalid granularity"),
        );
    }

    #[tokio::test]
    async fn analytics_resolve_session_rejects_unknown_token_with_400() {
        let _guard = HomeGuard::new();
        let params = ResolveSessionParams {
            token: "neither-current-nor-latest".to_string(),
            cwd: None,
        };
        let err = analytics_resolve_session(axum::extract::Query(params))
            .await
            .expect_err("unknown token must 400");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(
            err.1.0["error"]
                .as_str()
                .unwrap_or_default()
                .contains("unknown session token"),
        );
    }

    #[tokio::test]
    async fn analytics_file_detail_rejects_absolute_path_with_400() {
        let _guard = HomeGuard::new();
        let params = FileDetailParams {
            since: None,
            until: None,
            repo_id: None,
        };
        let err = analytics_file_detail(
            axum::extract::Path("/etc/passwd".to_string()),
            axum::extract::Query(params),
        )
        .await
        .expect_err("absolute path must 400");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn analytics_file_detail_rejects_traversal_with_400() {
        let _guard = HomeGuard::new();
        let params = FileDetailParams {
            since: None,
            until: None,
            repo_id: None,
        };
        let err = analytics_file_detail(
            axum::extract::Path("src/../../etc/passwd".to_string()),
            axum::extract::Query(params),
        )
        .await
        .expect_err("traversal must 400");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn analytics_resolve_session_current_with_empty_cwd_falls_back_to_latest() {
        // Empty cwd + `token=current` must not 400; the handler should
        // record a fallback_reason and try the DB for the latest session.
        // With an empty DB, that returns 404 — pin that contract.
        let guard = HomeGuard::new();
        guard.init_db();
        let params = ResolveSessionParams {
            token: "current".to_string(),
            cwd: Some(String::new()),
        };
        let err = analytics_resolve_session(axum::extract::Query(params))
            .await
            .expect_err("empty DB → no sessions → 404");
        assert_eq!(err.0, StatusCode::NOT_FOUND);
    }

    // ─── Empty-DB success-shape tests ───────────────────────────────────

    #[tokio::test]
    async fn analytics_registered_providers_returns_static_list_without_db() {
        // This handler never opens the DB — it surfaces the registry.
        let _guard = HomeGuard::new();
        let Json(list) = analytics_registered_providers()
            .await
            .expect("registered_providers must always succeed");
        assert!(!list.is_empty(), "at least one provider must be registered");
        for entry in &list {
            assert!(!entry.name.is_empty());
            assert!(!entry.display_name.is_empty());
        }
    }

    #[tokio::test]
    async fn analytics_summary_with_empty_db_returns_zeroed_shape() {
        let guard = HomeGuard::new();
        guard.init_db();
        let params = SummaryParams {
            since: None,
            until: None,
            provider: None,
            filters: DimensionParams {
                agents: None,
                models: None,
                projects: None,
                branches: None,
                surfaces: None,
            },
        };
        let Json(summary) = analytics_summary(axum::extract::Query(params))
            .await
            .expect("summary must succeed on empty DB");
        // Round-trip the body through serde so the test asserts on the
        // wire shape the CLI / dashboard consume, not the in-memory
        // struct field names.
        let body = serde_json::to_value(&summary).expect("serialize summary");
        assert!(body.is_object(), "summary must serialize as a JSON object");
    }

    #[tokio::test]
    async fn analytics_messages_with_empty_db_returns_empty_page() {
        let guard = HomeGuard::new();
        guard.init_db();
        let params = MessagesParams {
            since: None,
            until: None,
            search: None,
            sort_by: None,
            sort_asc: None,
            limit: None,
            offset: None,
            provider: None,
            filters: DimensionParams {
                agents: None,
                models: None,
                projects: None,
                branches: None,
                surfaces: None,
            },
        };
        let Json(page) = analytics_messages(axum::extract::Query(params))
            .await
            .expect("messages must succeed on empty DB");
        assert!(page.messages.is_empty(), "empty DB → no messages");
    }

    #[tokio::test]
    async fn analytics_messages_clamps_oversized_limit_to_200() {
        // `?limit=99999` is the "out-of-range pagination" path on
        // acceptance criterion #4. The handler caps via `.min(200)` so
        // the request must succeed (no 400, no 500) and return at most
        // 200 rows. On an empty DB the page is empty; we just pin the
        // success outcome here.
        let guard = HomeGuard::new();
        guard.init_db();
        let params = MessagesParams {
            since: None,
            until: None,
            search: None,
            sort_by: None,
            sort_asc: None,
            limit: Some(99_999),
            offset: Some(0),
            provider: None,
            filters: DimensionParams {
                agents: None,
                models: None,
                projects: None,
                branches: None,
                surfaces: None,
            },
        };
        let Json(page) = analytics_messages(axum::extract::Query(params))
            .await
            .expect("oversized limit must not 400");
        assert!(page.messages.len() <= 200);
    }

    #[tokio::test]
    async fn analytics_projects_with_zero_limit_succeeds() {
        // `--limit 0` is the CLI's "no cap" sentinel (see
        // `resolve_breakdown_limit`). The handler must accept it and
        // not fall over on the `.min(100_000)` branch.
        let guard = HomeGuard::new();
        guard.init_db();
        let params = ListParams {
            since: None,
            until: None,
            limit: Some(0),
            filters: DimensionParams {
                agents: None,
                models: None,
                projects: None,
                branches: None,
                surfaces: None,
            },
        };
        let Json(_page) = analytics_projects(axum::extract::Query(params))
            .await
            .expect("limit=0 must succeed");
    }

    #[tokio::test]
    async fn analytics_filter_options_returns_shape_on_empty_db() {
        let guard = HomeGuard::new();
        guard.init_db();
        let params = FilterOptionsParams { limit: None };
        let Json(opts) = analytics_filter_options(axum::extract::Query(params))
            .await
            .expect("filter_options must succeed on empty DB");
        let body = serde_json::to_value(&opts).expect("serialize");
        assert!(body.is_object(), "filter_options is a JSON object");
    }

    #[tokio::test]
    async fn analytics_status_snapshot_returns_shape_on_empty_db() {
        let guard = HomeGuard::new();
        guard.init_db();
        let params = SummaryParams {
            since: None,
            until: None,
            provider: None,
            filters: DimensionParams {
                agents: None,
                models: None,
                projects: None,
                branches: None,
                surfaces: None,
            },
        };
        let Json(snapshot) = analytics_status_snapshot(axum::extract::Query(params))
            .await
            .expect("status_snapshot must succeed on empty DB");
        let _ = serde_json::to_value(&snapshot).expect("serialize");
    }

    #[tokio::test]
    async fn analytics_schema_version_reports_current_after_migration() {
        // After `open_db_with_migration`, the schema version handler must
        // report `exists: true` with `current == target`. This is the
        // happy path the CLI keys off when deciding whether to suggest
        // `budi db check --fix`.
        let guard = HomeGuard::new();
        guard.init_db();
        let Json(resp) = analytics_schema_version()
            .await
            .expect("schema_version must succeed on migrated DB");
        assert!(resp.exists);
        assert!(resp.target > 0, "target SCHEMA_VERSION must be > 0");
        assert_eq!(
            resp.current, resp.target,
            "freshly migrated DB must be at target version"
        );
        assert_eq!(resp.needs_migration, Some(false));
    }

    #[tokio::test]
    async fn analytics_branch_detail_404s_for_missing_branch() {
        // Empty DB → `branch_cost_single` returns None → handler maps
        // to 404 via the shared `not_found` helper.
        let guard = HomeGuard::new();
        guard.init_db();
        let params = BranchDetailParams {
            since: None,
            until: None,
            repo_id: None,
        };
        let err = analytics_branch_detail(
            axum::extract::Path("nonexistent-branch".to_string()),
            axum::extract::Query(params),
        )
        .await
        .expect_err("missing branch must 404");
        assert_eq!(err.0, StatusCode::NOT_FOUND);
    }

    // ─── Full-router middleware tests ───────────────────────────────────
    //
    // The handler-level tests above call the functions directly; the
    // tests below wire the routes into a Router with the
    // `require_local_host` middleware so the host-allowlist branch
    // (the DNS-rebinding defense in #695) is exercised against the
    // analytics surface specifically.

    fn analytics_test_router() -> Router {
        Router::new()
            .route("/analytics/summary", get(analytics_summary))
            .route("/analytics/messages", get(analytics_messages))
            .route("/analytics/projects", get(analytics_projects))
            .layer(from_fn_with_state(
                super::super::HostAllowlist::for_tests(),
                super::super::require_local_host,
            ))
    }

    fn loopback_request(uri: &str, host: Option<&'static str>) -> Request<Body> {
        let mut req = Request::builder()
            .method(Method::GET)
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        if let Some(h) = host {
            req.headers_mut().insert(
                axum::http::header::HOST,
                axum::http::HeaderValue::from_static(h),
            );
        }
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 54545))));
        req
    }

    #[tokio::test]
    async fn analytics_router_rejects_non_local_host_with_403() {
        // The DNS-rebinding scenario: peer IP is loopback (because the
        // browser dialed 127.0.0.1) but the Host header is an attacker
        // name. The middleware must reject before any handler runs.
        let _guard = HomeGuard::new();
        let app = analytics_test_router();
        let req = loopback_request("/analytics/summary", Some("attacker.example"));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "invalid Host header");
    }

    #[tokio::test]
    async fn analytics_router_accepts_loopback_host_on_summary() {
        let guard = HomeGuard::new();
        guard.init_db();
        let app = analytics_test_router();
        let req = loopback_request("/analytics/summary", Some("127.0.0.1"));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn analytics_router_rejects_missing_host_header_with_403() {
        let _guard = HomeGuard::new();
        let app = analytics_test_router();
        let req = loopback_request("/analytics/messages", None);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    // ─── Bulk empty-DB success tests for the remaining GET breakdowns ──
    //
    // These handlers share the same shape: extract `ListParams` /
    // `DateRangeParams`, spawn-blocking into a query against an empty
    // DB, paginate or return the raw vec. Coverage-wise they're the
    // long tail of the module; each adds 5-15 lines of executed code.
    // Folded into a single test per handler family so the file doesn't
    // grow a tail of near-identical bodies.

    fn empty_dimension_filters() -> DimensionParams {
        DimensionParams {
            agents: None,
            models: None,
            projects: None,
            branches: None,
            surfaces: None,
        }
    }

    fn empty_list_params() -> ListParams {
        ListParams {
            since: None,
            until: None,
            limit: None,
            filters: empty_dimension_filters(),
        }
    }

    fn empty_date_range_params() -> DateRangeParams {
        DateRangeParams {
            since: None,
            until: None,
            filters: empty_dimension_filters(),
        }
    }

    fn empty_summary_params() -> SummaryParams {
        SummaryParams {
            since: None,
            until: None,
            provider: None,
            filters: empty_dimension_filters(),
        }
    }

    #[tokio::test]
    async fn analytics_breakdowns_succeed_on_empty_db() {
        let guard = HomeGuard::new();
        guard.init_db();

        let Json(_) = analytics_projects(axum::extract::Query(empty_list_params()))
            .await
            .expect("projects");
        let Json(_) = analytics_non_repo(axum::extract::Query(empty_list_params()))
            .await
            .expect("non_repo");
        let Json(_) = analytics_models(axum::extract::Query(empty_list_params()))
            .await
            .expect("models");
        let Json(_) = analytics_branches(axum::extract::Query(empty_list_params()))
            .await
            .expect("branches");

        let tag_params = TagParams {
            since: None,
            until: None,
            key: None,
            limit: None,
            filters: empty_dimension_filters(),
        };
        let Json(_) = analytics_tags(axum::extract::Query(tag_params))
            .await
            .expect("tags");

        let ticket_params = TicketListParams {
            since: None,
            until: None,
            limit: None,
            filters: empty_dimension_filters(),
        };
        let Json(_) = analytics_tickets(axum::extract::Query(ticket_params))
            .await
            .expect("tickets");

        let activity_list_params = ActivityListParams {
            since: None,
            until: None,
            limit: None,
            filters: empty_dimension_filters(),
        };
        let Json(_) = analytics_activities(axum::extract::Query(activity_list_params))
            .await
            .expect("activities");

        let file_list_params = FileListParams {
            since: None,
            until: None,
            limit: None,
            filters: empty_dimension_filters(),
        };
        let Json(_) = analytics_files(axum::extract::Query(file_list_params))
            .await
            .expect("files");
    }

    #[tokio::test]
    async fn analytics_date_range_endpoints_succeed_on_empty_db() {
        let guard = HomeGuard::new();
        guard.init_db();

        let Json(_) = analytics_providers(axum::extract::Query(empty_date_range_params()))
            .await
            .expect("providers");
        let Json(_) = analytics_surfaces(axum::extract::Query(empty_date_range_params()))
            .await
            .expect("surfaces");
        let Json(_) = analytics_cache_efficiency(axum::extract::Query(empty_date_range_params()))
            .await
            .expect("cache_efficiency");
        let Json(_) = analytics_session_cost_curve(axum::extract::Query(empty_date_range_params()))
            .await
            .expect("session_cost_curve");
        let Json(_) = analytics_cost_confidence(axum::extract::Query(empty_date_range_params()))
            .await
            .expect("cost_confidence");
        let Json(_) = analytics_subagent_cost(axum::extract::Query(empty_date_range_params()))
            .await
            .expect("subagent_cost");

        let activity_chart = ActivityChartParams {
            since: None,
            until: None,
            granularity: Some("day".to_string()),
            tz_offset: None,
            filters: empty_dimension_filters(),
        };
        let Json(_) = analytics_activity(axum::extract::Query(activity_chart))
            .await
            .expect("activity (day)");
    }

    #[tokio::test]
    async fn analytics_cost_and_audit_succeed_on_empty_db() {
        let guard = HomeGuard::new();
        guard.init_db();

        let Json(_) = analytics_cost(axum::extract::Query(empty_summary_params()))
            .await
            .expect("cost");
        let Json(_) = analytics_session_audit().await.expect("session_audit");
        let Json(_) = analytics_check().await.expect("check");
    }

    #[tokio::test]
    async fn analytics_sessions_succeed_on_empty_db() {
        let guard = HomeGuard::new();
        guard.init_db();

        let params = SessionsQueryParams {
            since: None,
            until: None,
            search: None,
            sort_by: None,
            sort_asc: None,
            limit: None,
            offset: None,
            ticket: None,
            activity: None,
            filters: empty_dimension_filters(),
        };
        let Json(page) = analytics_sessions(axum::extract::Query(params))
            .await
            .expect("sessions");
        assert!(page.sessions.is_empty());

        let Json(_) = analytics_session_health(axum::extract::Query(SessionHealthParams {
            session_id: None,
        }))
        .await
        .expect("session_health");
    }

    #[tokio::test]
    async fn analytics_session_detail_404s_for_missing_session() {
        let guard = HomeGuard::new();
        guard.init_db();
        let err = analytics_session_detail(axum::extract::Path("deadbeef".to_string()))
            .await
            .expect_err("missing session must 404");
        assert_eq!(err.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn analytics_message_detail_404s_for_missing_message() {
        let guard = HomeGuard::new();
        guard.init_db();
        let err = analytics_message_detail(axum::extract::Path("not-a-uuid".to_string()))
            .await
            .expect_err("missing message must 404");
        assert_eq!(err.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn analytics_ticket_and_activity_detail_404_for_missing_keys() {
        let guard = HomeGuard::new();
        guard.init_db();
        let ticket_err = analytics_ticket_detail(
            axum::extract::Path("ENG-999".to_string()),
            axum::extract::Query(TicketDetailParams {
                since: None,
                until: None,
                repo_id: None,
            }),
        )
        .await
        .expect_err("missing ticket must 404");
        assert_eq!(ticket_err.0, StatusCode::NOT_FOUND);

        let activity_err = analytics_activity_detail(
            axum::extract::Path("bugfix".to_string()),
            axum::extract::Query(ActivityDetailParams {
                since: None,
                until: None,
                repo_id: None,
            }),
        )
        .await
        .expect_err("missing activity must 404");
        assert_eq!(activity_err.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn analytics_file_detail_404s_for_missing_repo_relative_path() {
        let guard = HomeGuard::new();
        guard.init_db();
        let err = analytics_file_detail(
            axum::extract::Path("src/lib.rs".to_string()),
            axum::extract::Query(FileDetailParams {
                since: None,
                until: None,
                repo_id: None,
            }),
        )
        .await
        .expect_err("missing file must 404");
        assert_eq!(err.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn analytics_router_400s_on_unparseable_query() {
        // `limit=` is a string the `Option<usize>` deserializer can't
        // accept — axum maps the extractor failure to 400. Pins the
        // contract that bad query shapes don't bypass to a 500.
        let _guard = HomeGuard::new();
        let app = analytics_test_router();
        let req = loopback_request("/analytics/projects?limit=not-a-number", Some("127.0.0.1"));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
