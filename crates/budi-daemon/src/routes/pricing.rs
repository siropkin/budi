//! Pricing manifest status + refresh endpoints (ADR-0091 §8).
//!
//! `GET /pricing/status` — read-only snapshot of the current manifest
//! state (source layer, version, fetched-at, known model count, unknown
//! model tally). Safe to call from the dashboard and the CLI on any
//! cadence; no I/O beyond grabbing the pricing `RwLock` read guard.
//!
//! `POST /pricing/refresh` — loopback-only, triggers an immediate refresh
//! tick through the same code path as the periodic worker (see
//! [`crate::workers::pricing_refresh::run_tick`]). Returns the resulting
//! [`RefreshReport`] on success or a 502 with the validation / network
//! error on failure. Does not schedule the next periodic tick — the
//! worker's 24 h loop keeps running independently.

use axum::Json;
use axum::http::StatusCode;
use budi_core::analytics;
use budi_core::pricing;
use serde_json::{Value, json};

use crate::workers::pricing_refresh;

/// `GET /pricing/status`
pub async fn pricing_status() -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let state = tokio::task::spawn_blocking(pricing::current_state)
        .await
        .map_err(|e| super::internal_error(anyhow::anyhow!("pricing status task panicked: {e}")))?;
    Ok(Json(serde_json::to_value(state).unwrap_or_else(
        |_| json!({ "ok": false, "error": "failed to serialize pricing state" }),
    )))
}

/// `POST /pricing/refresh` — loopback-only; fire a manual refresh tick.
pub async fn pricing_refresh() -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let result =
        tokio::task::spawn_blocking(|| -> anyhow::Result<pricing_refresh::RefreshReport> {
            let db_path = analytics::db_path()?;
            pricing_refresh::run_tick(&db_path)
        })
        .await
        .map_err(|e| {
            super::internal_error(anyhow::anyhow!("pricing refresh task panicked: {e}"))
        })?;

    match result {
        Ok(report) => {
            let mut body = json!({
                "ok": true,
                "version": report.version,
                "known_model_count": report.known_model_count,
                "backfilled_rows": report.backfilled_rows,
            });
            // ADR-0091 §2 amendment (8.3.1 / #483): surface row-level
            // rejections on the refresh response so `budi pricing
            // status --refresh` can print them on the spot.
            // `skip-if-none` for older-client compatibility.
            if !report.rejected_upstream_rows.is_empty()
                && let Some(map) = body.as_object_mut()
            {
                map.insert(
                    "rejected_upstream_rows".to_string(),
                    serde_json::to_value(&report.rejected_upstream_rows).unwrap_or(json!([])),
                );
            }
            Ok(Json(body))
        }
        Err(e) => Err((
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "ok": false,
                "error": format!("{e:#}"),
            })),
        )),
    }
}
