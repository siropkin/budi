use std::path::PathBuf;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use budi_core::config;
use budi_core::hooks::UserPromptSubmitInput;
use budi_core::pre_filter;
use budi_core::rpc::{StatusRequest, StatusResponse};
use serde_json::json;

use crate::AppState;
use super::internal_error;

pub async fn health() -> Json<serde_json::Value> {
    Json(json!({ "ok": true }))
}

#[derive(serde::Deserialize)]
pub struct StatsParams {
    repo_root: Option<String>,
}

pub async fn hook_stats(
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

pub async fn hook_session_stats(
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

pub async fn hook_prompt_submit(
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

pub async fn hook_tool_use(
    State(state): State<AppState>,
    Json(input): Json<budi_core::hooks::PostToolUseInput>,
) -> Json<serde_json::Value> {
    // Trigger debounced sync so cost data from tool usage is picked up quickly.
    let _ = state
        .hook_sync_tx
        .try_send(PathBuf::from(&input.common.transcript_path));
    Json(json!({}))
}

pub async fn status_repo(
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

fn request_config(repo_root: &str) -> anyhow::Result<budi_core::config::BudiConfig> {
    let root = std::path::Path::new(repo_root);
    config::load_or_default(root)
}

pub async fn analytics_sync(
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
            let db_path = budi_core::analytics::db_path()?;
            let mut conn = budi_core::analytics::open_db(&db_path)?;
            budi_core::analytics::sync_all(&mut conn)
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
