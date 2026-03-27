//! OpenTelemetry (OTLP) HTTP/JSON ingestion endpoints.
//!
//! Receives OTLP logs and metrics from Claude Code's telemetry SDK.
//! Fire-and-forget: always returns 200 OK to avoid blocking the SDK.

use axum::Json;
use axum::http::StatusCode;

/// POST /v1/logs — OTLP logs ingestion.
///
/// Parses `claude_code.api_request` events and upserts into messages table
/// with `cost_confidence = 'otel_exact'`. Same fire-and-forget pattern as hooks.
pub async fn otel_logs_ingest(Json(payload): Json<serde_json::Value>) -> StatusCode {
    tokio::task::spawn_blocking(move || {
        let request: budi_core::otel::ExportLogsServiceRequest =
            match serde_json::from_value(payload) {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!("Failed to parse OTLP logs payload: {e}");
                    return;
                }
            };

        let events = budi_core::otel::parse_otel_logs(&request);
        if events.is_empty() {
            return;
        }

        let result = (|| -> Option<()> {
            let db_path = budi_core::analytics::db_path().ok()?;
            let mut conn = budi_core::analytics::open_db(&db_path).ok()?;
            match budi_core::otel::ingest_otel_events(&mut conn, &events) {
                Ok(n) => {
                    if n > 0 {
                        tracing::debug!("OTEL: ingested {n} api_request events");
                    }
                }
                Err(e) => {
                    tracing::warn!("OTEL ingestion error: {e}");
                }
            }
            Some(())
        })();

        if result.is_none() {
            tracing::debug!("OTEL: could not open database");
        }
    });

    StatusCode::OK
}

/// POST /v1/metrics — OTLP metrics ingestion (stub).
///
/// Acknowledges receipt immediately. Metrics processing is future work.
pub async fn otel_metrics_ingest() -> StatusCode {
    StatusCode::OK
}
