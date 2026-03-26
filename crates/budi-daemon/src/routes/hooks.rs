use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde_json::{Value, json};

use super::internal_error;
use crate::AppState;

pub async fn health() -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    Ok(Json(
        json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") }),
    ))
}

pub async fn sync_status(State(state): State<AppState>) -> Json<serde_json::Value> {
    let syncing = state.syncing.load(std::sync::atomic::Ordering::Acquire);
    Json(json!({ "syncing": syncing }))
}

#[derive(serde::Deserialize, Default)]
pub struct SyncParams {
    #[serde(default)]
    pub migrate: bool,
}

pub async fn analytics_sync(
    State(state): State<AppState>,
    body: Option<Json<SyncParams>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let params = body.map(|Json(p)| p).unwrap_or_default();
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
            Json(json!({ "ok": false, "error": "sync already running" })),
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
                    anyhow::bail!(
                        "Database needs migration. Use migrate=true or run `budi migrate`."
                    );
                }
                c
            };
            let (files_synced, messages_ingested, warnings) =
                budi_core::analytics::sync_all(&mut conn)?;
            Ok(json!({
                "files_synced": files_synced,
                "messages_ingested": messages_ingested,
                "warnings": warnings,
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
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
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
            Json(json!({ "ok": false, "error": "sync already running" })),
        ));
    }
    let flag = state.syncing.clone();
    let result = tokio::task::spawn_blocking(move || {
        let r = (|| -> anyhow::Result<_> {
            let db_path = budi_core::analytics::db_path()?;
            let mut conn = budi_core::analytics::open_db_with_migration(&db_path)?;
            let (files_synced, messages_ingested, warnings) =
                budi_core::analytics::sync_history(&mut conn)?;
            Ok(json!({
                "files_synced": files_synced,
                "messages_ingested": messages_ingested,
                "warnings": warnings,
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
//
// This endpoint opens its own SQLite connection via `open_db`, which is safe
// to run concurrently with the background sync.  SQLite in WAL mode allows
// concurrent readers, and write serialization is handled by SQLite's internal
// locking (SQLITE_BUSY with a timeout configured via `busy_timeout`).  The
// `syncing` AtomicBool only guards against *duplicate* long-running syncs;
// hook ingestion writes are small and fast, so the SQLite-level lock is
// sufficient to prevent data corruption.
// ---------------------------------------------------------------------------

pub async fn hooks_ingest(
    State(state): State<AppState>,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, (StatusCode, Json<serde_json::Value>)> {
    if state.syncing.load(std::sync::atomic::Ordering::Acquire) {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "ok": false, "error": "Sync in progress, try again shortly" })),
        ));
    }
    tokio::task::spawn_blocking(move || {
        let event = budi_core::hooks::parse_hook_event(&payload)?;

        let db_path = budi_core::analytics::db_path()?;
        let mut conn = budi_core::analytics::open_db(&db_path)?;

        let tx = conn.transaction()?;

        // If prompt submission, classify and update session
        if matches!(event.event.as_str(), "user_prompt_submit")
            && let Some(prompt) = payload
                .get("user_prompt")
                .or_else(|| payload.get("prompt"))
                .and_then(|v| v.as_str())
            && let Some(category) = budi_core::hooks::classify_prompt(prompt)
        {
            let _ = budi_core::hooks::update_session_category(&tx, &event, &category);
        }

        budi_core::hooks::upsert_session(&tx, &event)?;
        budi_core::hooks::ingest_hook_event(&tx, &event)?;

        tx.commit()?;
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

#[derive(Debug, serde::Deserialize)]
pub struct ListParams {
    pub since: Option<String>,
    pub until: Option<String>,
    pub limit: Option<usize>,
}

pub async fn analytics_tools(
    Query(params): Query<ListParams>,
) -> Result<Json<Vec<budi_core::hooks::ToolStats>>, (StatusCode, Json<serde_json::Value>)> {
    let result = tokio::task::spawn_blocking(move || {
        let db_path = budi_core::analytics::db_path()?;
        let conn = budi_core::analytics::open_db(&db_path)?;
        budi_core::hooks::query_tool_stats(
            &conn,
            params.since.as_deref(),
            params.until.as_deref(),
            params.limit.unwrap_or(20),
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
        let db_path = budi_core::analytics::db_path()?;
        let conn = budi_core::analytics::open_db(&db_path)?;
        budi_core::hooks::query_mcp_stats(
            &conn,
            params.since.as_deref(),
            params.until.as_deref(),
            params.limit.unwrap_or(20),
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    Ok(Json(result))
}
