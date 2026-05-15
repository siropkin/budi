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
pub(crate) async fn pricing_status() -> Result<Json<Value>, (StatusCode, Json<Value>)> {
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
                    workspace_id: Some(p.workspace_id),
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
pub(crate) struct RecomputeQuery {
    #[serde(default)]
    pub force: bool,
}

/// Render a [`team_pricing::CliTickOutcome`] as the wire body returned
/// by `POST /pricing/recompute`. Extracted so the per-variant JSON
/// shapes are unit-testable without spinning up the team-pricing
/// worker (which requires a cloud config + network).
fn recompute_outcome_body(outcome: team_pricing::CliTickOutcome) -> Value {
    match outcome {
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
    }
}

/// `POST /pricing/recompute` — loopback-only; immediately re-poll the
/// team-pricing endpoint and recompute `messages.cost_cents_effective`.
///
/// Without `?force=1` this short-circuits when `list_version` is
/// unchanged (the worker would no-op anyway). With `?force=1` it skips
/// the version check and always runs a recompute pass against the
/// currently-installed list. Returns the resulting `RecomputeSummary`
/// (or `{ok: true, skipped: true}` for the short-circuit path).
pub(crate) async fn pricing_recompute(
    Query(q): Query<RecomputeQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let force = q.force;
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
        let outcome = team_pricing::run_tick_for_cli(force)?;
        Ok(recompute_outcome_body(outcome))
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

/// Render a [`pricing_refresh::RefreshReport`] as the wire body
/// returned by `POST /pricing/refresh`. The `rejected_upstream_rows`
/// key is omitted when empty (ADR-0091 §2 amendment, 8.3.1 / #483) so
/// older clients can keep deserializing the response without growing a
/// field they don't yet know about.
fn refresh_report_body(report: &pricing_refresh::RefreshReport) -> Value {
    let mut body = json!({
        "ok": true,
        "version": report.version,
        "known_model_count": report.known_model_count,
        "backfilled_rows": report.backfilled_rows,
    });
    if !report.rejected_upstream_rows.is_empty()
        && let Some(map) = body.as_object_mut()
    {
        map.insert(
            "rejected_upstream_rows".to_string(),
            serde_json::to_value(&report.rejected_upstream_rows).unwrap_or(json!([])),
        );
    }
    body
}

/// `POST /pricing/refresh` — loopback-only; fire a manual refresh tick.
pub(crate) async fn pricing_refresh() -> Result<Json<Value>, (StatusCode, Json<Value>)> {
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
        Ok(report) => Ok(Json(refresh_report_body(&report))),
        Err(e) => Err((
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "ok": false,
                "error": format!("{e:#}"),
            })),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #745: the CLI used to send `?force=0|1`, but serde's strict bool
    /// deserializer only accepts the literal strings `true` / `false`.
    /// Pin the wire contract on the daemon side so a future CLI bump
    /// can't silently regress it again.
    #[test]
    fn recompute_query_accepts_true_false_literals() {
        let true_q: RecomputeQuery =
            serde_urlencoded::from_str("force=true").expect("force=true must parse");
        assert!(true_q.force);

        let false_q: RecomputeQuery =
            serde_urlencoded::from_str("force=false").expect("force=false must parse");
        assert!(!false_q.force);

        let missing_q: RecomputeQuery =
            serde_urlencoded::from_str("").expect("missing force must default");
        assert!(!missing_q.force);
    }

    /// Numeric `0` / `1` are explicitly *not* part of the wire shape — the
    /// CLI shipped a regression in v8.4.3 doing exactly that and produced a
    /// 400 the user couldn't recover from. Keep this test failing-loud so
    /// the bad shape can't be re-introduced without an opt-in custom
    /// deserializer.
    #[test]
    fn recompute_query_rejects_numeric_bool_literals() {
        assert!(serde_urlencoded::from_str::<RecomputeQuery>("force=1").is_err());
        assert!(serde_urlencoded::from_str::<RecomputeQuery>("force=0").is_err());
    }

    // ─── #818 handler coverage tests ─────────────────────────────────────
    //
    // `routes::pricing` was at 11% line coverage on the 8.5.2 baseline
    // (#804); the query parser above covered ~13 lines and every handler
    // body was 0%. These tests exercise each handler's response shape on
    // a tempdir-scoped HOME so they stay hermetic, with the JSON-body
    // helpers (`recompute_outcome_body`, `refresh_report_body`) called
    // directly so we can lock the per-variant wire contracts without
    // needing the team-pricing worker / network upstream.

    use budi_core::pricing::RejectedUpstreamRow;
    use budi_core::pricing::team::RecomputeSummary;
    use std::sync::Mutex;

    /// Process-global `HOME` / `BUDI_HOME` are mutated to point at a
    /// throw-away tempdir below. `cargo test` runs tests in parallel by
    /// default, so without this mutex two tests would observe each
    /// other's env writes between `set_var` and `remove_var`.
    static HOME_MUTEX: Mutex<()> = Mutex::new(());

    /// RAII guard that swaps `HOME` to a fresh tempdir for the duration
    /// of one test and also clears `BUDI_HOME` (which, when set, takes
    /// precedence over `HOME` inside `budi_home_dir`). Restores the
    /// previous values on drop.
    struct HomeGuard {
        prev_home: Option<String>,
        prev_budi_home: Option<String>,
        // Owns the tempdir for the lifetime of the guard so it's not
        // GC'd while the redirected `HOME` is active. Underscore-leading
        // so the field can stay private without tripping `dead_code`.
        _tmp: tempfile::TempDir,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl HomeGuard {
        fn new() -> Self {
            let lock = HOME_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            let tmp = tempfile::tempdir().expect("tempdir for HomeGuard");
            let prev_home = std::env::var("HOME").ok();
            let prev_budi_home = std::env::var("BUDI_HOME").ok();
            unsafe { std::env::set_var("HOME", tmp.path()) };
            unsafe { std::env::remove_var("BUDI_HOME") };
            Self {
                prev_home,
                prev_budi_home,
                _tmp: tmp,
                _lock: lock,
            }
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match &self.prev_home {
                Some(h) => unsafe { std::env::set_var("HOME", h) },
                None => unsafe { std::env::remove_var("HOME") },
            }
            match &self.prev_budi_home {
                Some(h) => unsafe { std::env::set_var("BUDI_HOME", h) },
                None => unsafe { std::env::remove_var("BUDI_HOME") },
            }
        }
    }

    fn dummy_summary() -> RecomputeSummary {
        RecomputeSummary {
            list_version: 7,
            rows_processed: 12,
            rows_changed: 3,
            before_total_cents: 100.0,
            after_total_cents: 88.5,
        }
    }

    #[test]
    fn recompute_outcome_body_renders_updated_variant() {
        let body = recompute_outcome_body(team_pricing::CliTickOutcome::Updated(dummy_summary()));
        assert_eq!(body["ok"], true);
        assert_eq!(body["skipped"], false);
        assert_eq!(body["status"], "updated");
        // `summary` is the serialized `RecomputeSummary` — lock the
        // field names the CLI / dashboard depend on.
        assert_eq!(body["summary"]["list_version"], 7);
        assert_eq!(body["summary"]["rows_processed"], 12);
        assert_eq!(body["summary"]["rows_changed"], 3);
    }

    #[test]
    fn recompute_outcome_body_renders_cleared_variant() {
        let body = recompute_outcome_body(team_pricing::CliTickOutcome::Cleared(dummy_summary()));
        assert_eq!(body["status"], "cleared");
        assert_eq!(body["skipped"], false);
        assert!(body.get("summary").is_some());
    }

    #[test]
    fn recompute_outcome_body_renders_forced_variant() {
        let body = recompute_outcome_body(team_pricing::CliTickOutcome::ForcedRecompute(
            dummy_summary(),
        ));
        assert_eq!(body["status"], "forced");
        assert_eq!(body["skipped"], false);
        assert!(body.get("summary").is_some());
    }

    #[test]
    fn recompute_outcome_body_renders_unchanged_variant() {
        let body = recompute_outcome_body(team_pricing::CliTickOutcome::Unchanged);
        assert_eq!(body["ok"], true);
        assert_eq!(body["skipped"], true);
        assert_eq!(body["status"], "unchanged");
        // No `summary` on the no-op path — the CLI keys off `skipped`.
        assert!(body.get("summary").is_none());
    }

    #[test]
    fn recompute_outcome_body_renders_not_configured_variant() {
        let body = recompute_outcome_body(team_pricing::CliTickOutcome::NotConfigured);
        assert_eq!(body["skipped"], true);
        assert_eq!(body["status"], "not_configured");
        assert!(body.get("summary").is_none());
    }

    #[test]
    fn refresh_report_body_omits_rejected_rows_when_empty() {
        // ADR-0091 §2 amendment: only emit `rejected_upstream_rows` when
        // there's at least one entry, so v8.3.0 CLI consumers don't see
        // a field they don't know about. Lock that here.
        let report = pricing_refresh::RefreshReport {
            version: 5,
            known_model_count: 124,
            backfilled_rows: 0,
            rejected_upstream_rows: Vec::new(),
        };
        let body = refresh_report_body(&report);
        assert_eq!(body["ok"], true);
        assert_eq!(body["version"], 5);
        assert_eq!(body["known_model_count"], 124);
        assert_eq!(body["backfilled_rows"], 0);
        assert!(body.get("rejected_upstream_rows").is_none());
    }

    #[test]
    fn refresh_report_body_emits_rejected_rows_when_present() {
        let report = pricing_refresh::RefreshReport {
            version: 6,
            known_model_count: 250,
            backfilled_rows: 11,
            rejected_upstream_rows: vec![RejectedUpstreamRow {
                model_id: "wandb/Qwen3".to_string(),
                reason: "$100000/M exceeds sanity ceiling".to_string(),
            }],
        };
        let body = refresh_report_body(&report);
        assert_eq!(body["version"], 6);
        assert_eq!(body["backfilled_rows"], 11);
        let rejected = body
            .get("rejected_upstream_rows")
            .expect("non-empty rejected_upstream_rows must be surfaced");
        assert_eq!(rejected[0]["model_id"], "wandb/Qwen3");
        assert!(
            rejected[0]["reason"]
                .as_str()
                .unwrap()
                .contains("sanity ceiling")
        );
    }

    /// `GET /pricing/status` always includes the LiteLLM `PricingState`
    /// fields plus a `team_pricing` object — the dashboard / CLI probe
    /// `team_pricing.active` to decide whether to render the section.
    /// With a fresh tempdir HOME and no `team::install`, the active flag
    /// must be false.
    #[tokio::test]
    async fn pricing_status_returns_body_with_inactive_team_pricing_key() {
        let _guard = HomeGuard::new();
        let Json(body) = pricing_status().await.expect("status handler must succeed");
        // PricingState fields are surfaced flat at the top level.
        assert!(
            body.get("source_label").is_some(),
            "status must include `source_label` from PricingState"
        );
        // team_pricing is always present, even when inactive (#732).
        let tp = body
            .get("team_pricing")
            .expect("team_pricing key must always be present");
        assert_eq!(tp["active"], false);
    }

    /// With no `cloud.toml` configured, `team_pricing::run_tick_for_cli`
    /// returns `NotConfigured` before touching the DB or network. The
    /// handler must surface that as `{ ok: true, skipped: true, status:
    /// "not_configured" }`.
    #[tokio::test]
    async fn pricing_recompute_without_cloud_config_returns_not_configured() {
        let _guard = HomeGuard::new();
        let Json(body) = pricing_recompute(Query(RecomputeQuery { force: false }))
            .await
            .expect("recompute handler must not error on the not_configured path");
        assert_eq!(body["ok"], true);
        assert_eq!(body["skipped"], true);
        assert_eq!(body["status"], "not_configured");
    }

    /// `force=true` cannot promote a NotConfigured outcome — the worker
    /// short-circuits on missing `api_key` before evaluating `force`.
    /// Pin the same response shape on the force path so a future
    /// refactor doesn't silently start running a recompute against an
    /// unconfigured org.
    #[tokio::test]
    async fn pricing_recompute_force_without_cloud_config_still_returns_not_configured() {
        let _guard = HomeGuard::new();
        let Json(body) = pricing_recompute(Query(RecomputeQuery { force: true }))
            .await
            .expect("recompute handler must not error on the not_configured path");
        assert_eq!(body["status"], "not_configured");
    }

    /// Wire `pricing_recompute` into a router and drive a malformed
    /// query through `oneshot` to exercise the `axum::Query` extractor's
    /// 400 path. The handler body is never entered.
    #[tokio::test]
    async fn router_rejects_malformed_recompute_query_with_400() {
        use axum::Router;
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use axum::routing::post;
        use tower::ServiceExt;

        let app: Router = Router::new().route("/pricing/recompute", post(pricing_recompute));
        // `force=1` is the v8.4.3 regression shape — `serde`'s strict
        // bool deserializer rejects it, axum maps that to a 400.
        let req = Request::post("/pricing/recompute?force=1")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
