use axum::Json;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use budi_core::{analytics, claude_data, cost, insights};
use chrono::Datelike;
use serde_json::json;

use super::internal_error;

#[derive(serde::Deserialize)]
pub struct SummaryParams {
    pub since: Option<String>,
    pub until: Option<String>,
    pub provider: Option<String>,
}

pub async fn analytics_summary(
    Query(params): Query<SummaryParams>,
) -> Result<Json<analytics::UsageSummary>, (StatusCode, String)> {
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
pub struct SessionsParams {
    since: Option<String>,
    until: Option<String>,
    search: Option<String>,
    sort_by: Option<String>,
    sort_asc: Option<bool>,
    limit: Option<usize>,
    offset: Option<usize>,
}

pub async fn analytics_sessions(
    Query(params): Query<SessionsParams>,
) -> Result<Json<analytics::PaginatedSessions>, (StatusCode, String)> {
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::session_list(
            &conn,
            &analytics::SessionListParams {
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

pub async fn analytics_session_detail(
    Path(id): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::session_detail(&conn, &id)
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    match result {
        Some(detail) => Ok(Json(detail).into_response()),
        None => Err((StatusCode::NOT_FOUND, "Session not found".to_string())),
    }
}

#[derive(serde::Deserialize)]
pub struct CwdParams {
    since: Option<String>,
    until: Option<String>,
    limit: Option<usize>,
}

pub async fn analytics_projects(
    Query(params): Query<CwdParams>,
) -> Result<Json<Vec<analytics::RepoUsage>>, (StatusCode, String)> {
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

#[derive(serde::Deserialize)]
pub struct InsightsParams {
    since: Option<String>,
    until: Option<String>,
    tz_offset: Option<i32>,
}

pub async fn analytics_insights(
    Query(params): Query<InsightsParams>,
) -> Result<Json<insights::Insights>, (StatusCode, String)> {
    let tz_offset = params.tz_offset.unwrap_or(0);
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        insights::generate_insights(
            &conn,
            params.since.as_deref(),
            params.until.as_deref(),
            tz_offset,
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    Ok(Json(result))
}

pub async fn analytics_models(
    Query(params): Query<SummaryParams>,
) -> Result<Json<Vec<analytics::ModelUsage>>, (StatusCode, String)> {
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
    Query(params): Query<SummaryParams>,
) -> Result<Json<Vec<analytics::BranchCost>>, (StatusCode, String)> {
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

pub async fn analytics_config_files()
-> Result<Json<Vec<analytics::ConfigFileInfo>>, (StatusCode, String)> {
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::config_files(&conn)
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    Ok(Json(result))
}

pub async fn analytics_cost(
    Query(params): Query<SummaryParams>,
) -> Result<Json<cost::CostEstimate>, (StatusCode, String)> {
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
) -> Result<Json<Vec<analytics::ActivityBucket>>, (StatusCode, String)> {
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

pub async fn analytics_timeline()
-> Result<Json<claude_data::ActivityTimeline>, (StatusCode, String)> {
    let result = tokio::task::spawn_blocking(claude_data::read_activity_timeline)
        .await
        .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
        .map_err(internal_error)?;
    Ok(Json(result))
}

pub async fn analytics_plugins() -> Result<Json<Vec<claude_data::PluginInfo>>, (StatusCode, String)>
{
    let result = tokio::task::spawn_blocking(claude_data::read_installed_plugins)
        .await
        .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
        .map_err(internal_error)?;
    Ok(Json(result))
}

pub async fn analytics_active_sessions()
-> Result<Json<Vec<claude_data::ActiveSession>>, (StatusCode, String)> {
    let result = tokio::task::spawn_blocking(claude_data::read_active_sessions)
        .await
        .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
        .map_err(internal_error)?;
    Ok(Json(result))
}

#[derive(serde::Deserialize)]
pub struct PlansParams {
    limit: Option<usize>,
    offset: Option<usize>,
    search: Option<String>,
}

#[derive(serde::Serialize)]
pub struct PaginatedPlans {
    plans: Vec<claude_data::PlanFile>,
    total_count: u64,
}

pub async fn analytics_plans(
    Query(params): Query<PlansParams>,
) -> Result<Json<PaginatedPlans>, (StatusCode, String)> {
    let limit = params.limit.unwrap_or(50);
    let offset = params.offset.unwrap_or(0);
    let search = params.search;
    let result = tokio::task::spawn_blocking(move || {
        let providers = budi_core::provider::available_providers();
        let mut all_plans = Vec::new();
        for provider in &providers {
            if let Ok(plans) = provider.discover_plans() {
                all_plans.extend(plans);
            }
        }
        all_plans.sort_by(|a, b| b.modified.cmp(&a.modified));
        if let Some(ref q) = search
            && !q.is_empty()
        {
            let lower = q.to_lowercase();
            all_plans.retain(|p| {
                p.title.to_lowercase().contains(&lower)
                    || p.name.to_lowercase().contains(&lower)
                    || p.preview.to_lowercase().contains(&lower)
            });
        }
        let total_count = all_plans.len() as u64;
        let page: Vec<_> = all_plans.into_iter().skip(offset).take(limit).collect();
        Ok::<_, anyhow::Error>(PaginatedPlans {
            plans: page,
            total_count,
        })
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;
    Ok(Json(result))
}

pub async fn analytics_memory() -> Result<Json<Vec<claude_data::MemoryFile>>, (StatusCode, String)>
{
    let result = tokio::task::spawn_blocking(claude_data::read_memory_files)
        .await
        .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
        .map_err(internal_error)?;
    Ok(Json(result))
}

pub async fn analytics_permissions()
-> Result<Json<claude_data::PermissionsSummary>, (StatusCode, String)> {
    let result = tokio::task::spawn_blocking(claude_data::read_permissions)
        .await
        .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
        .map_err(internal_error)?;
    Ok(Json(result))
}

#[derive(serde::Deserialize)]
pub struct HistoryParams {
    limit: Option<usize>,
    offset: Option<usize>,
    search: Option<String>,
}

pub async fn analytics_prompts(
    Query(params): Query<HistoryParams>,
) -> Result<Json<claude_data::PromptHistory>, (StatusCode, String)> {
    let limit = params.limit.unwrap_or(50);
    let offset = params.offset.unwrap_or(0);
    let search = params.search;
    let result = tokio::task::spawn_blocking(move || {
        let providers = budi_core::provider::available_providers();
        let mut all_entries = Vec::new();
        for provider in &providers {
            if let Ok(entries) = provider.prompt_history(1000) {
                all_entries.extend(entries);
            }
        }
        // Sort by timestamp descending.
        all_entries.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        // Apply search filter if provided
        if let Some(ref q) = search
            && !q.is_empty()
        {
            let lower = q.to_lowercase();
            all_entries.retain(|e| {
                e.display.to_lowercase().contains(&lower)
                    || e.project
                        .as_deref()
                        .unwrap_or("")
                        .to_lowercase()
                        .contains(&lower)
            });
        }
        let total_count = all_entries.len() as u64;
        let page = all_entries.into_iter().skip(offset).take(limit).collect();
        Ok::<_, anyhow::Error>(claude_data::PromptHistory {
            total_count,
            entries: page,
        })
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;
    Ok(Json(result))
}

pub async fn analytics_providers(
    Query(params): Query<SummaryParams>,
) -> Result<Json<Vec<analytics::ProviderStats>>, (StatusCode, String)> {
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

pub async fn analytics_top_tools(
    Query(params): Query<SummaryParams>,
) -> Result<Json<Vec<(String, u64)>>, (StatusCode, String)> {
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::top_tools(&conn, params.since.as_deref(), params.until.as_deref())
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    Ok(Json(result))
}

pub async fn analytics_mcp_tools(
    Query(params): Query<SummaryParams>,
) -> Result<Json<Vec<analytics::McpToolStat>>, (StatusCode, String)> {
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::mcp_tool_stats(&conn, params.since.as_deref(), params.until.as_deref())
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    Ok(Json(result))
}

pub async fn analytics_registered_providers() -> Json<serde_json::Value> {
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
    Json(json!(list))
}

pub async fn analytics_statusline() -> Result<Json<analytics::StatuslineStats>, (StatusCode, String)>
{
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
        analytics::statusline_stats(&conn, &today, &week_start, &month_start)
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    Ok(Json(result))
}

pub async fn analytics_context_usage(
    Query(params): Query<SummaryParams>,
) -> Result<Json<analytics::ContextUsageStats>, (StatusCode, String)> {
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

pub async fn analytics_interaction_modes(
    Query(params): Query<SummaryParams>,
) -> Result<Json<Vec<(String, u64)>>, (StatusCode, String)> {
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::interaction_mode_breakdown(
            &conn,
            params.since.as_deref(),
            params.until.as_deref(),
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;
    Ok(Json(result))
}
