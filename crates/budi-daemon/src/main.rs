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
use chrono::Datelike;
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
    syncing: std::sync::Arc<std::sync::atomic::AtomicBool>,
    hook_sync_tx: tokio::sync::mpsc::Sender<PathBuf>,
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
        .route("/analytics/top-tools", get(analytics_top_tools))
        .route("/analytics/mcp-tools", get(analytics_mcp_tools))
        .route("/analytics/providers", get(analytics_providers))
        .route(
            "/analytics/registered-providers",
            get(analytics_registered_providers),
        )
        .route("/analytics/statusline", get(analytics_statusline))
        .route("/analytics/context-usage", get(analytics_context_usage))
        .route(
            "/analytics/interaction-modes",
            get(analytics_interaction_modes),
        )
        .route("/system/integrations", get(system_integrations))
        .route("/dashboard", get(dashboard))
        .route("/dashboard/setup", get(dashboard))
        .route("/dashboard/plans", get(dashboard))
        .route("/dashboard/prompts", get(dashboard))
        .route("/dashboard/insights", get(dashboard))
        .route("/static/dashboard.css", get(dashboard_css))
        .route("/static/dashboard.js", get(dashboard_js))
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

    let (hook_sync_tx, mut hook_sync_rx) = tokio::sync::mpsc::channel::<PathBuf>(64);

    let app_state = AppState {
        daemon_state: DaemonState::new(),
        syncing: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        hook_sync_tx,
    };

    let sync_flag = app_state.syncing.clone();

    // Debounced hook-triggered sync: when hooks fire, sync just the active transcript
    // file after a 500ms debounce to collapse rapid hook bursts.
    let hook_sync_flag = app_state.syncing.clone();
    tokio::spawn(async move {
        let mut pending_path: Option<PathBuf> = None;
        loop {
            if pending_path.is_some() {
                // We have a pending path — wait for more or timeout after 500ms
                match tokio::time::timeout(
                    std::time::Duration::from_millis(500),
                    hook_sync_rx.recv(),
                )
                .await
                {
                    Ok(Some(path)) => {
                        pending_path = Some(path);
                        continue; // reset debounce timer
                    }
                    Ok(None) => break, // channel closed
                    Err(_) => {
                        // Timeout — debounce expired, sync the file
                        let path = pending_path.take().unwrap();
                        if hook_sync_flag
                            .compare_exchange(
                                false,
                                true,
                                std::sync::atomic::Ordering::SeqCst,
                                std::sync::atomic::Ordering::SeqCst,
                            )
                            .is_ok()
                        {
                            let flag = hook_sync_flag.clone();
                            let _ = tokio::task::spawn_blocking(move || {
                                let result = (|| {
                                    let db_path = analytics::db_path().ok()?;
                                    let mut conn = analytics::open_db(&db_path).ok()?;
                                    analytics::sync_one_file(&mut conn, &path).ok()
                                })();
                                flag.store(false, std::sync::atomic::Ordering::SeqCst);
                                result
                            })
                            .await;
                        }
                    }
                }
            } else {
                // No pending path — block until we get one
                match hook_sync_rx.recv().await {
                    Some(path) => {
                        pending_path = Some(path);
                    }
                    None => break, // channel closed
                }
            }
        }
    });

    let app = build_router(app_state);

    // Auto-sync JSONL transcripts every 30 seconds to keep analytics fresh.
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        interval.tick().await; // skip immediate first tick
        loop {
            interval.tick().await;
            if sync_flag
                .compare_exchange(
                    false,
                    true,
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                )
                .is_err()
            {
                continue; // Another sync is running, skip this tick
            }
            let flag = sync_flag.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let result = (|| {
                    let db_path = analytics::db_path().ok()?;
                    let mut conn = analytics::open_db(&db_path).ok()?;
                    analytics::sync_all(&mut conn).ok()
                })();
                flag.store(false, std::sync::atomic::Ordering::SeqCst);
                result
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

    // Trigger debounced sync of this transcript file so statusline data stays fresh.
    let _ = state
        .hook_sync_tx
        .try_send(PathBuf::from(&input.common.transcript_path));

    Json(budi_core::hooks::UserPromptSubmitOutput::allow_with_context(String::new()))
}

async fn sync_analytics(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
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
        return Ok(Json(
            json!({ "files_synced": 0, "messages_ingested": 0, "skipped": "sync already running" }),
        ));
    }
    let flag = state.syncing.clone();
    let result = tokio::task::spawn_blocking(move || {
        let r = (|| -> anyhow::Result<_> {
            let db_path = analytics::db_path()?;
            let mut conn = analytics::open_db(&db_path)?;
            analytics::sync_all(&mut conn)
        })();
        flag.store(false, std::sync::atomic::Ordering::SeqCst);
        r
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
    provider: Option<String>,
}

async fn analytics_summary(
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

async fn hook_tool_use(
    State(state): State<AppState>,
    Json(input): Json<budi_core::hooks::PostToolUseInput>,
) -> Json<serde_json::Value> {
    // Trigger debounced sync so cost data from tool usage is picked up quickly.
    let _ = state
        .hook_sync_tx
        .try_send(PathBuf::from(&input.common.transcript_path));
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

#[derive(serde::Deserialize)]
struct SessionsParams {
    since: Option<String>,
    until: Option<String>,
    search: Option<String>,
    sort_by: Option<String>,
    sort_asc: Option<bool>,
    limit: Option<usize>,
    offset: Option<usize>,
}

async fn analytics_sessions(
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

#[derive(serde::Deserialize)]
struct PlansParams {
    limit: Option<usize>,
    offset: Option<usize>,
    search: Option<String>,
}

#[derive(serde::Serialize)]
struct PaginatedPlans {
    plans: Vec<claude_data::PlanFile>,
    total_count: u64,
}

async fn analytics_plans(
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
    offset: Option<usize>,
    search: Option<String>,
}

async fn analytics_history(
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

async fn analytics_providers(
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

async fn analytics_top_tools(
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

async fn analytics_mcp_tools(
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

async fn analytics_registered_providers() -> Json<serde_json::Value> {
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

async fn analytics_statusline() -> Result<Json<analytics::StatuslineStats>, (StatusCode, String)> {
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

async fn analytics_context_usage(
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

async fn analytics_interaction_modes(
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

async fn system_integrations() -> Json<serde_json::Value> {
    let result = tokio::task::spawn_blocking(|| {
        let has_starship = std::process::Command::new("which")
            .arg("starship")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());

        let starship_config_path = std::env::var("STARSHIP_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                std::env::var("XDG_CONFIG_HOME")
                    .map(|x| PathBuf::from(x).join("starship.toml"))
                    .unwrap_or_else(|_| {
                        let home = std::env::var("HOME").unwrap_or_default();
                        PathBuf::from(home).join(".config/starship.toml")
                    })
            });

        let starship_configured = has_starship
            && std::fs::read_to_string(&starship_config_path)
                .unwrap_or_default()
                .contains("[custom.budi]");

        let home = std::env::var("HOME").unwrap_or_default();
        let claude_settings = PathBuf::from(&home).join(".claude/settings.json");
        let claude_statusline = std::fs::read_to_string(&claude_settings)
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .and_then(|v| {
                v.get("statusLine")?
                    .get("command")?
                    .as_str()
                    .map(String::from)
            })
            .is_some_and(|cmd| cmd.contains("budi"));

        json!({
            "claude_code_statusline": claude_statusline,
            "starship": {
                "installed": has_starship,
                "configured": starship_configured,
            }
        })
    })
    .await
    .unwrap_or_else(|_| json!({}));

    Json(result)
}

async fn dashboard() -> impl IntoResponse {
    let html = include_str!("../static/dashboard.html");
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html)
}

async fn dashboard_css() -> impl IntoResponse {
    let css = include_str!("../static/dashboard.css");
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], css)
}

async fn dashboard_js() -> impl IntoResponse {
    let js = include_str!("../static/dashboard.js");
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        js,
    )
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
        let (hook_sync_tx, _hook_sync_rx) = tokio::sync::mpsc::channel(64);
        build_router(AppState {
            daemon_state: DaemonState::new(),
            syncing: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            hook_sync_tx,
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
