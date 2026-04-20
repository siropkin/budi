//! Cloud sync management endpoints.
//!
//! `GET /cloud/status` reports whether cloud sync is enabled and when it last
//! ran, based on the local `cloud.toml` and the watermark rows in the
//! `sync_state` table. It never makes a network call.
//!
//! `POST /cloud/sync` triggers an immediate cloud flush — the same work the
//! background worker does on its interval (ADR-0083 §9), just user-driven.
//! It is loopback-protected (see `build_router` in `main.rs`) and guarded
//! against concurrent runs via `AppState::cloud_syncing`. See issue #225
//! (R2.1) for the CLI contract this endpoint backs (`budi cloud sync` /
//! `budi cloud status`).

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use budi_core::analytics;
use budi_core::cloud_sync::{self, SyncResult, SyncTickReport};
use budi_core::config::{self, CloudConfig};
use serde_json::{Value, json};

use crate::AppState;
use crate::workers::cloud_sync::CloudBusyFlagGuard;

/// Variants are snake_case so CLI / dashboard consumers can switch on a
/// stable string rather than parsing free-form error messages. Mirrors the
/// [`SyncResult`] taxonomy plus the two "pre-network" states the manual
/// path can observe before ever touching the wire.
const RESULT_SUCCESS: &str = "success";
const RESULT_EMPTY_PAYLOAD: &str = "empty_payload";
const RESULT_AUTH_FAILURE: &str = "auth_failure";
const RESULT_SCHEMA_MISMATCH: &str = "schema_mismatch";
const RESULT_TRANSIENT_ERROR: &str = "transient_error";
const RESULT_NOT_CONFIGURED: &str = "not_configured";
const RESULT_DISABLED: &str = "disabled";

/// `GET /cloud/status` — report cloud sync readiness and freshness.
/// No network call; reads `cloud.toml` and the local watermarks.
pub async fn cloud_status() -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let snapshot = tokio::task::spawn_blocking(read_status_snapshot)
        .await
        .map_err(|e| super::internal_error(anyhow::anyhow!("cloud status task panicked: {e}")))?;
    Ok(Json(serde_json::to_value(snapshot).unwrap_or_else(
        |_| json!({ "ok": false, "error": "failed to serialize cloud status" }),
    )))
}

fn read_status_snapshot() -> budi_core::cloud_sync::CloudSyncStatus {
    let db_path = analytics::db_path().unwrap_or_default();
    let cfg = config::load_cloud_config();
    cloud_sync::current_cloud_status(&db_path, &cfg)
}

/// `POST /cloud/sync` — flush the pending cloud queue now.
///
/// Returns 409 if another cloud sync is already running (either from the
/// background worker or a prior CLI invocation).
pub async fn cloud_sync(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let cfg = config::load_cloud_config();

    // Short-circuit without taking the busy flag so repeated `budi cloud
    // sync` against a not-configured daemon is cheap and never blocks the
    // background worker.
    if !cfg.effective_enabled() {
        return Ok(Json(not_ready_body(
            RESULT_DISABLED,
            &cfg,
            "Cloud sync is not enabled. Set `enabled = true` in ~/.config/budi/cloud.toml to opt in.",
        )));
    }
    if !cfg.is_ready() {
        return Ok(Json(not_ready_body(
            RESULT_NOT_CONFIGURED,
            &cfg,
            "Cloud sync is not fully configured. Ensure api_key, device_id, and org_id are set in ~/.config/budi/cloud.toml.",
        )));
    }

    if state
        .cloud_syncing
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
            Json(json!({
                "ok": false,
                "error": "cloud sync already running",
                "result": "busy"
            })),
        ));
    }

    let flag = state.cloud_syncing.clone();
    let report = tokio::task::spawn_blocking(move || {
        let _guard = CloudBusyFlagGuard::new(flag);
        let db_path = analytics::db_path().unwrap_or_default();
        cloud_sync::sync_tick_report(&db_path, &cfg)
    })
    .await
    .map_err(|e| super::internal_error(anyhow::anyhow!("cloud sync task panicked: {e}")))?;

    Ok(Json(report_to_json(report)))
}

fn not_ready_body(result: &str, cfg: &CloudConfig, message: &str) -> Value {
    json!({
        "ok": false,
        "result": result,
        "endpoint": cfg.effective_endpoint(),
        "message": message,
        "records_upserted": 0,
        "rollups_attempted": 0,
        "sessions_attempted": 0,
    })
}

fn report_to_json(report: SyncTickReport) -> Value {
    let SyncTickReport {
        result,
        endpoint,
        envelope_rollups,
        envelope_sessions,
        server_records_upserted,
        server_watermark,
    } = report;

    let (ok, result_tag, message) = match &result {
        SyncResult::Success(_) => (
            true,
            RESULT_SUCCESS,
            "Cloud sync completed successfully.".to_string(),
        ),
        SyncResult::EmptyPayload => (
            true,
            RESULT_EMPTY_PAYLOAD,
            "Nothing new to sync — local and cloud are already in lockstep.".to_string(),
        ),
        SyncResult::AuthFailure => (
            false,
            RESULT_AUTH_FAILURE,
            "Authentication failed (401). Check `api_key` in ~/.config/budi/cloud.toml."
                .to_string(),
        ),
        SyncResult::SchemaMismatch(msg) => (
            false,
            RESULT_SCHEMA_MISMATCH,
            format!(
                "Server rejected the payload as schema-incompatible (422). Update budi to resume syncing. Detail: {msg}"
            ),
        ),
        SyncResult::TransientError(msg) => (
            false,
            RESULT_TRANSIENT_ERROR,
            format!("Cloud sync hit a transient error: {msg}"),
        ),
    };

    json!({
        "ok": ok,
        "result": result_tag,
        "endpoint": endpoint,
        "message": message,
        "records_upserted": server_records_upserted.unwrap_or(0),
        "rollups_attempted": envelope_rollups,
        "sessions_attempted": envelope_sessions,
        "watermark": server_watermark,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_ready_body_tags_result_and_endpoint() {
        let cfg = CloudConfig::default();
        let body = not_ready_body(RESULT_DISABLED, &cfg, "off");
        assert_eq!(body["ok"], false);
        assert_eq!(body["result"], RESULT_DISABLED);
        assert_eq!(body["records_upserted"], 0);
        assert_eq!(body["rollups_attempted"], 0);
        assert_eq!(body["sessions_attempted"], 0);
    }

    #[test]
    fn report_to_json_success_reports_upsert_count() {
        let report = SyncTickReport {
            result: SyncResult::Success(budi_core::cloud_sync::IngestResponse {
                accepted: true,
                watermark: Some("2026-04-17".into()),
                records_upserted: Some(5),
            }),
            endpoint: "https://app.getbudi.dev".into(),
            envelope_rollups: 5,
            envelope_sessions: 0,
            server_records_upserted: Some(5),
            server_watermark: Some("2026-04-17".into()),
        };
        let body = report_to_json(report);
        assert_eq!(body["ok"], true);
        assert_eq!(body["result"], RESULT_SUCCESS);
        assert_eq!(body["records_upserted"], 5);
        assert_eq!(body["watermark"], "2026-04-17");
    }

    #[test]
    fn report_to_json_empty_is_still_ok() {
        let report = SyncTickReport {
            result: SyncResult::EmptyPayload,
            endpoint: "https://app.getbudi.dev".into(),
            envelope_rollups: 0,
            envelope_sessions: 0,
            server_records_upserted: None,
            server_watermark: None,
        };
        let body = report_to_json(report);
        assert_eq!(body["ok"], true);
        assert_eq!(body["result"], RESULT_EMPTY_PAYLOAD);
    }

    #[test]
    fn report_to_json_auth_failure_is_not_ok() {
        let report = SyncTickReport {
            result: SyncResult::AuthFailure,
            endpoint: "https://app.getbudi.dev".into(),
            envelope_rollups: 3,
            envelope_sessions: 4,
            server_records_upserted: None,
            server_watermark: None,
        };
        let body = report_to_json(report);
        assert_eq!(body["ok"], false);
        assert_eq!(body["result"], RESULT_AUTH_FAILURE);
        assert_eq!(body["rollups_attempted"], 3);
        assert_eq!(body["sessions_attempted"], 4);
    }
}
