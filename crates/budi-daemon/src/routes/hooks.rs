use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde_json::{Value, json};

use super::internal_error;
use crate::AppState;

pub async fn health() -> Json<serde_json::Value> {
    Json(json!({ "ok": true }))
}

#[derive(serde::Deserialize, Default)]
pub struct SyncParams {
    #[serde(default)]
    pub migrate: bool,
}

pub async fn analytics_sync(
    State(state): State<AppState>,
    params: Option<Json<SyncParams>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let params = params.map(|p| p.0).unwrap_or_default();
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
            let mut conn = if params.migrate {
                budi_core::analytics::open_db_with_migration(&db_path)?
            } else {
                let c = budi_core::analytics::open_db(&db_path)?;
                if budi_core::migration::needs_migration(&c) {
                    anyhow::bail!("Database needs migration. Use migrate=true or run `budi migrate`.");
                }
                c
            };
            let (files_synced, messages_ingested) = budi_core::analytics::sync_all(&mut conn)?;
            Ok(json!({
                "files_synced": files_synced,
                "messages_ingested": messages_ingested,
            }))
        })();
        flag.store(false, std::sync::atomic::Ordering::SeqCst);
        r
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    Ok(Json(result))
}

pub async fn analytics_history(
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
            let mut conn = budi_core::analytics::open_db_with_migration(&db_path)?;
            let (files_synced, messages_ingested) = budi_core::analytics::sync_history(&mut conn)?;
            Ok(json!({
                "files_synced": files_synced,
                "messages_ingested": messages_ingested,
            }))
        })();
        flag.store(false, std::sync::atomic::Ordering::SeqCst);
        r
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    Ok(Json(result))
}

// ---------------------------------------------------------------------------
// Hook event ingestion
// ---------------------------------------------------------------------------

pub async fn hooks_ingest(
    Json(payload): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    tokio::task::spawn_blocking(move || {
        let event = budi_core::hooks::parse_hook_event(&payload)?;

        let db_path = budi_core::analytics::db_path()?;
        let conn = budi_core::analytics::open_db(&db_path)?;

        // If prompt submission, classify and update session
        if matches!(event.event.as_str(), "user_prompt_submit") {
            if let Some(prompt) = payload
                .get("user_prompt")
                .or_else(|| payload.get("prompt"))
                .and_then(|v| v.as_str())
            {
                if let Some(category) = budi_core::hooks::classify_prompt(prompt) {
                    let _ = budi_core::hooks::update_session_category(&conn, &event, &category);
                }
            }
        }

        budi_core::hooks::upsert_session(&conn, &event)?;
        budi_core::hooks::ingest_hook_event(&conn, &event)?;
        Ok::<_, anyhow::Error>(())
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    Ok(Json(json!({"ok": true})))
}

// ---------------------------------------------------------------------------
// Session & tool analytics endpoints
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
pub struct SessionsParams {
    pub since: Option<String>,
    pub until: Option<String>,
    pub limit: Option<usize>,
}

pub async fn analytics_sessions(
    Query(params): Query<SessionsParams>,
) -> Result<Json<Vec<budi_core::hooks::SessionStats>>, (StatusCode, String)> {
    let result = tokio::task::spawn_blocking(move || {
        let db_path = budi_core::analytics::db_path()?;
        let conn = budi_core::analytics::open_db(&db_path)?;
        budi_core::hooks::query_sessions(
            &conn,
            params.since.as_deref(),
            params.until.as_deref(),
            params.limit.unwrap_or(100),
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    Ok(Json(result))
}

#[derive(serde::Deserialize)]
pub struct ToolsParams {
    pub since: Option<String>,
    pub until: Option<String>,
    pub limit: Option<usize>,
}

pub async fn analytics_tools(
    Query(params): Query<ToolsParams>,
) -> Result<Json<Vec<budi_core::hooks::ToolStats>>, (StatusCode, String)> {
    let result = tokio::task::spawn_blocking(move || {
        let db_path = budi_core::analytics::db_path()?;
        let conn = budi_core::analytics::open_db(&db_path)?;
        budi_core::hooks::query_tool_stats(
            &conn,
            params.since.as_deref(),
            params.until.as_deref(),
            params.limit.unwrap_or(50),
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    Ok(Json(result))
}

#[derive(serde::Deserialize)]
pub struct McpParams {
    pub since: Option<String>,
    pub until: Option<String>,
    pub limit: Option<usize>,
}

pub async fn analytics_mcp(
    Query(params): Query<McpParams>,
) -> Result<Json<Vec<budi_core::hooks::McpStats>>, (StatusCode, String)> {
    let result = tokio::task::spawn_blocking(move || {
        let db_path = budi_core::analytics::db_path()?;
        let conn = budi_core::analytics::open_db(&db_path)?;
        budi_core::hooks::query_mcp_stats(
            &conn,
            params.since.as_deref(),
            params.until.as_deref(),
            params.limit.unwrap_or(50),
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    Ok(Json(result))
}
