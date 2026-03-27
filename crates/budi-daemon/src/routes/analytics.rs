use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use budi_core::{analytics, cost};
use chrono::Datelike;
use serde_json::json;

use super::{bad_request, internal_error, not_found};
use crate::AppState;

#[derive(serde::Deserialize)]
pub struct DateRangeParams {
    pub since: Option<String>,
    pub until: Option<String>,
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

const VALID_SORT_BY: &[&str] = &["timestamp", "cost", "model", "tokens", "provider", "branch", "git_branch", "ticket", "repo_id"];

pub async fn analytics_messages(
    Query(params): Query<MessagesParams>,
) -> Result<Json<analytics::PaginatedMessages>, (StatusCode, Json<serde_json::Value>)> {
    if let Some(ref sort) = params.sort_by
        && !VALID_SORT_BY.contains(&sort.as_str())
    {
        return Err(bad_request(format!(
            "invalid sort_by '{}'; valid values: {}",
            sort,
            VALID_SORT_BY.join(", ")
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
                limit: params.limit.unwrap_or(50),
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
pub struct ProjectsParams {
    pub since: Option<String>,
    pub until: Option<String>,
    pub limit: Option<usize>,
}

pub async fn analytics_projects(
    Query(params): Query<ProjectsParams>,
) -> Result<Json<Vec<analytics::RepoUsage>>, (StatusCode, Json<serde_json::Value>)> {
    let limit = params.limit.unwrap_or(20);
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
    Query(params): Query<DateRangeParams>,
) -> Result<Json<Vec<analytics::ModelUsage>>, (StatusCode, Json<serde_json::Value>)> {
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::model_usage(&conn, params.since.as_deref(), params.until.as_deref())
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    Ok(Json(result))
}

pub async fn analytics_branches(
    Query(params): Query<DateRangeParams>,
) -> Result<Json<Vec<analytics::BranchCost>>, (StatusCode, Json<serde_json::Value>)> {
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::branch_cost(&conn, params.since.as_deref(), params.until.as_deref())
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
    let limit = params.limit.unwrap_or(20);
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

#[derive(serde::Deserialize)]
pub struct BranchDetailParams {
    pub since: Option<String>,
    pub until: Option<String>,
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
-> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<serde_json::Value> {
        let db_path = analytics::db_path()?;
        if !db_path.exists() {
            return Ok(json!({ "current": 0, "target": budi_core::migration::SCHEMA_VERSION, "exists": false }));
        }
        let conn = analytics::open_db(&db_path)?;
        let current = budi_core::migration::current_version(&conn);
        let target = budi_core::migration::SCHEMA_VERSION;
        Ok(json!({ "current": current, "target": target, "exists": true, "needs_migration": current < target }))
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;
    Ok(Json(result))
}

pub async fn analytics_migrate(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if state.syncing.load(std::sync::atomic::Ordering::SeqCst) {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({ "ok": false, "error": "cannot migrate while sync is running" })),
        ));
    }
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        let current = budi_core::migration::current_version(&conn);
        let target = budi_core::migration::SCHEMA_VERSION;
        if current >= target {
            return Ok(json!({ "current": current, "target": target, "migrated": false }));
        }
        drop(conn);
        analytics::open_db_with_migration(&db_path)?;
        Ok(json!({ "current": target, "target": target, "migrated": true, "from": current }))
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;
    Ok(Json(result))
}
