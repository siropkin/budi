pub mod analytics;
pub mod cloud;
pub mod hooks;
pub mod pricing;

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
///   "error": "analytics schema is v<N>, daemon expects v<M>; run `budi db migrate` (or `budi init`) to upgrade",
///   "needs_migration": true,
///   "current": N,
///   "target": M
/// }
/// ```
///
/// The CLI's `parse_needs_migration_error` pattern-matches on
/// `needs_migration: true` (not on the verb text) so renaming
/// `budi migrate` → `budi db migrate` in 8.2.1 (#368) is a pure
/// user-facing string change; the wire contract is unchanged.
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
                 run `budi db migrate` (or `budi init`) to upgrade"
            ),
            "needs_migration": true,
            "current": current,
            "target": target,
        })),
    )
}

/// Outcome of inspecting the analytics DB's `user_version` against the
/// binary's compiled-in [`budi_core::migration::SCHEMA_VERSION`].
///
/// Broken out as its own enum so the decision logic can be unit-tested
/// against a real tempdir DB without going through the axum router and
/// the process-global `BUDI_HOME` env var that `analytics::db_path()`
/// reads (setenv across threads is unsound on macOS, which previously
/// caused flaky CI — see #366 PR history).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaStatus {
    /// DB is absent, unreadable, or at exactly the expected version — let
    /// the request proceed.  A missing file is normal on first boot; the
    /// daemon's `open_db_with_migration` at startup is what materializes
    /// the schema.  A read error is treated as "fall through" so the real
    /// handler can produce its own diagnostic instead of being masked by
    /// a misleading 503.
    Proceed,
    /// DB's `user_version` is below `target`.  The request should be
    /// short-circuited with `schema_unavailable(current, target)`.
    Stale { current: u32, target: u32 },
    /// DB's `user_version` is above `target` (operator downgraded the
    /// daemon binary against a newer DB).  We forward the request but
    /// log a warn line so ops can diagnose from `daemon.log`.
    Ahead { current: u32, target: u32 },
}

/// Pure function that inspects the SQLite DB at `db_path` and classifies
/// the schema state.  No env-var reads, no global state — tests drive
/// this directly with a tempdir path.
pub fn schema_status_for(db_path: &std::path::Path) -> SchemaStatus {
    if !db_path.exists() {
        return SchemaStatus::Proceed;
    }
    let conn = match budi_core::analytics::open_db(db_path) {
        Ok(c) => c,
        Err(_) => return SchemaStatus::Proceed,
    };
    let current = budi_core::migration::current_version(&conn);
    let target = budi_core::migration::SCHEMA_VERSION;
    if current < target {
        SchemaStatus::Stale { current, target }
    } else if current > target {
        SchemaStatus::Ahead { current, target }
    } else {
        SchemaStatus::Proceed
    }
}

/// Middleware that short-circuits `/analytics/*` (and any other route it is
/// layered onto) with `schema_unavailable` when the analytics SQLite DB is
/// present but at a lower schema version than this binary expects.
///
/// Thin wrapper over [`schema_status_for`]: resolves `db_path()` from env,
/// runs the pure classifier, maps to an HTTP response.
pub async fn require_current_schema(
    req: Request,
    next: Next,
) -> Result<Response, (StatusCode, Json<serde_json::Value>)> {
    let db_path = match budi_core::analytics::db_path() {
        Ok(p) => p,
        Err(_) => return Ok(next.run(req).await),
    };
    match schema_status_for(&db_path) {
        SchemaStatus::Proceed => Ok(next.run(req).await),
        SchemaStatus::Stale { current, target } => {
            tracing::warn!(
                target: "budi_daemon::schema",
                current,
                target,
                path = %req.uri().path(),
                "refusing analytics request; DB schema is older than daemon expects"
            );
            Err(schema_unavailable(current, target))
        }
        SchemaStatus::Ahead { current, target } => {
            tracing::warn!(
                target: "budi_daemon::schema",
                current,
                target,
                "DB schema is newer than daemon expects; request forwarded but results may be wrong"
            );
            Ok(next.run(req).await)
        }
    }
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
