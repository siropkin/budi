use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::json;

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
