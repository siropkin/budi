use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Result;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use budi_core::analytics;
use budi_core::claude_data;
use budi_core::config::{self, BudiConfig, DEFAULT_DAEMON_HOST, DEFAULT_DAEMON_PORT};
use budi_core::cost;
use budi_core::daemon::DaemonState;
use budi_core::hooks::UserPromptSubmitInput;
use budi_core::insights;
use budi_core::pre_filter;
use budi_core::rpc::{StatusRequest, StatusResponse};
use clap::{Parser, Subcommand};
use serde_json::json;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "budi-daemon")]
#[command(about = "budi analytics daemon")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Serve {
        #[arg(long, default_value = DEFAULT_DAEMON_HOST)]
        host: String,
        #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
        port: u16,
    },
}

#[derive(Clone)]
struct AppState {
    daemon_state: DaemonState,
}

fn build_router(app_state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/status", post(status_repo))
        .route("/stats", get(stats))
        .route("/session-stats", post(session_stats))
        .route("/hook/prompt-submit", post(hook_prompt_submit))
        .route("/hook/tool-use", post(hook_tool_use))
        .route("/sync", post(sync_analytics))
        .route("/analytics/summary", get(analytics_summary))
        .route("/analytics/sessions", get(analytics_sessions))
        .route("/analytics/session/{id}", get(analytics_session_detail))
        .route("/analytics/cwd", get(analytics_cwd))
        .route("/analytics/insights", get(analytics_insights))
        .route("/analytics/cost", get(analytics_cost))
        .route("/analytics/models", get(analytics_models))
        .route("/analytics/config-files", get(analytics_config_files))
        .route("/analytics/activity", get(analytics_activity))
        .route("/analytics/activity-chart", get(analytics_activity_chart))
        .route("/analytics/plugins", get(analytics_plugins))
        .route("/analytics/active-sessions", get(analytics_active_sessions))
        .route("/analytics/plans", get(analytics_plans))
        .route("/analytics/memory", get(analytics_memory))
        .route("/analytics/permissions", get(analytics_permissions))
        .route("/analytics/history", get(analytics_history))
        .route("/analytics/statusline", get(analytics_statusline))
        .route("/dashboard", get(dashboard))
        .route("/dashboard/setup", get(dashboard))
        .route("/dashboard/plans", get(dashboard))
        .route("/dashboard/prompts", get(dashboard))
        .route("/dashboard/insights", get(dashboard))
        .with_state(app_state)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .compact()
        .init();

    let cli = Cli::parse();
    let (host, port) = match cli.command.unwrap_or(Commands::Serve {
        host: DEFAULT_DAEMON_HOST.to_string(),
        port: DEFAULT_DAEMON_PORT,
    }) {
        Commands::Serve { host, port } => (host, port),
    };

    let app_state = AppState {
        daemon_state: DaemonState::new(),
    };

    let app = build_router(app_state);

    // Auto-sync JSONL transcripts every 30 seconds to keep analytics fresh.
    tokio::spawn(async {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        interval.tick().await; // skip immediate first tick
        loop {
            interval.tick().await;
            let _ = tokio::task::spawn_blocking(|| {
                let db_path = analytics::db_path().ok()?;
                let mut conn = analytics::open_db(&db_path).ok()?;
                analytics::sync_all(&mut conn).ok()
            })
            .await;
        }
    });

    let addr: SocketAddr = format!("{}:{}", host, port).parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("budi-daemon listening on {}", addr);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "ok": true }))
}

#[derive(serde::Deserialize)]
struct StatsParams {
    repo_root: Option<String>,
}

async fn stats(
    State(state): State<AppState>,
    Query(params): Query<StatsParams>,
) -> Json<serde_json::Value> {
    let (queries, skips) = if let Some(ref repo_root) = params.repo_root {
        state
            .daemon_state
            .repo_stats_snapshot(repo_root)
            .unwrap_or_default()
    } else {
        state.daemon_state.query_stats_snapshot()
    };
    Json(json!({
        "queries": queries,
        "skips": skips,
    }))
}

async fn session_stats(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let session_id = body
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if let Some(snap) = state.daemon_state.session_stats(session_id) {
        Json(serde_json::to_value(snap).unwrap_or_default())
    } else {
        Json(json!({}))
    }
}

async fn hook_prompt_submit(
    State(state): State<AppState>,
    Json(input): Json<UserPromptSubmitInput>,
) -> Json<budi_core::hooks::UserPromptSubmitOutput> {
    let cwd = PathBuf::from(&input.common.cwd);
    let session_id = input.common.session_id.clone();

    let repo_root = match config::find_repo_root(&cwd) {
        Ok(path) => path,
        Err(_) => {
            return Json(
                budi_core::hooks::UserPromptSubmitOutput::allow_with_context(String::new()),
            );
        }
    };

    let skipped = pre_filter::is_obviously_non_code(&input.prompt)
        || pre_filter::is_conversational_followup(&input.prompt);

    let repo_root_str = repo_root.display().to_string();
    state
        .daemon_state
        .record_prompt(&repo_root_str, Some(&session_id), skipped);

    Json(budi_core::hooks::UserPromptSubmitOutput::allow_with_context(String::new()))
}

async fn sync_analytics() -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    // Run sync in a blocking task since it does file I/O and SQLite writes.
    let result = tokio::task::spawn_blocking(|| {
        let db_path = analytics::db_path()?;
        let mut conn = analytics::open_db(&db_path)?;
        analytics::sync_all(&mut conn)
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    Ok(Json(json!({
        "files_synced": result.0,
        "messages_ingested": result.1,
    })))
}

#[derive(serde::Deserialize)]
struct SummaryParams {
    since: Option<String>,
    until: Option<String>,
}

async fn analytics_summary(
    Query(params): Query<SummaryParams>,
) -> Result<Json<analytics::UsageSummary>, (StatusCode, String)> {
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::usage_summary(&conn, params.since.as_deref(), params.until.as_deref())
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    Ok(Json(result))
}

async fn hook_tool_use(
    Json(_input): Json<budi_core::hooks::PostToolUseInput>,
) -> Json<serde_json::Value> {
    // Tool use tracking will be added in the analytics phase.
    Json(json!({}))
}

async fn status_repo(
    State(state): State<AppState>,
    Json(request): Json<StatusRequest>,
) -> Result<Json<StatusResponse>, (StatusCode, String)> {
    let config = request_config(&request.repo_root).map_err(internal_error)?;
    let response = state
        .daemon_state
        .status(request, &config)
        .map_err(internal_error)?;
    Ok(Json(response))
}

fn request_config(repo_root: &str) -> Result<BudiConfig> {
    let root = std::path::Path::new(repo_root);
    config::load_or_default(root)
}

async fn analytics_sessions(
    Query(params): Query<SummaryParams>,
) -> Result<Json<Vec<analytics::SessionSummary>>, (StatusCode, String)> {
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        analytics::session_list(&conn, params.since.as_deref(), params.until.as_deref())
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    Ok(Json(result))
}

async fn analytics_session_detail(
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
struct CwdParams {
    since: Option<String>,
    until: Option<String>,
    limit: Option<usize>,
}

async fn analytics_cwd(
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
struct InsightsParams {
    since: Option<String>,
    until: Option<String>,
    tz_offset: Option<i32>,
}

async fn analytics_insights(
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

async fn analytics_models(
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

async fn analytics_config_files()
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

async fn analytics_cost(
    Query(params): Query<SummaryParams>,
) -> Result<Json<cost::CostEstimate>, (StatusCode, String)> {
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        cost::estimate_cost(&conn, params.since.as_deref(), params.until.as_deref())
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    Ok(Json(result))
}

#[derive(serde::Deserialize)]
struct ActivityChartParams {
    since: Option<String>,
    until: Option<String>,
    granularity: Option<String>,
    tz_offset: Option<i32>,
}

async fn analytics_activity_chart(
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

async fn analytics_activity() -> Result<Json<claude_data::ActivityTimeline>, (StatusCode, String)> {
    let result = tokio::task::spawn_blocking(claude_data::read_activity_timeline)
        .await
        .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
        .map_err(internal_error)?;
    Ok(Json(result))
}

async fn analytics_plugins() -> Result<Json<Vec<claude_data::PluginInfo>>, (StatusCode, String)> {
    let result = tokio::task::spawn_blocking(claude_data::read_installed_plugins)
        .await
        .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
        .map_err(internal_error)?;
    Ok(Json(result))
}

async fn analytics_active_sessions()
-> Result<Json<Vec<claude_data::ActiveSession>>, (StatusCode, String)> {
    let result = tokio::task::spawn_blocking(claude_data::read_active_sessions)
        .await
        .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
        .map_err(internal_error)?;
    Ok(Json(result))
}

async fn analytics_plans() -> Result<Json<Vec<claude_data::PlanFile>>, (StatusCode, String)> {
    let result = tokio::task::spawn_blocking(|| {
        let providers = budi_core::provider::available_providers();
        let mut all_plans = Vec::new();
        for provider in &providers {
            if let Ok(plans) = provider.discover_plans() {
                all_plans.extend(plans);
            }
        }
        // Sort by modified date (newest first), consistent with original behavior.
        all_plans.sort_by(|a, b| b.modified.cmp(&a.modified));
        Ok::<_, anyhow::Error>(all_plans)
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;
    Ok(Json(result))
}

async fn analytics_memory() -> Result<Json<Vec<claude_data::MemoryFile>>, (StatusCode, String)> {
    let result = tokio::task::spawn_blocking(claude_data::read_memory_files)
        .await
        .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
        .map_err(internal_error)?;
    Ok(Json(result))
}

async fn analytics_permissions()
-> Result<Json<claude_data::PermissionsSummary>, (StatusCode, String)> {
    let result = tokio::task::spawn_blocking(claude_data::read_permissions)
        .await
        .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
        .map_err(internal_error)?;
    Ok(Json(result))
}

#[derive(serde::Deserialize)]
struct HistoryParams {
    limit: Option<usize>,
}

async fn analytics_history(
    Query(params): Query<HistoryParams>,
) -> Result<Json<claude_data::PromptHistory>, (StatusCode, String)> {
    let limit = params.limit.unwrap_or(200);
    let result = tokio::task::spawn_blocking(move || {
        let providers = budi_core::provider::available_providers();
        let mut all_entries = Vec::new();
        for provider in &providers {
            if let Ok(entries) = provider.prompt_history(limit) {
                all_entries.extend(entries);
            }
        }
        // Sort by timestamp descending.
        all_entries.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        all_entries.truncate(limit);
        let total_count = all_entries.len() as u64;
        Ok::<_, anyhow::Error>(claude_data::PromptHistory {
            total_count,
            entries: all_entries,
        })
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;
    Ok(Json(result))
}

async fn analytics_statusline() -> Result<Json<analytics::StatuslineStats>, (StatusCode, String)> {
    let result = tokio::task::spawn_blocking(move || {
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        let today_start = chrono::Local::now()
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        let today_utc = today_start
            .and_local_timezone(chrono::Local)
            .unwrap()
            .with_timezone(&chrono::Utc)
            .to_rfc3339();
        analytics::statusline_stats(&conn, &today_utc)
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    Ok(Json(result))
}

async fn dashboard() -> impl IntoResponse {
    let html = include_str!("../static/dashboard.html");
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html)
}

fn internal_error(err: anyhow::Error) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_app() -> Router {
        build_router(AppState {
            daemon_state: DaemonState::new(),
        })
    }

    #[tokio::test]
    async fn health_returns_ok() {
        let app = test_app();
        let resp = app
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json, serde_json::json!({ "ok": true }));
    }

    #[tokio::test]
    async fn stats_returns_json() {
        let app = test_app();
        let resp = app
            .oneshot(Request::get("/stats").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json.get("queries").is_some());
    }

    #[tokio::test]
    async fn dashboard_returns_html() {
        let app = test_app();
        let resp = app
            .oneshot(Request::get("/dashboard").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(ct.contains("text/html"));
    }
}
