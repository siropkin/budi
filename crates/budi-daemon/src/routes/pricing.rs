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
use axum::extract::Query;
use axum::http::StatusCode;
use budi_core::analytics;
use budi_core::pricing;
use budi_core::pricing::team;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::workers::pricing_refresh;
use crate::workers::team_pricing;

/// `GET /pricing/status`
///
/// Returns the LiteLLM manifest snapshot (`PricingState`) plus a
/// `team_pricing` object surfacing the cloud-pulled price list (#732).
/// `team_pricing` is always present so JSON consumers can probe a
/// single key — `team_pricing.active == false` means "not in use", same
/// shape as the inactive path.
pub async fn pricing_status() -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let result = tokio::task::spawn_blocking(|| -> anyhow::Result<Value> {
        let state = pricing::current_state();
        let mut body = serde_json::to_value(state)?;
        let team_status = match analytics::db_path().and_then(|p| analytics::open_db(&p)) {
            Ok(conn) => {
                team::build_status(&conn).unwrap_or_else(|_| team::TeamPricingStatus::inactive())
            }
            // No DB yet (fresh install) → mirror the in-memory snapshot
            // without an audit row or savings figure.
            Err(_) => match team::snapshot() {
                Some(p) => team::TeamPricingStatus {
                    active: true,
                    org_id: Some(p.org_id),
                    list_version: Some(p.list_version),
                    effective_from: Some(p.effective_from),
                    effective_to: p.effective_to,
                    defaults: Some(p.defaults),
                    last_recompute: None,
                    savings_last_30d_cents: None,
                },
                None => team::TeamPricingStatus::inactive(),
            },
        };
        if let Some(map) = body.as_object_mut() {
            map.insert(
                "team_pricing".to_string(),
                serde_json::to_value(team_status)?,
            );
        }
        Ok(body)
    })
    .await
    .map_err(|e| super::internal_error(anyhow::anyhow!("pricing status task panicked: {e}")))?;

    match result {
        Ok(body) => Ok(Json(body)),
        Err(_) => Ok(Json(
            json!({ "ok": false, "error": "failed to serialize pricing state" }),
        )),
    }
}

#[derive(Debug, Deserialize)]
pub struct RecomputeQuery {
    #[serde(default)]
    pub force: bool,
}

/// `POST /pricing/recompute` — loopback-only; immediately re-poll the
/// team-pricing endpoint and recompute `messages.cost_cents_effective`.
///
/// Without `?force=1` this short-circuits when `list_version` is
/// unchanged (the worker would no-op anyway). With `?force=1` it skips
/// the version check and always runs a recompute pass against the
/// currently-installed list. Returns the resulting `RecomputeSummary`
/// (or `{ok: true, skipped: true}` for the short-circuit path).
pub async fn pricing_recompute(
    Query(q): Query<RecomputeQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let force = q.force;
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
        let outcome = team_pricing::run_tick_for_cli(force)?;
        let body = match outcome {
            team_pricing::CliTickOutcome::Updated(summary) => json!({
                "ok": true,
                "skipped": false,
                "status": "updated",
                "summary": summary,
            }),
            team_pricing::CliTickOutcome::Cleared(summary) => json!({
                "ok": true,
                "skipped": false,
                "status": "cleared",
                "summary": summary,
            }),
            team_pricing::CliTickOutcome::ForcedRecompute(summary) => json!({
                "ok": true,
                "skipped": false,
                "status": "forced",
                "summary": summary,
            }),
            team_pricing::CliTickOutcome::Unchanged => json!({
                "ok": true,
                "skipped": true,
                "status": "unchanged",
            }),
            team_pricing::CliTickOutcome::NotConfigured => json!({
                "ok": true,
                "skipped": true,
                "status": "not_configured",
            }),
        };
        Ok(body)
    })
    .await
    .map_err(|e| super::internal_error(anyhow::anyhow!("pricing recompute task panicked: {e}")))?;

    match result {
        Ok(body) => Ok(Json(body)),
        Err(e) => Err((
            StatusCode::BAD_GATEWAY,
            Json(json!({ "ok": false, "error": format!("{e:#}") })),
        )),
    }
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
