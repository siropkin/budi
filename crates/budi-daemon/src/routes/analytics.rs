use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use budi_core::{analytics, cost};
use chrono::Datelike;
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
pub struct DateRangeParams {
    pub since: Option<String>,
    pub until: Option<String>,
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
}

pub async fn analytics_summary(
    Query(params): Query<SummaryParams>,
) -> Result<Json<analytics::UsageSummary>, (StatusCode, Json<serde_json::Value>)> {
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::usage_summary_filtered(
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
}

pub async fn analytics_projects(
    Query(params): Query<ListParams>,
) -> Result<Json<Vec<analytics::RepoUsage>>, (StatusCode, Json<serde_json::Value>)> {
    let limit = params.limit.unwrap_or(20).min(200);
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::repo_usage(
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
) -> Result<Json<Vec<analytics::ModelUsage>>, (StatusCode, Json<serde_json::Value>)> {
    let limit = params.limit.unwrap_or(20).min(200);
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::model_usage(
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

pub async fn analytics_branches(
    Query(params): Query<ListParams>,
) -> Result<Json<Vec<analytics::BranchCost>>, (StatusCode, Json<serde_json::Value>)> {
    let limit = params.limit.unwrap_or(20).min(200);
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::branch_cost(
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

pub async fn analytics_cost(
    Query(params): Query<SummaryParams>,
) -> Result<Json<cost::CostEstimate>, (StatusCode, Json<serde_json::Value>)> {
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        cost::estimate_cost_filtered(
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
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::activity_chart(
            &conn,
            params.since.as_deref(),
            params.until.as_deref(),
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
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::provider_stats(&conn, params.since.as_deref(), params.until.as_deref())
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
pub struct MigrateResponse {
    pub current: u32,
    pub target: u32,
    pub migrated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<u32>,
}

#[derive(serde::Serialize)]
pub struct RepairResponse {
    pub from_version: u32,
    pub to_version: u32,
    pub migrated: bool,
    pub repaired: bool,
    pub added_columns: Vec<String>,
}

#[derive(serde::Serialize)]
pub struct IntegrationsResponse {
    pub claude_code_hooks: bool,
    pub cursor_hooks: bool,
    pub cursor_extension: bool,
    pub mcp_server: bool,
    pub otel: bool,
    pub statusline: bool,
    pub starship: bool,
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
    pub cursor_hooks: String,
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
        let now = chrono::Local::now();
        let to_utc = |d: chrono::NaiveDateTime| -> String {
            d.and_local_timezone(chrono::Local)
                .latest()
                .unwrap_or_else(|| chrono::Utc::now().with_timezone(&chrono::Local))
                .with_timezone(&chrono::Utc)
                .to_rfc3339()
        };
        let today = to_utc(now.date_naive().and_hms_opt(0, 0, 0).unwrap());
        let dow = now.weekday().num_days_from_monday();
        let week_start = to_utc(
            (now.date_naive() - chrono::Duration::days(dow as i64))
                .and_hms_opt(0, 0, 0)
                .unwrap(),
        );
        let month_start = to_utc(
            chrono::NaiveDate::from_ymd_opt(now.year(), now.month(), 1)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap(),
        );
        analytics::statusline_stats(&conn, &today, &week_start, &month_start, &params)
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
}

pub async fn analytics_tags(
    Query(params): Query<TagParams>,
) -> Result<Json<Vec<analytics::TagCost>>, (StatusCode, Json<serde_json::Value>)> {
    let limit = params.limit.unwrap_or(20).min(200);
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::tag_stats(
            &conn,
            params.key.as_deref(),
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
            needs_migration: Some(current < target),
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
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::cache_efficiency(&conn, params.since.as_deref(), params.until.as_deref())
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;
    Ok(Json(result))
}

pub async fn analytics_session_cost_curve(
    Query(params): Query<DateRangeParams>,
) -> Result<Json<Vec<analytics::SessionCostBucket>>, (StatusCode, Json<serde_json::Value>)> {
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::session_cost_curve(&conn, params.since.as_deref(), params.until.as_deref())
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;
    Ok(Json(result))
}

pub async fn analytics_cost_confidence(
    Query(params): Query<DateRangeParams>,
) -> Result<Json<Vec<analytics::CostConfidenceStat>>, (StatusCode, Json<serde_json::Value>)> {
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::cost_confidence_stats(&conn, params.since.as_deref(), params.until.as_deref())
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;
    Ok(Json(result))
}

pub async fn analytics_subagent_cost(
    Query(params): Query<DateRangeParams>,
) -> Result<Json<Vec<analytics::SubagentCostStat>>, (StatusCode, Json<serde_json::Value>)> {
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::subagent_cost_stats(&conn, params.since.as_deref(), params.until.as_deref())
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
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        let mut paginated = analytics::session_list(
            &conn,
            &analytics::SessionListParams {
                since: params.since.as_deref(),
                until: params.until.as_deref(),
                search: params.search.as_deref(),
                sort_by: params.sort_by.as_deref(),
                sort_asc: params.sort_asc.unwrap_or(false),
                limit: params.limit.unwrap_or(50).min(200),
                offset: params.offset.unwrap_or(0),
            },
        )?;

        let sids: Vec<&str> = paginated
            .sessions
            .iter()
            .map(|s| s.session_id.as_str())
            .collect();
        if let Ok(health_map) = analytics::session_health_batch(&conn, &sids) {
            for session in &mut paginated.sessions {
                session.health_state = health_map.get(&session.session_id).cloned();
            }
        }

        Ok::<_, anyhow::Error>(paginated)
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;
    Ok(Json(result))
}

pub async fn analytics_session_detail(
    Path(session_id): Path<String>,
) -> Result<Json<analytics::SessionListEntry>, (StatusCode, Json<serde_json::Value>)> {
    let sid = session_id.clone();
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
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        let tags = analytics::session_tags(&conn, &session_id)?;
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
) -> Result<Json<Vec<analytics::MessageRow>>, (StatusCode, Json<serde_json::Value>)> {
    let roles = match params.roles.as_deref() {
        None => analytics::SessionMessageRoles::Assistant,
        Some(raw) => raw
            .parse::<analytics::SessionMessageRoles>()
            .map_err(bad_request)?,
    };
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::session_messages_with_roles(&conn, &session_id, roles)
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;
    Ok(Json(result))
}

#[derive(serde::Deserialize)]
pub struct SessionMessagesQueryParams {
    pub roles: Option<String>,
}

#[derive(serde::Deserialize)]
pub struct SessionHookEventsQueryParams {
    pub linked_only: Option<bool>,
    pub event: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    pub include_raw: Option<bool>,
}

#[derive(serde::Deserialize)]
pub struct SessionOtelEventsQueryParams {
    pub linked_only: Option<bool>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    pub include_raw: Option<bool>,
}

pub async fn analytics_session_hook_events(
    Path(session_id): Path<String>,
    Query(params): Query<SessionHookEventsQueryParams>,
) -> Result<Json<Vec<analytics::SessionHookEventRow>>, (StatusCode, Json<serde_json::Value>)> {
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::session_hook_events(
            &conn,
            &session_id,
            &analytics::SessionHookEventsParams {
                linked_only: params.linked_only.unwrap_or(false),
                event: params.event.as_deref(),
                limit: params.limit.unwrap_or(50).min(500),
                offset: params.offset.unwrap_or(0),
                include_raw: params.include_raw.unwrap_or(false),
            },
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;
    Ok(Json(result))
}

pub async fn analytics_session_otel_events(
    Path(session_id): Path<String>,
    Query(params): Query<SessionOtelEventsQueryParams>,
) -> Result<Json<Vec<analytics::OtelEventRow>>, (StatusCode, Json<serde_json::Value>)> {
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::session_otel_events(
            &conn,
            &session_id,
            &analytics::SessionOtelEventsParams {
                linked_only: params.linked_only.unwrap_or(false),
                limit: params.limit.unwrap_or(50).min(500),
                offset: params.offset.unwrap_or(0),
                include_raw: params.include_raw.unwrap_or(false),
            },
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;
    Ok(Json(result))
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
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::session_health(&conn, params.session_id.as_deref())
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;
    Ok(Json(result))
}

pub async fn analytics_migrate(
    State(state): State<AppState>,
) -> Result<Json<MigrateResponse>, (StatusCode, Json<serde_json::Value>)> {
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
        (|| -> anyhow::Result<MigrateResponse> {
            let db_path = analytics::db_path()?;
            let conn = analytics::open_db(&db_path)?;
            let current = budi_core::migration::current_version(&conn);
            let target = budi_core::migration::SCHEMA_VERSION;
            if current >= target {
                return Ok(MigrateResponse {
                    current,
                    target,
                    migrated: false,
                    from: None,
                });
            }
            drop(conn);
            analytics::open_db_with_migration(&db_path)?;
            Ok(MigrateResponse {
                current: target,
                target,
                migrated: true,
                from: Some(current),
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
                repaired: !report.added_columns.is_empty(),
                added_columns: report.added_columns,
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

pub async fn analytics_tools(
    Query(params): Query<ListParams>,
) -> Result<Json<Vec<budi_core::hooks::ToolStats>>, (StatusCode, Json<serde_json::Value>)> {
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        budi_core::hooks::query_tool_stats(
            &conn,
            params.since.as_deref(),
            params.until.as_deref(),
            params.limit.unwrap_or(20).min(200),
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    Ok(Json(result))
}

pub async fn analytics_mcp(
    Query(params): Query<ListParams>,
) -> Result<Json<Vec<budi_core::hooks::McpStats>>, (StatusCode, Json<serde_json::Value>)> {
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        budi_core::hooks::query_mcp_stats(
            &conn,
            params.since.as_deref(),
            params.until.as_deref(),
            params.limit.unwrap_or(20).min(200),
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    Ok(Json(result))
}
