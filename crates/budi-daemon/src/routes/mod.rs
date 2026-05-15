pub(crate) mod analytics;
pub(crate) mod cloud;
pub(crate) mod hooks;
pub(crate) mod pricing;

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Json;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::StatusCode;
use axum::http::header;
use axum::middleware::Next;
use axum::response::Response;

pub(crate) fn internal_error(err: anyhow::Error) -> (StatusCode, Json<serde_json::Value>) {
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
///   "error": "analytics schema is v<N>, daemon expects v<M>; run `budi db check --fix` (or `budi init`) to upgrade",
///   "needs_migration": true,
///   "current": N,
///   "target": M
/// }
/// ```
///
/// The CLI's `parse_needs_migration_error` pattern-matches on
/// `needs_migration: true` (not on the verb text) so renaming
/// `budi db migrate` → `budi db check --fix` in 8.3.14 (#586) is a
/// pure user-facing string change; the wire contract is unchanged.
///
/// Kept in one place so the `/analytics/*` middleware, the `POST /sync`
/// handler, and any future endpoints emit byte-identical bodies — which
/// the CLI in `budi-cli::client::check_response` then pattern-matches on
/// to render an actionable error instead of "Daemon returned 500".
pub(crate) fn schema_unavailable(
    current: u32,
    target: u32,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({
            "ok": false,
            "error": format!(
                "analytics schema is v{current}, daemon expects v{target}; \
                 run `budi db check --fix` (or `budi init`) to upgrade"
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
pub(crate) enum SchemaStatus {
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
pub(crate) fn schema_status_for(db_path: &std::path::Path) -> SchemaStatus {
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
pub(crate) async fn require_current_schema(
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

pub(crate) fn bad_request(msg: impl std::fmt::Display) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "ok": false, "error": format!("{msg}") })),
    )
}

pub(crate) fn not_found(msg: impl std::fmt::Display) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "ok": false, "error": format!("{msg}") })),
    )
}

pub(crate) fn forbidden(msg: impl std::fmt::Display) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({ "ok": false, "error": format!("{msg}") })),
    )
}

/// Allowlist of `Host` header values the daemon accepts.
///
/// DNS-rebinding defense (#695): a malicious page can resolve a hostile
/// name to `127.0.0.1` and then have the user's browser hit the daemon
/// on a loopback connection.  `require_loopback` only inspects the peer
/// IP, which is loopback in that scenario, so it lets the request
/// through.  Layering [`require_local_host`] in front of every route
/// closes the gap: the rebound page sends `Host: attacker.example`,
/// which is not in the allowlist, so the request is rejected before any
/// handler runs.
///
/// The allowlist contains, with and without an explicit port:
/// - `127.0.0.1`
/// - `[::1]` (canonical bracketed IPv6)
/// - `localhost`
/// - the literal `--host` value the daemon was started with (so
///   `--host 0.0.0.0 --port 7878` still accepts `Host: 0.0.0.0:7878`
///   for the operator who explicitly opened the listener).
///
/// Anything else returns `403` with `invalid Host header` and a
/// `tracing::warn!` line for ops visibility.
#[derive(Clone, Debug)]
pub(crate) struct HostAllowlist {
    hosts: Arc<HashSet<String>>,
}

impl HostAllowlist {
    pub(crate) fn new(configured_host: &str, port: u16) -> Self {
        let mut hosts = HashSet::new();
        let bases = ["127.0.0.1", "[::1]", "localhost", configured_host];
        for base in bases {
            // `0.0.0.0` is a wildcard bind sentinel, never a valid Host
            // value — skip it so an operator running `--host 0.0.0.0`
            // doesn't silently widen the allowlist with an unreachable
            // literal.
            if base.is_empty() || base == "0.0.0.0" {
                continue;
            }
            hosts.insert(base.to_string());
            hosts.insert(format!("{base}:{port}"));
        }
        Self {
            hosts: Arc::new(hosts),
        }
    }

    /// Test/dev convenience: a permissive default whose only consumers
    /// are router-level unit tests where the Host header is unset.
    /// Production callers always go through `HostAllowlist::new`.
    #[cfg(test)]
    pub(crate) fn for_tests() -> Self {
        Self::new("127.0.0.1", 0)
    }

    pub(crate) fn allows(&self, host: &str) -> bool {
        self.hosts.contains(host)
    }
}

/// Reject any request whose `Host` header is not in the local-host
/// allowlist.  Layered onto every route (public and protected) to defend
/// against DNS-rebinding (#695) — the peer IP is loopback in that
/// scenario, so `require_loopback` alone is insufficient.
pub(crate) async fn require_local_host(
    State(allowlist): State<HostAllowlist>,
    req: Request,
    next: Next,
) -> Result<Response, (StatusCode, Json<serde_json::Value>)> {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok());
    match host {
        Some(h) if allowlist.allows(h) => Ok(next.run(req).await),
        Some(h) => {
            tracing::warn!(
                host = %h,
                path = %req.uri().path(),
                "blocked request with non-local Host header (DNS-rebinding defense, #695)"
            );
            Err(forbidden("invalid Host header"))
        }
        None => {
            // HTTP/1.1 requires a Host header; HTTP/2 carries `:authority`
            // which axum surfaces as `Host` here. A missing/non-ASCII Host
            // is anomalous on a loopback API and not worth distinguishing
            // from an explicitly bad value.
            tracing::warn!(
                path = %req.uri().path(),
                "blocked request with missing or non-ASCII Host header"
            );
            Err(forbidden("invalid Host header"))
        }
    }
}

pub(crate) async fn require_loopback(
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
