pub mod analytics;
pub mod dashboard;
pub mod hooks;
pub mod system;

use axum::http::StatusCode;

pub fn internal_error(err: anyhow::Error) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}"))
}
