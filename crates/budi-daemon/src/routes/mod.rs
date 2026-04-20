pub mod analytics;
pub mod cloud;
pub mod hooks;

use std::net::SocketAddr;

use axum::Json;
use axum::extract::{ConnectInfo, Request};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;

pub fn internal_error(err: anyhow::Error) -> (StatusCode, Json<serde_json::Value>) {
    // Log the full `anyhow` chain (with `{:#}`) so `daemon.log` captures the
    // root cause for ops, even though the HTTP response stays opaque on
    // purpose (no stack frames leaked to loopback consumers).
    //
    // See #366 acceptance criteria — the log side of that ticket is wired
    // through here; the HTTP-body / 503 side is `schema_unavailable` +
    // `require_current_schema` above.
    tracing::error!(error.chain = %format!("{err:#}"), "request failed with internal error");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "ok": false, "error": "internal server error" })),
    )
}

/// Structured `503 Service Unavailable` payload returned when the analytics
/// SQLite schema is older than the version this daemon binary was built for.
///
/// Mirrors the acceptance criteria on #366 / #309:
///
/// ```json
/// {
///   "ok": false,
///   "error": "analytics schema is v<N>, daemon expects v<M>; run `budi migrate` (or `budi init`) to upgrade",
///   "needs_migration": true,
///   "current": N,
///   "target": M
/// }
/// ```
///
/// Kept in one place so the `/analytics/*` middleware, the `POST /sync`
/// handler, and any future endpoints emit byte-identical bodies — which
/// the CLI in `budi-cli::client::check_response` then pattern-matches on
/// to render an actionable error instead of "Daemon returned 500".
pub fn schema_unavailable(current: u32, target: u32) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({
            "ok": false,
            "error": format!(
                "analytics schema is v{current}, daemon expects v{target}; \
                 run `budi migrate` (or `budi init`) to upgrade"
            ),
            "needs_migration": true,
            "current": current,
            "target": target,
        })),
    )
}

/// Middleware that short-circuits `/analytics/*` (and any other route it is
/// layered onto) with `schema_unavailable` when the analytics SQLite DB is
/// present but at a lower schema version than this binary expects.
///
/// Design notes:
///
/// * Opens the DB read-only via [`budi_core::analytics::open_db`]. If the DB
///   file does not exist yet, we treat that as "fresh install, handler will
///   create it" and fall through — the daemon's boot-time
///   `open_db_with_migration` is responsible for initial bring-up.
/// * Only trips on `current < target`. `current == target` is the happy path;
///   `current > target` means the operator downgraded the daemon binary
///   against a newer DB — a different class of problem that this ticket
///   (#366) does not try to handle. We log a warn line in that case so
///   ops can diagnose it from `daemon.log`.
/// * Any transient open/query error also falls through: we'd rather the
///   handler produce its own diagnostic than mask it behind a misleading 503.
pub async fn require_current_schema(
    req: Request,
    next: Next,
) -> Result<Response, (StatusCode, Json<serde_json::Value>)> {
    let db_path = match budi_core::analytics::db_path() {
        Ok(p) => p,
        Err(_) => return Ok(next.run(req).await),
    };
    if !db_path.exists() {
        return Ok(next.run(req).await);
    }
    let conn = match budi_core::analytics::open_db(&db_path) {
        Ok(c) => c,
        Err(_) => return Ok(next.run(req).await),
    };
    let current = budi_core::migration::current_version(&conn);
    let target = budi_core::migration::SCHEMA_VERSION;
    if current < target {
        tracing::warn!(
            target: "budi_daemon::schema",
            current,
            target,
            path = %req.uri().path(),
            "refusing analytics request; DB schema is older than daemon expects"
        );
        return Err(schema_unavailable(current, target));
    }
    if current > target {
        tracing::warn!(
            target: "budi_daemon::schema",
            current,
            target,
            "DB schema is newer than daemon expects; request forwarded but results may be wrong"
        );
    }
    Ok(next.run(req).await)
}

pub fn bad_request(msg: impl std::fmt::Display) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "ok": false, "error": format!("{msg}") })),
    )
}

pub fn not_found(msg: impl std::fmt::Display) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "ok": false, "error": format!("{msg}") })),
    )
}

pub fn forbidden(msg: impl std::fmt::Display) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({ "ok": false, "error": format!("{msg}") })),
    )
}

pub async fn require_loopback(
    req: Request,
    next: Next,
) -> Result<Response, (StatusCode, Json<serde_json::Value>)> {
    let remote_addr = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0);

    match remote_addr {
        Some(addr) if addr.ip().is_loopback() => Ok(next.run(req).await),
        Some(addr) => {
            tracing::warn!(remote_addr = %addr, "blocked non-loopback access to protected route");
            Err(forbidden("this endpoint is available only via loopback"))
        }
        None => {
            tracing::warn!("missing peer address on protected route");
            Err(forbidden("missing peer address for loopback validation"))
        }
    }
}
