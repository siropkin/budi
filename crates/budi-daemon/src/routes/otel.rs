//! OpenTelemetry (OTLP) HTTP/JSON ingestion endpoints.
//!
//! Receives OTLP logs and metrics from Claude Code's telemetry SDK.
//! Ingestion is durable-first: payloads are queued, then drained in background.

use axum::Json;
use axum::http::StatusCode;

/// POST /v1/logs — OTLP logs ingestion.
///
/// Appends raw payload to the durable ingest queue.
/// Background worker parses and upserts `claude_code.api_request` events.
pub async fn otel_logs_ingest(Json(payload): Json<serde_json::Value>) -> StatusCode {
    match tokio::task::spawn_blocking(move || {
        budi_core::ingest_queue::enqueue_otel_payload(&payload)
    })
    .await
    {
        Ok(Ok(_)) => StatusCode::OK,
        Ok(Err(e)) => {
            tracing::warn!("OTEL queue enqueue failed: {e}");
            StatusCode::SERVICE_UNAVAILABLE
        }
        Err(e) => {
            tracing::warn!("OTEL queue enqueue task failed: {e}");
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
}

/// POST /v1/metrics — OTLP metrics ingestion (stub).
///
/// Acknowledges receipt immediately. Metrics processing is future work.
pub async fn otel_metrics_ingest() -> StatusCode {
    StatusCode::OK
}
