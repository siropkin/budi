use axum::Json;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use budi_core::{analytics, cost};
use chrono::Datelike;
use serde_json::json;

use super::internal_error;

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
    since: Option<String>,
    until: Option<String>,
    search: Option<String>,
    sort_by: Option<String>,
    sort_asc: Option<bool>,
    limit: Option<usize>,
    offset: Option<usize>,
}

pub async fn analytics_messages(
    Query(params): Query<MessagesParams>,
) -> Result<Json<analytics::PaginatedMessages>, (StatusCode, Json<serde_json::Value>)> {
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
    since: Option<String>,
    until: Option<String>,
    limit: Option<usize>,
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
    since: Option<String>,
    until: Option<String>,
    granularity: Option<String>,
    tz_offset: Option<i32>,
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

pub async fn analytics_registered_providers() -> Result<Json<Vec<serde_json::Value>>, (StatusCode, Json<serde_json::Value>)> {
    let providers = budi_core::provider::all_providers();
    let list: Vec<serde_json::Value> = providers
        .iter()
        .map(|p| {
            json!({
                "name": p.name(),
                "display_name": p.display_name(),
            })
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

pub async fn analytics_context_usage(
    Query(params): Query<DateRangeParams>,
) -> Result<Json<analytics::ContextUsageStats>, (StatusCode, Json<serde_json::Value>)> {
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::context_usage_stats(&conn, params.since.as_deref(), params.until.as_deref())
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;
    Ok(Json(result))
}


#[derive(serde::Deserialize)]
pub struct TagParams {
    since: Option<String>,
    until: Option<String>,
    key: Option<String>,
    limit: Option<usize>,
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
    since: Option<String>,
    until: Option<String>,
}

pub async fn analytics_branch_detail(
    Path(branch): Path<String>,
    Query(params): Query<BranchDetailParams>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::branch_cost_single(&conn, &branch, params.since.as_deref(), params.until.as_deref())
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    match result {
        Some(detail) => Ok(Json(detail).into_response()),
        None => Ok(Json(serde_json::Value::Null).into_response()),
    }
}

pub async fn analytics_provider_count() -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::provider_count(&conn)
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;
    Ok(Json(json!({ "count": result })))
}

pub async fn analytics_schema_version() -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<serde_json::Value> {
        let db_path = analytics::db_path()?;
        if !db_path.exists() {
            return Ok(json!({ "current": 0, "target": budi_core::migration::SCHEMA_VERSION, "exists": false }));
        }
        let conn = analytics::open_db(&db_path)?;
        let current = budi_core::migration::current_version(&conn);
        let target = budi_core::migration::SCHEMA_VERSION;
        Ok(json!({ "current": current, "target": target, "exists": true }))
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;
    Ok(Json(result))
}

pub async fn analytics_migrate() -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
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
