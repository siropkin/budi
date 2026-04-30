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
        // #521: spell out which field is missing and where to find its
        // value so a fresh user can complete the flow without reading
        // the ADR. Pre-fix the operator saw a generic "ensure api_key,
        // device_id, and org_id are set" line that listed every
        // possible gap.
        let missing = missing_fields_message(&cfg);
        return Ok(Json(not_ready_body(RESULT_NOT_CONFIGURED, &cfg, &missing)));
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

/// `POST /cloud/reset` — drop the cloud-sync watermarks so the next
/// sync re-uploads every local rollup + session summary (#564).
///
/// User-driven escape hatch for org switches, device_id rotations, and
/// cloud-side data wipes that leave the daemon's incremental watermark
/// "ahead" of where the cloud actually is. Cloud-side dedup
/// (ADR-0083 §6) keeps the re-upload safe even when records overlap
/// with rows the cloud already has.
///
/// This route is loopback-protected (see `build_router` in `main.rs`)
/// because it mutates `sync_state`. We grab the same `cloud_syncing`
/// busy flag as `/cloud/sync` so a manual reset can never race a
/// background tick that already built an envelope against the
/// soon-to-be-deleted watermark — that would push under the old
/// watermark, then the reset would land, then the next tick would
/// re-push everything anyway. Holding the flag keeps the sequencing
/// honest. Returns 409 when the worker (or another `cloud sync`) is
/// already running so the operator can re-run after it finishes.
pub async fn cloud_reset(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
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
                "error": "cloud sync already running — wait for it to finish, then re-run `budi cloud reset`",
                "result": "busy"
            })),
        ));
    }

    let flag = state.cloud_syncing.clone();
    let removed = tokio::task::spawn_blocking(move || -> anyhow::Result<usize> {
        let _guard = CloudBusyFlagGuard::new(flag);
        let db_path = analytics::db_path()?;
        let conn = analytics::open_db(&db_path)?;
        cloud_sync::reset_cloud_watermarks(&conn)
    })
    .await
    .map_err(|e| super::internal_error(anyhow::anyhow!("cloud reset task panicked: {e}")))?
    .map_err(super::internal_error)?;

    let cfg = config::load_cloud_config();
    Ok(Json(json!({
        "ok": true,
        "result": "reset",
        "endpoint": cfg.effective_endpoint(),
        "org_id": cfg.org_id,
        "removed": removed,
        "message": "Cloud sync watermarks reset. Run `budi cloud sync` to push everything now, or wait for the next interval tick.",
    })))
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

/// #521: enumerate which `[cloud]` fields are still missing and
/// point each missing field at the concrete action the operator
/// needs to take. Returned as a single user-facing line so both
/// `/cloud/status` and `/cloud/sync` surface the same prose.
fn missing_fields_message(cfg: &CloudConfig) -> String {
    let mut problems: Vec<String> = Vec::new();
    if cfg.effective_api_key().is_none() {
        problems
            .push("`api_key` — paste from https://app.getbudi.dev/dashboard/settings".to_string());
    } else if cfg
        .effective_api_key()
        .as_deref()
        .map(|k| k == config::CLOUD_API_KEY_STUB)
        .unwrap_or(false)
    {
        problems.push(
            "`api_key` — still the placeholder; paste your real key from https://app.getbudi.dev/dashboard/settings"
                .to_string(),
        );
    }
    if cfg.device_id.is_none() {
        problems.push(
            "`device_id` — run `budi init` to auto-generate a UUID, or set any stable string"
                .to_string(),
        );
    }
    if cfg.org_id.is_none() {
        problems.push(
            "`org_id` — copy from the Organization panel at https://app.getbudi.dev/dashboard/settings"
                .to_string(),
        );
    }
    if problems.is_empty() {
        // Defensive: `is_ready()` was false so something must be missing
        // — fall back to a generic line rather than returning an empty
        // string that would read as no-message.
        return "Cloud sync is not fully configured. Check ~/.config/budi/cloud.toml.".to_string();
    }
    format!(
        "Cloud sync is not fully configured. Missing:\n  - {}\nAfter editing ~/.config/budi/cloud.toml, re-run `budi cloud status`.",
        problems.join("\n  - ")
    )
}

fn report_to_json(report: SyncTickReport) -> Value {
    let SyncTickReport {
        result,
        endpoint,
        envelope_rollups,
        envelope_sessions,
        server_records_upserted,
        server_watermark,
        chunks_total,
        chunks_succeeded,
    } = report;

    let (ok, result_tag, message) = match &result {
        SyncResult::Success(_) if chunks_total > 0 && chunks_succeeded < chunks_total => (
            true,
            RESULT_SUCCESS,
            format!(
                "Cloud sync partially complete: {chunks_succeeded}/{chunks_total} chunks confirmed. Re-run `budi cloud sync` to push the rest."
            ),
        ),
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
        SyncResult::SchemaMismatch(msg) if chunks_succeeded > 0 => (
            false,
            RESULT_SCHEMA_MISMATCH,
            format!(
                "Server rejected chunk {} of {chunks_total} as schema-incompatible (422) after confirming {chunks_succeeded}. Update budi to resume syncing. Detail: {msg}",
                chunks_succeeded + 1,
            ),
        ),
        SyncResult::SchemaMismatch(msg) => (
            false,
            RESULT_SCHEMA_MISMATCH,
            format!(
                "Server rejected the payload as schema-incompatible (422). Update budi to resume syncing. Detail: {msg}"
            ),
        ),
        SyncResult::TransientError(msg) if chunks_succeeded > 0 => (
            false,
            RESULT_TRANSIENT_ERROR,
            format!(
                "Cloud sync hit a transient error on chunk {} of {chunks_total} after confirming {chunks_succeeded}: {msg}",
                chunks_succeeded + 1,
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
        "chunks_total": chunks_total,
        "chunks_succeeded": chunks_succeeded,
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
            chunks_total: 1,
            chunks_succeeded: 1,
        };
        let body = report_to_json(report);
        assert_eq!(body["ok"], true);
        assert_eq!(body["result"], RESULT_SUCCESS);
        assert_eq!(body["records_upserted"], 5);
        assert_eq!(body["watermark"], "2026-04-17");
        assert_eq!(body["chunks_total"], 1);
        assert_eq!(body["chunks_succeeded"], 1);
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
            chunks_total: 0,
            chunks_succeeded: 0,
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
            chunks_total: 1,
            chunks_succeeded: 0,
        };
        let body = report_to_json(report);
        assert_eq!(body["ok"], false);
        assert_eq!(body["result"], RESULT_AUTH_FAILURE);
        assert_eq!(body["rollups_attempted"], 3);
        assert_eq!(body["sessions_attempted"], 4);
    }

    #[test]
    fn report_to_json_transient_after_partial_success_includes_progress() {
        // #572: partial-success message must tell the operator how
        // many chunks landed so re-run is obviously the next step.
        let report = SyncTickReport {
            result: SyncResult::TransientError("Server returned 413".into()),
            endpoint: "https://app.getbudi.dev".into(),
            envelope_rollups: 1500,
            envelope_sessions: 800,
            server_records_upserted: Some(1000),
            server_watermark: Some("2026-04-15".into()),
            chunks_total: 5,
            chunks_succeeded: 2,
        };
        let body = report_to_json(report);
        assert_eq!(body["ok"], false);
        assert_eq!(body["result"], RESULT_TRANSIENT_ERROR);
        assert_eq!(body["chunks_total"], 5);
        assert_eq!(body["chunks_succeeded"], 2);
        let msg = body["message"].as_str().unwrap();
        assert!(msg.contains("chunk 3 of 5"), "got: {msg}");
        assert!(msg.contains("after confirming 2"), "got: {msg}");
    }
}
