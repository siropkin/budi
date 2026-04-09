pub mod analytics;
pub mod dashboard;
pub mod hooks;
pub mod otel;

use std::net::SocketAddr;

use axum::Json;
use axum::extract::{ConnectInfo, Request};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;

pub fn internal_error(err: anyhow::Error) -> (StatusCode, Json<serde_json::Value>) {
    tracing::error!("{err:#}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "ok": false, "error": "internal server error" })),
    )
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
