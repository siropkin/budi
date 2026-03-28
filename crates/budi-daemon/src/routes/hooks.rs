use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Value, json};

use super::internal_error;
use crate::AppState;

#[derive(serde::Serialize)]
pub struct HealthResponse {
    pub ok: bool,
    pub version: &'static str,
}

#[derive(serde::Serialize)]
pub struct SyncResponse {
    pub files_synced: usize,
    pub messages_ingested: usize,
    pub warnings: Vec<String>,
}

#[derive(serde::Serialize)]
pub struct SyncStatusResponse {
    pub syncing: bool,
    pub last_synced: Option<String>,
}

pub async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        ok: true,
        version: env!("CARGO_PKG_VERSION"),
    })
}

pub async fn health_check_update()
-> Result<Json<super::analytics::CheckUpdateResponse>, (StatusCode, Json<serde_json::Value>)> {
    use super::analytics::CheckUpdateResponse;

    let result = tokio::task::spawn_blocking(|| -> anyhow::Result<CheckUpdateResponse> {
        let current = env!("CARGO_PKG_VERSION").to_string();
        let output = std::process::Command::new("curl")
            .args([
                "-sf",
                "--max-time",
                "10",
                "-H",
                &format!("User-Agent: budi/{current}"),
                "https://api.github.com/repos/siropkin/budi/releases/latest",
            ])
            .output()?;

        if !output.status.success() {
            return Ok(CheckUpdateResponse {
                current,
                latest: None,
                up_to_date: None,
                error: Some("Could not reach GitHub API".to_string()),
            });
        }

        let release: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        let latest = release
            .get("tag_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim_start_matches('v')
            .to_string();
        let up_to_date = latest == current;
        Ok(CheckUpdateResponse {
            current,
            latest: Some(latest),
            up_to_date: Some(up_to_date),
            error: None,
        })
    })
    .await
    .map_err(|e| super::internal_error(anyhow::anyhow!("{e}")))?
    .map_err(super::internal_error)?;
    Ok(Json(result))
}

pub async fn health_integrations()
-> Result<Json<super::analytics::IntegrationsResponse>, (StatusCode, Json<serde_json::Value>)> {
    use super::analytics::{DatabaseStats, IntegrationPaths, IntegrationsResponse};

    let result = tokio::task::spawn_blocking(|| -> IntegrationsResponse {
        let home = budi_core::config::home_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();

        // Check Claude Code settings
        let claude_path = format!("{home}/.claude/settings.json");
        let claude_settings: Option<serde_json::Value> = std::fs::read_to_string(&claude_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok());

        let hooks_installed = claude_settings
            .as_ref()
            .and_then(|s| s.get("hooks"))
            .map(|h| !h.as_object().map(|o| o.is_empty()).unwrap_or(true))
            .unwrap_or(false);

        let mcp_installed = claude_settings
            .as_ref()
            .and_then(|s| s.get("mcpServers"))
            .and_then(|m| m.get("budi"))
            .is_some();

        let otel_installed = claude_settings
            .as_ref()
            .and_then(|s| s.get("env"))
            .and_then(|e| e.get("OTEL_EXPORTER_OTLP_ENDPOINT"))
            .is_some();

        let statusline_installed = claude_settings
            .as_ref()
            .and_then(|s| s.get("statusLine"))
            .and_then(|sl| sl.get("command"))
            .and_then(|c| c.as_str())
            .map(|c| c.contains("budi"))
            .unwrap_or(false);

        // Cursor hooks
        let cursor_path = format!("{home}/.cursor/hooks.json");
        let cursor_hooks = std::fs::read_to_string(&cursor_path)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| v.get("hooks").cloned())
            .map(|h| !h.as_object().map(|o| o.is_empty()).unwrap_or(true))
            .unwrap_or(false);

        // DB stats + paths
        let db_path_str = budi_core::analytics::db_path()
            .ok()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let db_stats = budi_core::analytics::db_path()
            .ok()
            .and_then(|p| {
                let size_mb = std::fs::metadata(&p)
                    .ok()
                    .map(|m| m.len() as f64 / 1_048_576.0);
                let conn = budi_core::analytics::open_db(&p).ok()?;
                let msg_count: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM messages WHERE role = 'assistant'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                let first_record: Option<String> = conn
                    .query_row(
                        "SELECT MIN(timestamp) FROM messages WHERE role = 'assistant'",
                        [],
                        |r| r.get(0),
                    )
                    .ok()
                    .flatten();
                Some(DatabaseStats {
                    size_mb: (size_mb.unwrap_or(0.0) * 10.0).round() / 10.0,
                    records: msg_count,
                    first_record,
                })
            })
            .unwrap_or(DatabaseStats {
                size_mb: 0.0,
                records: 0,
                first_record: None,
            });

        let config_dir = budi_core::config::budi_home_dir()
            .ok()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();

        IntegrationsResponse {
            claude_code_hooks: hooks_installed,
            cursor_hooks,
            mcp_server: mcp_installed,
            otel: otel_installed,
            statusline: statusline_installed,
            database: db_stats,
            paths: IntegrationPaths {
                database: db_path_str,
                config: config_dir,
                claude_settings: claude_path,
                cursor_hooks: cursor_path,
            },
        }
    })
    .await
    .map_err(|e| super::internal_error(anyhow::anyhow!("{e}")))?;

    Ok(Json(result))
}

pub async fn sync_status(State(state): State<AppState>) -> Json<SyncStatusResponse> {
    let syncing = state.syncing.load(std::sync::atomic::Ordering::Acquire);
    let last_synced = tokio::task::spawn_blocking(|| {
        let db_path = budi_core::analytics::db_path().ok()?;
        let conn = budi_core::analytics::open_db(&db_path).ok()?;
        conn.query_row("SELECT MAX(last_synced) FROM sync_state", [], |r| {
            r.get::<_, Option<String>>(0)
        })
        .ok()
        .flatten()
    })
    .await
    .ok()
    .flatten();
    Json(SyncStatusResponse {
        syncing,
        last_synced,
    })
}

#[derive(serde::Deserialize, Default)]
pub struct SyncParams {
    #[serde(default)]
    pub migrate: bool,
}

pub async fn analytics_sync(
    State(state): State<AppState>,
    body: Option<Json<SyncParams>>,
) -> Result<Json<SyncResponse>, (StatusCode, Json<serde_json::Value>)> {
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
            Ok(SyncResponse {
                files_synced,
                messages_ingested,
                warnings,
            })
        })();
        flag.store(false, std::sync::atomic::Ordering::SeqCst);
        r
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    Ok(Json(result))
}

pub async fn analytics_sync_reset(
    State(state): State<AppState>,
) -> Result<Json<SyncResponse>, (StatusCode, Json<serde_json::Value>)> {
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
            budi_core::analytics::reset_sync_state(&conn)?;
            let (files_synced, messages_ingested, warnings) =
                budi_core::analytics::sync_history(&mut conn)?;
            Ok(SyncResponse {
                files_synced,
                messages_ingested,
                warnings,
            })
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
) -> Result<Json<SyncResponse>, (StatusCode, Json<serde_json::Value>)> {
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
            Ok(SyncResponse {
                files_synced,
                messages_ingested,
                warnings,
            })
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
