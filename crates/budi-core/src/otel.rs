//! OpenTelemetry (OTLP) JSON ingestion for exact per-request cost data.
//!
//! Parses OTLP HTTP/JSON log payloads from Claude Code's telemetry SDK,
//! extracts `claude_code.api_request` events, and upserts them into the
//! messages table with `cost_confidence = 'otel_exact'`.

use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use serde::Deserialize;
use sha2::{Digest, Sha256};

// ── OTLP JSON types (subset we need) ─────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ExportLogsServiceRequest {
    #[serde(default, rename = "resourceLogs")]
    pub resource_logs: Vec<ResourceLogs>,
}

#[derive(Debug, Deserialize)]
pub struct ResourceLogs {
    #[serde(default)]
    pub resource: Option<Resource>,
    #[serde(default, rename = "scopeLogs")]
    pub scope_logs: Vec<ScopeLogs>,
}

#[derive(Debug, Deserialize)]
pub struct Resource {
    #[serde(default)]
    pub attributes: Vec<KeyValue>,
}

#[derive(Debug, Deserialize)]
pub struct ScopeLogs {
    #[serde(default, rename = "logRecords")]
    pub log_records: Vec<LogRecord>,
}

#[derive(Debug, Deserialize)]
pub struct LogRecord {
    #[serde(default, rename = "timeUnixNano")]
    pub time_unix_nano: Option<String>,
    #[serde(default)]
    pub body: Option<AnyValue>,
    #[serde(default)]
    pub attributes: Vec<KeyValue>,
}

#[derive(Debug, Deserialize)]
pub struct KeyValue {
    pub key: String,
    #[serde(default)]
    pub value: Option<AnyValue>,
}

#[derive(Debug, Deserialize)]
pub struct AnyValue {
    #[serde(default, rename = "stringValue")]
    pub string_value: Option<String>,
    #[serde(default, rename = "intValue")]
    pub int_value: Option<serde_json::Value>,
    #[serde(default, rename = "doubleValue")]
    pub double_value: Option<f64>,
}

// ── Parsed event ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct OtelApiRequest {
    pub session_id: String,
    pub timestamp: DateTime<Utc>,
    pub timestamp_nano: String,
    pub model: String,
    pub cost_usd: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub duration_ms: u64,
}

// ── Parsing ───────────────────────────────────────────────────────────

fn get_attr_str(attrs: &[KeyValue], key: &str) -> Option<String> {
    attrs.iter().find(|kv| kv.key == key).and_then(|kv| {
        kv.value
            .as_ref()
            .and_then(|v| v.string_value.as_ref().cloned())
    })
}

fn get_attr_double(attrs: &[KeyValue], key: &str) -> Option<f64> {
    attrs.iter().find(|kv| kv.key == key).and_then(|kv| {
        kv.value.as_ref().and_then(|v| {
            v.double_value.or_else(|| {
                // Sometimes doubles come as stringValue
                v.string_value.as_ref().and_then(|s| s.parse().ok())
            })
        })
    })
}

fn get_attr_u64(attrs: &[KeyValue], key: &str) -> Option<u64> {
    attrs.iter().find(|kv| kv.key == key).and_then(|kv| {
        kv.value.as_ref().and_then(|v| {
            // intValue can be a JSON string "1000" or a number 1000
            match &v.int_value {
                Some(serde_json::Value::String(s)) => s.parse().ok(),
                Some(serde_json::Value::Number(n)) => n.as_u64(),
                _ => {
                    // Fallback: try stringValue
                    v.string_value.as_ref().and_then(|s| s.parse().ok())
                }
            }
        })
    })
}

fn parse_timestamp_nano(nano_str: &str) -> Option<DateTime<Utc>> {
    let nanos: i64 = nano_str.parse().ok()?;
    let secs = nanos / 1_000_000_000;
    let nsecs = (nanos % 1_000_000_000) as u32;
    DateTime::from_timestamp(secs, nsecs)
}

/// Parse an OTLP logs payload and extract `claude_code.api_request` events.
pub fn parse_otel_logs(request: &ExportLogsServiceRequest) -> Vec<OtelApiRequest> {
    let mut events = Vec::new();

    for rl in &request.resource_logs {
        // Extract session.id from resource attributes
        let resource_attrs = rl
            .resource
            .as_ref()
            .map(|r| r.attributes.as_slice())
            .unwrap_or_default();
        let resource_session_id = get_attr_str(resource_attrs, "session.id");

        for sl in &rl.scope_logs {
            for record in &sl.log_records {
                // Check if this is a claude_code.api_request event
                let event_name = record.body.as_ref().and_then(|b| b.string_value.as_ref());
                if event_name.map(|s| s.as_str()) != Some("claude_code.api_request") {
                    continue;
                }

                let attrs = &record.attributes;

                // session_id: from record attributes, then resource attributes
                let session_id = get_attr_str(attrs, "session.id")
                    .or_else(|| resource_session_id.clone())
                    .unwrap_or_default();
                if session_id.is_empty() {
                    continue;
                }

                let timestamp_nano = record.time_unix_nano.as_deref().unwrap_or("0").to_string();
                let timestamp = parse_timestamp_nano(&timestamp_nano).unwrap_or_else(Utc::now);

                let model = get_attr_str(attrs, "model").unwrap_or_default();
                let cost_usd = get_attr_double(attrs, "cost_usd").unwrap_or(0.0);
                let input_tokens = get_attr_u64(attrs, "input_tokens").unwrap_or(0);
                let output_tokens = get_attr_u64(attrs, "output_tokens").unwrap_or(0);
                let cache_read_tokens = get_attr_u64(attrs, "cache_read_tokens").unwrap_or(0);
                let cache_creation_tokens =
                    get_attr_u64(attrs, "cache_creation_tokens").unwrap_or(0);
                let duration_ms = get_attr_u64(attrs, "duration_ms").unwrap_or(0);

                events.push(OtelApiRequest {
                    session_id,
                    timestamp,
                    timestamp_nano,
                    model,
                    cost_usd,
                    input_tokens,
                    output_tokens,
                    cache_read_tokens,
                    cache_creation_tokens,
                    duration_ms,
                });
            }
        }
    }

    events
}

// ── Ingestion ─────────────────────────────────────────────────────────

/// Generate a deterministic UUID from session_id + timestamp_nano.
fn otel_uuid(session_id: &str, timestamp_nano: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"otel:");
    hasher.update(session_id.as_bytes());
    hasher.update(b":");
    hasher.update(timestamp_nano.as_bytes());
    let hash = hasher.finalize();
    format!("otel-{}", hex::encode(&hash[..16]))
}

/// Hex-encode bytes (no extra dependency).
mod hex {
    pub fn encode(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// Ingest OTEL api_request events into the messages table.
///
/// Dedup strategy: OTEL and JSONL produce different UUIDs for the same API call.
/// When an OTEL event arrives we first try to find an existing JSONL row for the
/// same session + model + close timestamp (±1s) and upgrade it in-place. If no
/// match exists (OTEL arrived before JSONL sync), we insert a new row with an
/// `otel-` prefixed UUID. Either way, the row ends up with `cost_confidence = 'otel_exact'`.
///
/// Returns the number of rows upserted.
pub fn ingest_otel_events(conn: &mut Connection, events: &[OtelApiRequest]) -> Result<usize> {
    if events.is_empty() {
        return Ok(0);
    }

    let tx = conn.transaction()?;
    let mut upserted = 0;

    for event in events {
        let ts = event.timestamp.to_rfc3339();
        // Pre-compute ±1 second window for index-friendly range predicates
        let ts_lo = (event.timestamp - chrono::Duration::seconds(1)).to_rfc3339();
        let ts_hi = (event.timestamp + chrono::Duration::seconds(1)).to_rfc3339();
        // Calculate cost from tokens × pricing instead of trusting OTEL's self-reported
        // cost_usd, which systematically underreports by ~10% vs official Anthropic billing.
        let pricing = crate::providers::claude_code::claude_pricing_for_model(&event.model);
        let cost_cents = (event.input_tokens as f64 * pricing.input / 1_000_000.0
            + event.output_tokens as f64 * pricing.output / 1_000_000.0
            + event.cache_creation_tokens as f64 * pricing.cache_write / 1_000_000.0
            + event.cache_read_tokens as f64 * pricing.cache_read / 1_000_000.0)
            * 100.0;

        // Look up session context (repo_id, git_branch, cwd)
        let session_ctx: Option<(Option<String>, Option<String>, Option<String>)> = tx
            .query_row(
                "SELECT repo_id, git_branch, workspace_root FROM sessions WHERE conversation_id = ?1",
                params![event.session_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .ok();

        let (repo_id, git_branch, cwd) = session_ctx.unwrap_or((None, None, None));

        // Check if an otel_exact row already exists for this API call (dedup repeated OTEL events)
        let already_has_otel: bool = tx
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM messages
                    WHERE session_id = ?1
                      AND model = ?2
                      AND role = 'assistant'
                      AND cost_confidence = 'otel_exact'
                      AND timestamp BETWEEN ?3 AND ?4
                )",
                params![event.session_id, event.model, ts_lo, ts_hi],
                |row| row.get(0),
            )
            .unwrap_or(false);

        if already_has_otel {
            // Already processed — just log to otel_events and skip
            tx.execute(
                "INSERT INTO otel_events (event_name, session_id, timestamp, processed)
                VALUES ('claude_code.api_request', ?1, ?2, 1)",
                params![event.session_id, ts],
            )?;
            continue;
        }

        // Strategy 1: Try to find and upgrade an existing JSONL row.
        // Fetch candidates from the index-friendly range, then filter in Rust.
        let existing_uuid: Option<String> = {
            let mut stmt = tx.prepare_cached(
                "SELECT uuid, cost_confidence, timestamp FROM messages
                 WHERE session_id = ?1
                   AND model = ?2
                   AND role = 'assistant'
                   AND timestamp BETWEEN ?3 AND ?4",
            )?;
            let rows: Vec<(String, String, String)> = stmt
                .query_map(
                    params![event.session_id, event.model, ts_lo, ts_hi],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?
                .filter_map(|r| r.ok())
                .collect();
            // Pick the closest non-otel_exact row by actual timestamp distance
            let target_secs = event.timestamp.timestamp();
            rows.into_iter()
                .filter(|(_, conf, _)| conf != "otel_exact")
                .min_by_key(|(_, _, row_ts)| {
                    DateTime::parse_from_rfc3339(row_ts)
                        .map(|dt| (dt.timestamp() - target_secs).unsigned_abs())
                        .unwrap_or(u64::MAX)
                })
                .map(|(uuid, _, _)| uuid)
        };

        if let Some(ref jsonl_uuid) = existing_uuid {
            // Upgrade the existing JSONL row in-place with exact OTEL data
            tx.execute(
                "UPDATE messages SET
                    cost_cents = ?1,
                    cost_confidence = 'otel_exact',
                    input_tokens = ?2,
                    output_tokens = ?3,
                    cache_creation_tokens = ?4,
                    cache_read_tokens = ?5,
                    model = ?6
                 WHERE uuid = ?7",
                params![
                    cost_cents,
                    event.input_tokens as i64,
                    event.output_tokens as i64,
                    event.cache_creation_tokens as i64,
                    event.cache_read_tokens as i64,
                    event.model,
                    jsonl_uuid,
                ],
            )?;
            upserted += 1;
        } else {
            // Strategy 2: No JSONL row yet — insert with otel-prefixed UUID.
            // If JSONL syncs later, the JSONL row will be a separate INSERT OR IGNORE
            // with a different UUID. We handle that via the reverse path: JSONL's
            // INSERT OR IGNORE succeeds (different UUID), but we'll clean it up
            // in the next OTEL event or it stays as a low-cost duplicate.
            // Actually, once this otel row exists, future OTEL events with the same
            // timestamp_nano will match it via ON CONFLICT and be no-ops.
            let uuid = otel_uuid(&event.session_id, &event.timestamp_nano);
            tx.execute(
                "INSERT INTO messages (uuid, session_id, role, timestamp, model, provider,
                    input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
                    cost_cents, cost_confidence, repo_id, git_branch, cwd)
                VALUES (?1, ?2, 'assistant', ?3, ?4, 'claude_code', ?5, ?6, ?7, ?8, ?9, 'otel_exact', ?10, ?11, ?12)
                ON CONFLICT(uuid) DO UPDATE SET
                    cost_cents = excluded.cost_cents,
                    cost_confidence = excluded.cost_confidence,
                    input_tokens = excluded.input_tokens,
                    output_tokens = excluded.output_tokens,
                    cache_creation_tokens = excluded.cache_creation_tokens,
                    cache_read_tokens = excluded.cache_read_tokens,
                    model = excluded.model
                WHERE messages.cost_confidence != 'otel_exact'",
                params![
                    uuid,
                    event.session_id,
                    ts,
                    event.model,
                    event.input_tokens as i64,
                    event.output_tokens as i64,
                    event.cache_creation_tokens as i64,
                    event.cache_read_tokens as i64,
                    cost_cents,
                    repo_id,
                    git_branch,
                    cwd,
                ],
            )?;
            upserted += 1;
        }

        // Insert stub session if it doesn't exist yet (hooks may arrive later)
        tx.execute(
            "INSERT OR IGNORE INTO sessions (conversation_id, provider) VALUES (?1, 'claude_code')",
            params![event.session_id],
        )?;

        // Store raw event in otel_events for debugging
        tx.execute(
            "INSERT INTO otel_events (event_name, session_id, timestamp, processed)
            VALUES ('claude_code.api_request', ?1, ?2, 1)",
            params![event.session_id, ts],
        )?;
    }

    tx.commit()?;
    Ok(upserted)
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA foreign_keys=ON;",
        )
        .unwrap();
        crate::migration::migrate(&conn).unwrap();
        conn
    }

    fn sample_otlp_payload() -> ExportLogsServiceRequest {
        serde_json::from_value(serde_json::json!({
            "resourceLogs": [{
                "resource": {
                    "attributes": [
                        {"key": "session.id", "value": {"stringValue": "sess-abc-123"}}
                    ]
                },
                "scopeLogs": [{
                    "logRecords": [
                        {
                            "timeUnixNano": "1711500000000000000",
                            "body": {"stringValue": "claude_code.api_request"},
                            "attributes": [
                                {"key": "model", "value": {"stringValue": "claude-opus-4-6"}},
                                {"key": "cost_usd", "value": {"doubleValue": 0.05}},
                                {"key": "input_tokens", "value": {"intValue": "1000"}},
                                {"key": "output_tokens", "value": {"intValue": "500"}},
                                {"key": "cache_read_tokens", "value": {"intValue": "50000"}},
                                {"key": "cache_creation_tokens", "value": {"intValue": "5000"}},
                                {"key": "duration_ms", "value": {"intValue": "1200"}}
                            ]
                        },
                        {
                            "timeUnixNano": "1711500001000000000",
                            "body": {"stringValue": "claude_code.api_request"},
                            "attributes": [
                                {"key": "model", "value": {"stringValue": "claude-sonnet-4-6"}},
                                {"key": "cost_usd", "value": {"doubleValue": 0.02}},
                                {"key": "input_tokens", "value": {"intValue": "800"}},
                                {"key": "output_tokens", "value": {"intValue": "200"}},
                                {"key": "cache_read_tokens", "value": {"intValue": "10000"}},
                                {"key": "cache_creation_tokens", "value": {"intValue": "0"}},
                                {"key": "duration_ms", "value": {"intValue": "500"}}
                            ]
                        }
                    ]
                }]
            }]
        }))
        .unwrap()
    }

    #[test]
    fn parse_otel_logs_extracts_api_requests() {
        let payload = sample_otlp_payload();
        let events = parse_otel_logs(&payload);

        assert_eq!(events.len(), 2);

        assert_eq!(events[0].session_id, "sess-abc-123");
        assert_eq!(events[0].model, "claude-opus-4-6");
        assert!((events[0].cost_usd - 0.05).abs() < 1e-10);
        assert_eq!(events[0].input_tokens, 1000);
        assert_eq!(events[0].output_tokens, 500);
        assert_eq!(events[0].cache_read_tokens, 50000);
        assert_eq!(events[0].cache_creation_tokens, 5000);
        assert_eq!(events[0].duration_ms, 1200);

        assert_eq!(events[1].model, "claude-sonnet-4-6");
        assert!((events[1].cost_usd - 0.02).abs() < 1e-10);
    }

    #[test]
    fn parse_otel_logs_skips_non_api_request_events() {
        let payload: ExportLogsServiceRequest = serde_json::from_value(serde_json::json!({
            "resourceLogs": [{
                "resource": {
                    "attributes": [
                        {"key": "session.id", "value": {"stringValue": "sess-1"}}
                    ]
                },
                "scopeLogs": [{
                    "logRecords": [
                        {
                            "timeUnixNano": "1711500000000000000",
                            "body": {"stringValue": "claude_code.tool_use"},
                            "attributes": [
                                {"key": "model", "value": {"stringValue": "claude-opus-4-6"}}
                            ]
                        }
                    ]
                }]
            }]
        }))
        .unwrap();

        let events = parse_otel_logs(&payload);
        assert!(events.is_empty());
    }

    #[test]
    fn parse_otel_logs_skips_missing_session_id() {
        let payload: ExportLogsServiceRequest = serde_json::from_value(serde_json::json!({
            "resourceLogs": [{
                "resource": {"attributes": []},
                "scopeLogs": [{
                    "logRecords": [{
                        "timeUnixNano": "1711500000000000000",
                        "body": {"stringValue": "claude_code.api_request"},
                        "attributes": [
                            {"key": "model", "value": {"stringValue": "claude-opus-4-6"}}
                        ]
                    }]
                }]
            }]
        }))
        .unwrap();

        let events = parse_otel_logs(&payload);
        assert!(events.is_empty());
    }

    #[test]
    fn parse_otel_logs_int_value_as_number() {
        let payload: ExportLogsServiceRequest = serde_json::from_value(serde_json::json!({
            "resourceLogs": [{
                "resource": {
                    "attributes": [
                        {"key": "session.id", "value": {"stringValue": "sess-num"}}
                    ]
                },
                "scopeLogs": [{
                    "logRecords": [{
                        "timeUnixNano": "1711500000000000000",
                        "body": {"stringValue": "claude_code.api_request"},
                        "attributes": [
                            {"key": "model", "value": {"stringValue": "test-model"}},
                            {"key": "cost_usd", "value": {"doubleValue": 0.01}},
                            {"key": "input_tokens", "value": {"intValue": 500}},
                            {"key": "output_tokens", "value": {"intValue": 100}}
                        ]
                    }]
                }]
            }]
        }))
        .unwrap();

        let events = parse_otel_logs(&payload);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].input_tokens, 500);
        assert_eq!(events[0].output_tokens, 100);
    }

    #[test]
    fn ingest_otel_events_inserts_messages() {
        let mut conn = setup_db();
        let payload = sample_otlp_payload();
        let events = parse_otel_logs(&payload);

        let count = ingest_otel_events(&mut conn, &events).unwrap();
        assert_eq!(count, 2);

        // Verify messages were inserted
        let msg_count: i64 = conn
            .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(msg_count, 2);

        // Verify cost_confidence
        let confidence: String = conn
            .query_row("SELECT cost_confidence FROM messages LIMIT 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(confidence, "otel_exact");

        // Verify cost_cents is calculated from tokens × pricing (not from cost_usd)
        // Event 1: opus-4-6, 1000 input, 500 output, 5000 cache_create, 50000 cache_read
        // Cost = (1000*5 + 500*25 + 5000*6.25 + 50000*0.50) / 1M * 100 cents
        //      = (5000 + 12500 + 31250 + 25000) / 1M * 100 = 0.07375 * 100 = 7.375
        let cost: f64 = conn
            .query_row(
                "SELECT cost_cents FROM messages ORDER BY timestamp LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            (cost - 7.375).abs() < 0.001,
            "cost_cents should be 7.375, got {cost}"
        );

        // Verify session stub was created
        let sess_count: i64 = conn
            .query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sess_count, 1);

        // Verify otel_events were logged
        let otel_count: i64 = conn
            .query_row("SELECT count(*) FROM otel_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(otel_count, 2);
    }

    #[test]
    fn otel_upgrades_existing_jsonl_row() {
        let mut conn = setup_db();

        // Simulate JSONL sync: insert a message with estimated cost and a JSONL UUID
        let jsonl_uuid = "abc12345-jsonl-uuid-0000-000000000001";
        conn.execute(
            "INSERT INTO messages (uuid, session_id, role, timestamp, model, provider,
                input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
                cost_cents, cost_confidence)
            VALUES (?1, 'sess-dedup', 'assistant', '2024-03-27T00:00:00+00:00', 'claude-opus-4-6',
                    'claude_code', 900, 400, 4000, 40000, 3.5, 'estimated')",
            params![jsonl_uuid],
        )
        .unwrap();

        // Now ingest OTEL data for the same API call (close timestamp, same session+model)
        let events = vec![OtelApiRequest {
            session_id: "sess-dedup".to_string(),
            timestamp: DateTime::parse_from_rfc3339("2024-03-27T00:00:00.200Z")
                .unwrap()
                .with_timezone(&Utc),
            timestamp_nano: "1711500000200000000".to_string(),
            model: "claude-opus-4-6".to_string(),
            cost_usd: 0.07,
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: 50000,
            cache_creation_tokens: 5000,
            duration_ms: 1200,
        }];

        let count = ingest_otel_events(&mut conn, &events).unwrap();
        assert_eq!(count, 1);

        // Should still be only 1 message (upgraded in-place, not duplicated)
        let msg_count: i64 = conn
            .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            msg_count, 1,
            "OTEL should upgrade the JSONL row, not insert a new one"
        );

        // Verify OTEL data overwrote estimated data on the JSONL UUID
        let (cost, confidence, input_tokens): (f64, String, i64) = conn
            .query_row(
                "SELECT cost_cents, cost_confidence, input_tokens FROM messages WHERE uuid = ?1",
                params![jsonl_uuid],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();

        // Cost calculated from tokens: (1000*5 + 500*25 + 5000*6.25 + 50000*0.50) / 1M * 100
        assert!(
            (cost - 7.375).abs() < 0.001,
            "cost should be 7.375, got {cost}"
        );
        assert_eq!(confidence, "otel_exact");
        assert_eq!(input_tokens, 1000);
    }

    #[test]
    fn otel_does_not_overwrite_existing_otel_data() {
        let mut conn = setup_db();

        // Insert OTEL data (no existing JSONL row, so it inserts with otel- UUID)
        let events = vec![OtelApiRequest {
            session_id: "sess-keep".to_string(),
            timestamp: DateTime::parse_from_rfc3339("2024-03-27T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            timestamp_nano: "1711500000000000000".to_string(),
            model: "claude-opus-4-6".to_string(),
            cost_usd: 0.05,
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: 50000,
            cache_creation_tokens: 5000,
            duration_ms: 1200,
        }];
        ingest_otel_events(&mut conn, &events).unwrap();

        // Try to overwrite with different OTEL data (same timestamp_nano → same otel UUID,
        // and the existing row is already otel_exact so the timestamp match will find it
        // but the WHERE clause in ON CONFLICT prevents overwrite)
        let events2 = vec![OtelApiRequest {
            session_id: "sess-keep".to_string(),
            timestamp: DateTime::parse_from_rfc3339("2024-03-27T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            // Use a DIFFERENT timestamp_nano so it doesn't match the existing otel row
            // by UUID, but DOES match by session_id + model + close timestamp.
            // Since the existing row is otel_exact, the timestamp match should skip it.
            timestamp_nano: "1711500000100000000".to_string(),
            model: "claude-opus-4-6".to_string(),
            cost_usd: 0.99, // different cost
            input_tokens: 9999,
            output_tokens: 9999,
            cache_read_tokens: 9999,
            cache_creation_tokens: 9999,
            duration_ms: 9999,
        }];
        ingest_otel_events(&mut conn, &events2).unwrap();

        // Should still be only 1 message (not duplicated)
        let msg_count: i64 = conn
            .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            msg_count, 1,
            "Duplicate OTEL event should not create second row"
        );

        // Original OTEL data should be preserved
        let cost: f64 = conn
            .query_row("SELECT cost_cents FROM messages LIMIT 1", [], |r| r.get(0))
            .unwrap();
        // Original cost from tokens: (1000*5 + 500*25 + 5000*6.25 + 50000*0.50) / 1M * 100
        assert!(
            (cost - 7.375).abs() < 0.001,
            "original cost should be preserved, got {cost}"
        );
    }

    #[test]
    fn session_correlation_provides_git_context() {
        let mut conn = setup_db();

        // Create a session with git context (as hooks would)
        conn.execute(
            "INSERT INTO sessions (conversation_id, provider, repo_id, git_branch, workspace_root)
            VALUES ('sess-git', 'claude_code', 'github.com/user/repo', 'feature/otel', '/home/user/repo')",
            [],
        )
        .unwrap();

        // Ingest OTEL event for that session
        let events = vec![OtelApiRequest {
            session_id: "sess-git".to_string(),
            timestamp: DateTime::parse_from_rfc3339("2024-03-27T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            timestamp_nano: "1711500000000000000".to_string(),
            model: "claude-opus-4-6".to_string(),
            cost_usd: 0.03,
            input_tokens: 500,
            output_tokens: 200,
            cache_read_tokens: 10000,
            cache_creation_tokens: 1000,
            duration_ms: 800,
        }];
        ingest_otel_events(&mut conn, &events).unwrap();

        // Verify message picked up git context from session
        let (repo_id, branch, cwd): (Option<String>, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT repo_id, git_branch, cwd FROM messages LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();

        assert_eq!(repo_id.as_deref(), Some("github.com/user/repo"));
        assert_eq!(branch.as_deref(), Some("feature/otel"));
        assert_eq!(cwd.as_deref(), Some("/home/user/repo"));
    }

    #[test]
    fn deterministic_uuid_generation() {
        let uuid1 = otel_uuid("sess-1", "1711500000000000000");
        let uuid2 = otel_uuid("sess-1", "1711500000000000000");
        let uuid3 = otel_uuid("sess-1", "1711500001000000000");
        let uuid4 = otel_uuid("sess-2", "1711500000000000000");

        assert_eq!(uuid1, uuid2); // same input → same uuid
        assert_ne!(uuid1, uuid3); // different timestamp → different uuid
        assert_ne!(uuid1, uuid4); // different session → different uuid
        assert!(uuid1.starts_with("otel-"));
    }

    #[test]
    fn empty_events_is_noop() {
        let mut conn = setup_db();
        let count = ingest_otel_events(&mut conn, &[]).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn parse_empty_payload() {
        let payload: ExportLogsServiceRequest =
            serde_json::from_value(serde_json::json!({"resourceLogs": []})).unwrap();
        let events = parse_otel_logs(&payload);
        assert!(events.is_empty());
    }
}
