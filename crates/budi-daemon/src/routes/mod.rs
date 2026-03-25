pub mod analytics;
pub mod dashboard;
pub mod hooks;

use axum::Json;
use axum::http::StatusCode;

pub fn internal_error(err: anyhow::Error) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "ok": false, "error": format!("{err:#}") })),
    )
}
