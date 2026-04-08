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
    pub request_id: Option<String>,
    /// Parsed from OTEL attributes but intentionally not used for ingestion —
    /// cost_cents is recomputed from tokens x pricing because cost_usd
    /// systematically underreports (~10%). Kept for tests and debugging.
    pub cost_usd: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
}

// ── Parsing ───────────────────────────────────────────────────────────

fn get_attr_str(attrs: &[KeyValue], key: &str) -> Option<String> {
    attrs.iter().find(|kv| kv.key == key).and_then(|kv| {
        kv.value
            .as_ref()
            .and_then(|v| v.string_value.as_ref().cloned())
    })
}

fn get_first_attr_str(attrs: &[KeyValue], keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| get_attr_str(attrs, key))
        .and_then(|value| {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
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
    if nanos < 0 {
        return None;
    }
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
                let session_id = crate::identity::normalize_session_id(&session_id);

                let timestamp_nano = record.time_unix_nano.as_deref().unwrap_or("0").to_string();
                let timestamp = match parse_timestamp_nano(&timestamp_nano) {
                    Some(ts) => ts,
                    None => {
                        tracing::warn!(
                            "OTEL: skipping event with unparseable timestamp: {timestamp_nano}"
                        );
                        continue;
                    }
                };

                let model = get_attr_str(attrs, "model").unwrap_or_default();
                let request_id = get_first_attr_str(
                    attrs,
                    &[
                        "message.id",
                        "message_id",
                        "request_id",
                        "message_request_id",
                    ],
                );
                let cost_usd = get_attr_double(attrs, "cost_usd").unwrap_or(0.0);
                let input_tokens = get_attr_u64(attrs, "input_tokens").unwrap_or(0);
                let output_tokens = get_attr_u64(attrs, "output_tokens").unwrap_or(0);
                let cache_read_tokens = get_attr_u64(attrs, "cache_read_tokens").unwrap_or(0);
                let cache_creation_tokens =
                    get_attr_u64(attrs, "cache_creation_tokens").unwrap_or(0);
                events.push(OtelApiRequest {
                    session_id,
                    timestamp,
                    timestamp_nano,
                    model,
                    request_id,
                    cost_usd,
                    input_tokens,
                    output_tokens,
                    cache_read_tokens,
                    cache_creation_tokens,
                });
            }
        }
    }

    events
}

/// Parse and ingest one OTEL JSON payload into analytics tables.
pub fn ingest_otel_payload(conn: &mut Connection, payload: &serde_json::Value) -> Result<usize> {
    let request: ExportLogsServiceRequest = serde_json::from_value(payload.clone())?;
    let events = parse_otel_logs(&request);
    if events.is_empty() {
        return Ok(0);
    }
    ingest_otel_events(conn, &events)
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
    hex::encode(&hash[..16])
}

fn otel_event_snapshot_json(
    event: &OtelApiRequest,
    cost_cents_computed: f64,
    policy: &crate::privacy::PrivacyPolicy,
) -> Option<String> {
    if policy.mode == crate::privacy::PrivacyMode::Omit {
        return None;
    }

    Some(
        serde_json::json!({
            "session_id": crate::privacy::minimize_sensitive_field(Some(event.session_id.as_str()), policy.mode),
            "timestamp": event.timestamp.to_rfc3339(),
            "timestamp_nano": event.timestamp_nano,
            "model": event.model,
            "request_id": crate::privacy::minimize_sensitive_field(event.request_id.as_deref(), policy.mode),
            "cost_usd_reported": event.cost_usd,
            "cost_cents_computed": cost_cents_computed,
            "input_tokens": event.input_tokens,
            "output_tokens": event.output_tokens,
            "cache_read_tokens": event.cache_read_tokens,
            "cache_creation_tokens": event.cache_creation_tokens
        })
        .to_string(),
    )
}

/// Hex-encode bytes (no extra dependency).
mod hex {
    pub fn encode(bytes: &[u8]) -> String {
        use std::fmt::Write;
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            let _ = write!(s, "{b:02x}");
        }
        s
    }
}

#[derive(Debug, Clone)]
struct MessageDedupCandidate {
    id: String,
    request_id: Option<String>,
    timestamp: String,
    input_tokens: i64,
    output_tokens: i64,
    cache_creation_tokens: i64,
    cache_read_tokens: i64,
}

#[derive(Debug, Clone, Copy)]
enum DedupStrategy {
    DeterministicOtelId,
    ExactRequestId,
    SourceFingerprint,
    TimestampFallback,
}

impl DedupStrategy {
    fn as_str(self) -> &'static str {
        match self {
            Self::DeterministicOtelId => "deterministic_otel_id",
            Self::ExactRequestId => "exact_request_id",
            Self::SourceFingerprint => "source_fingerprint",
            Self::TimestampFallback => "timestamp_fallback",
        }
    }
}

fn normalize_request_id(id: Option<&str>) -> Option<&str> {
    id.map(str::trim).filter(|s| !s.is_empty())
}

fn token_fingerprint_matches(candidate: &MessageDedupCandidate, event: &OtelApiRequest) -> bool {
    candidate.input_tokens == event.input_tokens as i64
        && candidate.output_tokens == event.output_tokens as i64
        && candidate.cache_creation_tokens == event.cache_creation_tokens as i64
        && candidate.cache_read_tokens == event.cache_read_tokens as i64
}

fn timestamp_distance_millis(timestamp: &str, target: DateTime<Utc>) -> i64 {
    DateTime::parse_from_rfc3339(timestamp)
        .map(|dt| {
            dt.with_timezone(&Utc)
                .signed_duration_since(target)
                .num_milliseconds()
                .abs()
        })
        .unwrap_or(i64::MAX)
}

fn choose_candidate<'a>(
    candidates: &'a [MessageDedupCandidate],
    event: &OtelApiRequest,
) -> Option<(&'a MessageDedupCandidate, DedupStrategy)> {
    if candidates.is_empty() {
        return None;
    }

    if let Some(request_id) = normalize_request_id(event.request_id.as_deref()) {
        let by_request_id: Vec<&MessageDedupCandidate> = candidates
            .iter()
            .filter(|candidate| {
                normalize_request_id(candidate.request_id.as_deref()) == Some(request_id)
            })
            .collect();
        if let Some(best) = by_request_id.into_iter().min_by_key(|candidate| {
            timestamp_distance_millis(&candidate.timestamp, event.timestamp)
        }) {
            return Some((best, DedupStrategy::ExactRequestId));
        }
    }

    let by_fingerprint: Vec<&MessageDedupCandidate> = candidates
        .iter()
        .filter(|candidate| token_fingerprint_matches(candidate, event))
        .collect();
    if by_fingerprint.len() == 1 {
        return Some((by_fingerprint[0], DedupStrategy::SourceFingerprint));
    }
    if by_fingerprint.len() > 1 {
        return None;
    }

    if candidates.len() == 1 {
        return Some((&candidates[0], DedupStrategy::TimestampFallback));
    }

    None
}

/// Ingest OTEL api_request events into the messages table.
///
/// Dedup strategy: OTEL and JSONL produce different UUIDs for the same API call.
/// When an OTEL event arrives we first try to find an existing JSONL row for the
/// same session + model + close timestamp (±1s) and upgrade it in-place. If no
/// match exists (OTEL arrived before JSONL sync), we insert a new row with an
/// deterministic OTEL UUID. Either way, the row ends up with `cost_confidence = 'otel_exact'`.
///
/// Returns the number of rows upserted.
pub fn ingest_otel_events(conn: &mut Connection, events: &[OtelApiRequest]) -> Result<usize> {
    let policy = crate::privacy::load_privacy_policy();
    ingest_otel_events_with_policy(conn, events, &policy)
}

/// Ingest OTEL events using an explicit privacy policy.
pub fn ingest_otel_events_with_policy(
    conn: &mut Connection,
    events: &[OtelApiRequest],
    policy: &crate::privacy::PrivacyPolicy,
) -> Result<usize> {
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
        let pricing = crate::provider::pricing_for_model(&event.model, "claude_code");
        let cost_cents = pricing.calculate_cost_cents(
            event.input_tokens,
            event.output_tokens,
            event.cache_creation_tokens,
            event.cache_read_tokens,
            0,    // OTEL doesn't yet provide 1h cache tier breakdown
            None, // OTEL doesn't yet provide speed
            0,    // OTEL doesn't yet provide web search count
        );

        // Look up session context (repo_id, git_branch, cwd)
        let session_ctx: Option<(Option<String>, Option<String>, Option<String>)> = tx
            .query_row(
                "SELECT repo_id, git_branch, workspace_root FROM sessions WHERE id = ?1",
                params![event.session_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .ok();

        let (repo_id, git_branch, cwd) = session_ctx.unwrap_or((None, None, None));
        let otel_event_uuid = otel_uuid(&event.session_id, &event.timestamp_nano);

        // Fetch candidates in the constrained window and resolve in Rust:
        // request_id -> source fingerprint -> timestamp fallback.
        let mut stmt = tx.prepare_cached(
            "SELECT id, request_id, timestamp, input_tokens, output_tokens,
                    cache_creation_tokens, cache_read_tokens, cost_confidence
             FROM messages
             WHERE session_id = ?1
               AND model = ?2
               AND role = 'assistant'
               AND timestamp BETWEEN ?3 AND ?4",
        )?;
        let rows: Vec<(MessageDedupCandidate, String)> = stmt
            .query_map(
                params![event.session_id, event.model, ts_lo, ts_hi],
                |row| {
                    Ok((
                        MessageDedupCandidate {
                            id: row.get(0)?,
                            request_id: row.get(1)?,
                            timestamp: row.get(2)?,
                            input_tokens: row.get(3)?,
                            output_tokens: row.get(4)?,
                            cache_creation_tokens: row.get(5)?,
                            cache_read_tokens: row.get(6)?,
                        },
                        row.get(7)?,
                    ))
                },
            )?
            .filter_map(|r| r.ok())
            .collect();
        let otel_candidates: Vec<MessageDedupCandidate> = rows
            .iter()
            .filter(|(_, confidence)| confidence == "otel_exact")
            .map(|(candidate, _)| candidate.clone())
            .collect();
        let jsonl_candidates: Vec<MessageDedupCandidate> = rows
            .iter()
            .filter(|(_, confidence)| confidence != "otel_exact")
            .map(|(candidate, _)| candidate.clone())
            .collect();

        let otel_selection = choose_candidate(&otel_candidates, event);
        let existing_otel_match: Option<(String, DedupStrategy)> = tx
            .query_row(
                "SELECT id FROM messages WHERE id = ?1 AND cost_confidence = 'otel_exact' LIMIT 1",
                params![otel_event_uuid],
                |row| row.get(0),
            )
            .ok()
            .map(|id| (id, DedupStrategy::DeterministicOtelId))
            .or_else(|| {
                otel_selection
                    .as_ref()
                    .map(|(candidate, strategy)| (candidate.id.clone(), *strategy))
            });

        let snapshot_json = otel_event_snapshot_json(event, cost_cents, policy);

        if let Some((existing_uuid, strategy)) = existing_otel_match {
            tracing::debug!(
                session_id = %event.session_id,
                model = %event.model,
                strategy = strategy.as_str(),
                message_id = %existing_uuid,
                "OTEL dedup matched existing otel_exact message"
            );
            // Already processed — just log to otel_events and skip
            tx.execute(
                "INSERT INTO otel_events (
                    event_name, session_id, timestamp, raw_json, processed,
                    message_id, timestamp_nano, model, cost_usd_reported, cost_cents_computed
                )
                VALUES ('claude_code.api_request', ?1, ?2, ?3, 1, ?4, ?5, ?6, ?7, ?8)",
                params![
                    event.session_id,
                    ts,
                    snapshot_json.clone(),
                    existing_uuid,
                    event.timestamp_nano,
                    event.model,
                    event.cost_usd,
                    cost_cents
                ],
            )?;
            continue;
        }
        if otel_candidates.len() > 1 && otel_selection.is_none() {
            let fingerprint_matches = otel_candidates
                .iter()
                .filter(|candidate| token_fingerprint_matches(candidate, event))
                .count();
            tracing::warn!(
                session_id = %event.session_id,
                model = %event.model,
                candidate_count = otel_candidates.len(),
                fingerprint_matches,
                "OTEL dedup saw ambiguous otel_exact candidates; skipping unsafe merge"
            );
        }

        let jsonl_selection = choose_candidate(&jsonl_candidates, event);
        if jsonl_candidates.len() > 1 && jsonl_selection.is_none() {
            let fingerprint_matches = jsonl_candidates
                .iter()
                .filter(|candidate| token_fingerprint_matches(candidate, event))
                .count();
            tracing::warn!(
                session_id = %event.session_id,
                model = %event.model,
                candidate_count = jsonl_candidates.len(),
                fingerprint_matches,
                "OTEL dedup saw ambiguous JSONL candidates; inserting dedicated OTEL row"
            );
        }

        let message_id: String = if let Some((jsonl_candidate, strategy)) = jsonl_selection {
            let jsonl_uuid = jsonl_candidate.id.clone();
            // Upgrade the existing JSONL row in-place with exact OTEL data
            tx.execute(
                "UPDATE messages SET
                    cost_cents = ?1,
                    cost_confidence = 'otel_exact',
                    input_tokens = ?2,
                    output_tokens = ?3,
                    cache_creation_tokens = ?4,
                    cache_read_tokens = ?5,
                    model = ?6,
                    request_id = COALESCE(request_id, ?7)
                 WHERE id = ?8",
                params![
                    cost_cents,
                    event.input_tokens as i64,
                    event.output_tokens as i64,
                    event.cache_creation_tokens as i64,
                    event.cache_read_tokens as i64,
                    event.model,
                    event.request_id,
                    jsonl_uuid,
                ],
            )?;
            tracing::info!(
                session_id = %event.session_id,
                model = %event.model,
                strategy = strategy.as_str(),
                message_id = %jsonl_uuid,
                "OTEL upgraded existing JSONL message"
            );
            upserted += 1;
            jsonl_uuid
        } else {
            // Strategy 2: No JSONL row yet — insert with deterministic OTEL UUID.
            tx.execute(
                "INSERT INTO messages (id, session_id, role, timestamp, model, provider,
                    input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
                    cost_cents, cost_confidence, repo_id, git_branch, cwd, request_id)
                VALUES (?1, ?2, 'assistant', ?3, ?4, 'claude_code', ?5, ?6, ?7, ?8, ?9, 'otel_exact', ?10, ?11, ?12, ?13)
                ON CONFLICT(id) DO UPDATE SET
                    cost_cents = excluded.cost_cents,
                    cost_confidence = excluded.cost_confidence,
                    input_tokens = excluded.input_tokens,
                    output_tokens = excluded.output_tokens,
                    cache_creation_tokens = excluded.cache_creation_tokens,
                    cache_read_tokens = excluded.cache_read_tokens,
                    model = excluded.model,
                    request_id = COALESCE(messages.request_id, excluded.request_id)
                WHERE messages.cost_confidence != 'otel_exact'",
                params![
                    otel_event_uuid,
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
                    event.request_id,
                ],
            )?;
            upserted += 1;
            otel_event_uuid.clone()
        };

        // Insert stub session if it doesn't exist yet (hooks may arrive later)
        tx.execute(
            "INSERT OR IGNORE INTO sessions (id, provider) VALUES (?1, 'claude_code')",
            params![event.session_id],
        )?;

        // Store raw event in otel_events for debugging
        tx.execute(
            "INSERT INTO otel_events (
                event_name, session_id, timestamp, raw_json, processed,
                message_id, timestamp_nano, model, cost_usd_reported, cost_cents_computed
            )
            VALUES ('claude_code.api_request', ?1, ?2, ?3, 1, ?4, ?5, ?6, ?7, ?8)",
            params![
                event.session_id,
                ts,
                snapshot_json,
                message_id,
                event.timestamp_nano,
                event.model,
                event.cost_usd,
                cost_cents
            ],
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
                                {"key": "cache_creation_tokens", "value": {"intValue": "5000"}}
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
                                {"key": "cache_creation_tokens", "value": {"intValue": "0"}}
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
        assert_eq!(events[0].request_id, None);
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
    fn parse_otel_logs_extracts_request_id_from_supported_keys() {
        let payload: ExportLogsServiceRequest = serde_json::from_value(serde_json::json!({
            "resourceLogs": [{
                "resource": {
                    "attributes": [
                        {"key": "session.id", "value": {"stringValue": "sess-req"}}
                    ]
                },
                "scopeLogs": [{
                    "logRecords": [{
                        "timeUnixNano": "1711500000000000000",
                        "body": {"stringValue": "claude_code.api_request"},
                        "attributes": [
                            {"key": "model", "value": {"stringValue": "claude-opus-4-6"}},
                            {"key": "message.id", "value": {"stringValue": "msg_otel_1"}},
                            {"key": "input_tokens", "value": {"intValue": "500"}},
                            {"key": "output_tokens", "value": {"intValue": "100"}}
                        ]
                    }]
                }]
            }]
        }))
        .unwrap();

        let events = parse_otel_logs(&payload);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].request_id.as_deref(), Some("msg_otel_1"));
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

        let linked_count: i64 = conn
            .query_row(
                "SELECT count(*) FROM otel_events WHERE message_id IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(linked_count, 2);
    }

    #[test]
    fn otel_upgrades_existing_jsonl_row() {
        let mut conn = setup_db();

        // Simulate JSONL sync: insert a message with estimated cost and a JSONL UUID
        let jsonl_uuid = "abc12345-jsonl-uuid-0000-000000000001";
        conn.execute(
            "INSERT INTO messages (id, session_id, role, timestamp, model, provider,
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
            request_id: None,
            cost_usd: 0.07,
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: 50000,
            cache_creation_tokens: 5000,
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
                "SELECT cost_cents, cost_confidence, input_tokens FROM messages WHERE id = ?1",
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

        let linked_uuid: Option<String> = conn
            .query_row(
                "SELECT message_id FROM otel_events ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(linked_uuid.as_deref(), Some(jsonl_uuid));
    }

    #[test]
    fn otel_prefers_fingerprint_match_when_multiple_jsonl_candidates_exist() {
        let mut conn = setup_db();

        conn.execute(
            "INSERT INTO messages (id, session_id, role, timestamp, model, provider,
                input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
                cost_cents, cost_confidence)
             VALUES ('jsonl-a', 'sess-fp', 'assistant', '2024-03-27T00:00:00.050Z', 'claude-opus-4-6',
                     'claude_code', 100, 20, 0, 0, 1.0, 'estimated')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, session_id, role, timestamp, model, provider,
                input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
                cost_cents, cost_confidence)
             VALUES ('jsonl-b', 'sess-fp', 'assistant', '2024-03-27T00:00:00.120Z', 'claude-opus-4-6',
                     'claude_code', 900, 400, 5000, 50000, 3.5, 'estimated')",
            [],
        )
        .unwrap();

        let events = vec![OtelApiRequest {
            session_id: "sess-fp".to_string(),
            timestamp: DateTime::parse_from_rfc3339("2024-03-27T00:00:00.180Z")
                .unwrap()
                .with_timezone(&Utc),
            timestamp_nano: "1711500000180000000".to_string(),
            model: "claude-opus-4-6".to_string(),
            request_id: None,
            cost_usd: 0.07,
            input_tokens: 900,
            output_tokens: 400,
            cache_read_tokens: 50000,
            cache_creation_tokens: 5000,
        }];

        let count = ingest_otel_events(&mut conn, &events).unwrap();
        assert_eq!(count, 1);

        let (a_conf, b_conf): (String, String) = conn
            .query_row(
                "SELECT
                    (SELECT cost_confidence FROM messages WHERE id = 'jsonl-a'),
                    (SELECT cost_confidence FROM messages WHERE id = 'jsonl-b')",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(a_conf, "estimated");
        assert_eq!(b_conf, "otel_exact");

        let total: i64 = conn
            .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 2);

        let linked: Option<String> = conn
            .query_row(
                "SELECT message_id FROM otel_events ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(linked.as_deref(), Some("jsonl-b"));
    }

    #[test]
    fn otel_inserts_new_row_when_jsonl_fingerprint_is_ambiguous() {
        let mut conn = setup_db();

        for id in ["jsonl-1", "jsonl-2"] {
            conn.execute(
                "INSERT INTO messages (id, session_id, role, timestamp, model, provider,
                    input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
                    cost_cents, cost_confidence)
                 VALUES (?1, 'sess-ambig', 'assistant', '2024-03-27T00:00:00.100Z', 'claude-opus-4-6',
                         'claude_code', 1000, 500, 5000, 50000, 3.0, 'estimated')",
                params![id],
            )
            .unwrap();
        }

        let events = vec![OtelApiRequest {
            session_id: "sess-ambig".to_string(),
            timestamp: DateTime::parse_from_rfc3339("2024-03-27T00:00:00.200Z")
                .unwrap()
                .with_timezone(&Utc),
            timestamp_nano: "1711500000200000000".to_string(),
            model: "claude-opus-4-6".to_string(),
            request_id: None,
            cost_usd: 0.07,
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: 50000,
            cache_creation_tokens: 5000,
        }];

        let count = ingest_otel_events(&mut conn, &events).unwrap();
        assert_eq!(count, 1);

        let total: i64 = conn
            .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 3, "ambiguous candidates should not be merged");

        let exact_count: i64 = conn
            .query_row(
                "SELECT count(*) FROM messages WHERE cost_confidence = 'otel_exact'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(exact_count, 1);

        let inserted_id = otel_uuid("sess-ambig", "1711500000200000000");
        let exists: i64 = conn
            .query_row(
                "SELECT count(*) FROM messages WHERE id = ?1",
                params![inserted_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1);
    }

    #[test]
    fn otel_does_not_overwrite_existing_otel_data() {
        let mut conn = setup_db();

        // Insert OTEL data (no existing JSONL row, so it inserts with deterministic OTEL UUID)
        let events = vec![OtelApiRequest {
            session_id: "sess-keep".to_string(),
            timestamp: DateTime::parse_from_rfc3339("2024-03-27T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            timestamp_nano: "1711500000000000000".to_string(),
            model: "claude-opus-4-6".to_string(),
            request_id: None,
            cost_usd: 0.05,
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: 50000,
            cache_creation_tokens: 5000,
        }];
        ingest_otel_events(&mut conn, &events).unwrap();

        // Send a second OTEL event in the same narrow time window.
        // With only one otel_exact candidate present, constrained fallback should
        // treat it as the same API call and keep the original exact data.
        let events2 = vec![OtelApiRequest {
            session_id: "sess-keep".to_string(),
            timestamp: DateTime::parse_from_rfc3339("2024-03-27T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            // Different timestamp_nano intentionally avoids deterministic-id matching.
            timestamp_nano: "1711500000100000000".to_string(),
            model: "claude-opus-4-6".to_string(),
            request_id: None,
            cost_usd: 0.99, // different cost
            input_tokens: 9999,
            output_tokens: 9999,
            cache_read_tokens: 9999,
            cache_creation_tokens: 9999,
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

        let linked_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM otel_events WHERE message_id IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(linked_rows, 2);
    }

    #[test]
    fn session_correlation_provides_git_context() {
        let mut conn = setup_db();

        // Create a session with git context (as hooks would)
        conn.execute(
            "INSERT INTO sessions (id, provider, repo_id, git_branch, workspace_root)
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
            request_id: None,
            cost_usd: 0.03,
            input_tokens: 500,
            output_tokens: 200,
            cache_read_tokens: 10000,
            cache_creation_tokens: 1000,
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
    fn ingest_otel_events_with_omit_policy_drops_raw_snapshot() {
        let mut conn = setup_db();
        let events = vec![OtelApiRequest {
            session_id: "sess-privacy-omit".to_string(),
            timestamp: DateTime::parse_from_rfc3339("2024-03-27T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            timestamp_nano: "1711500000000000000".to_string(),
            model: "claude-opus-4-6".to_string(),
            request_id: Some("msg_abc123".to_string()),
            cost_usd: 0.05,
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: 50000,
            cache_creation_tokens: 5000,
        }];
        let policy = crate::privacy::PrivacyPolicy {
            mode: crate::privacy::PrivacyMode::Omit,
            raw_retention_days: None,
            session_metadata_retention_days: None,
        };

        ingest_otel_events_with_policy(&mut conn, &events, &policy).unwrap();

        let raw_json: Option<String> = conn
            .query_row("SELECT raw_json FROM otel_events LIMIT 1", [], |r| r.get(0))
            .unwrap();
        assert!(raw_json.is_none());
    }

    #[test]
    fn ingest_otel_events_with_hash_policy_hashes_sensitive_raw_fields() {
        let mut conn = setup_db();
        let events = vec![OtelApiRequest {
            session_id: "sess-privacy-hash".to_string(),
            timestamp: DateTime::parse_from_rfc3339("2024-03-27T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            timestamp_nano: "1711500000000000000".to_string(),
            model: "claude-opus-4-6".to_string(),
            request_id: Some("msg_secret".to_string()),
            cost_usd: 0.05,
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: 50000,
            cache_creation_tokens: 5000,
        }];
        let policy = crate::privacy::PrivacyPolicy {
            mode: crate::privacy::PrivacyMode::Hash,
            raw_retention_days: None,
            session_metadata_retention_days: None,
        };

        ingest_otel_events_with_policy(&mut conn, &events, &policy).unwrap();

        let raw_json: Option<String> = conn
            .query_row("SELECT raw_json FROM otel_events LIMIT 1", [], |r| r.get(0))
            .unwrap();
        let raw_json = raw_json.expect("raw_json should be present in hash mode");
        assert!(!raw_json.contains("sess-privacy-hash"));
        assert!(!raw_json.contains("msg_secret"));
        assert!(raw_json.contains("sha256:"));
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
        assert_eq!(uuid1.len(), 32);
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
