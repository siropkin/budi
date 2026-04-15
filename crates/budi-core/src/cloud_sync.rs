//! Cloud sync worker: pushes scrubbed local rollups to the cloud ingest API.
//!
//! Per ADR-0083: only pre-aggregated metrics (daily rollups and session summaries)
//! cross the wire. Prompts, code, responses, file paths, emails, raw payloads,
//! tag values, and tool details are **never uploaded**.
//!
//! The sync worker runs as a background task in the daemon on a configurable
//! interval (default 300s). It is disabled by default and requires explicit
//! opt-in via `~/.config/budi/cloud.toml` configuration.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::config::CloudConfig;

// ---------------------------------------------------------------------------
// Sync envelope types (ADR-0083 §2)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct SyncEnvelope {
    pub schema_version: u32,
    pub device_id: String,
    pub org_id: String,
    pub synced_at: String,
    pub payload: SyncPayload,
}

#[derive(Debug, Clone, Serialize)]
pub struct SyncPayload {
    pub daily_rollups: Vec<DailyRollupRecord>,
    pub session_summaries: Vec<SessionSummaryRecord>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DailyRollupRecord {
    pub bucket_day: String,
    pub role: String,
    pub provider: String,
    pub model: String,
    pub repo_id: String,
    pub git_branch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ticket: Option<String>,
    pub message_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_creation_tokens: i64,
    pub cache_read_tokens: i64,
    pub cost_cents: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionSummaryRecord {
    pub session_id: String,
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ticket: Option<String>,
    pub message_count: i64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub total_cost_cents: f64,
}

/// Server response from `POST /v1/ingest` (ADR-0083 §5).
#[derive(Debug, Clone, Deserialize)]
pub struct IngestResponse {
    pub accepted: bool,
    pub watermark: Option<String>,
    pub records_upserted: Option<i64>,
}

// ---------------------------------------------------------------------------
// Watermark tracking (ADR-0083 §5)
// ---------------------------------------------------------------------------

/// Sentinel key in `sync_state` table for the cloud sync watermark.
pub const CLOUD_SYNC_WATERMARK_KEY: &str = "__budi_cloud_sync__";

/// Sentinel key for tracking the last session sync timestamp.
pub const CLOUD_SYNC_SESSION_WATERMARK_KEY: &str = "__budi_cloud_sync_sessions__";

/// Get the current cloud sync watermark (latest fully-synced bucket_day).
pub fn get_cloud_watermark(conn: &Connection) -> Result<Option<String>> {
    match conn.query_row(
        "SELECT last_synced FROM sync_state WHERE file_path = ?1",
        params![CLOUD_SYNC_WATERMARK_KEY],
        |r| r.get::<_, String>(0),
    ) {
        Ok(val) => Ok(Some(val)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Update the cloud sync watermark after server confirmation.
pub fn set_cloud_watermark(conn: &Connection, watermark: &str) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO sync_state (file_path, byte_offset, last_synced)
         VALUES (?1, 0, ?2)
         ON CONFLICT(file_path) DO UPDATE SET last_synced = ?2",
        params![CLOUD_SYNC_WATERMARK_KEY, now],
    )?;
    // Store the actual watermark date in byte_offset is not ideal;
    // instead we store the watermark value in a second key.
    conn.execute(
        "INSERT INTO sync_state (file_path, byte_offset, last_synced)
         VALUES (?1, 0, ?2)
         ON CONFLICT(file_path) DO UPDATE SET last_synced = ?2",
        params![format!("{CLOUD_SYNC_WATERMARK_KEY}_value"), watermark],
    )?;
    Ok(())
}

/// Get the stored watermark date value (bucket_day string like "2026-04-10").
pub fn get_cloud_watermark_value(conn: &Connection) -> Result<Option<String>> {
    match conn.query_row(
        "SELECT last_synced FROM sync_state WHERE file_path = ?1",
        params![format!("{CLOUD_SYNC_WATERMARK_KEY}_value")],
        |r| r.get::<_, String>(0),
    ) {
        Ok(val) => Ok(Some(val)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Get the session sync watermark (ISO 8601 timestamp).
pub fn get_session_watermark(conn: &Connection) -> Result<Option<String>> {
    match conn.query_row(
        "SELECT last_synced FROM sync_state WHERE file_path = ?1",
        params![CLOUD_SYNC_SESSION_WATERMARK_KEY],
        |r| r.get::<_, String>(0),
    ) {
        Ok(val) => Ok(Some(val)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Update the session sync watermark.
pub fn set_session_watermark(conn: &Connection, timestamp: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO sync_state (file_path, byte_offset, last_synced)
         VALUES (?1, 0, ?2)
         ON CONFLICT(file_path) DO UPDATE SET last_synced = ?2",
        params![CLOUD_SYNC_SESSION_WATERMARK_KEY, timestamp],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Data extraction from local SQLite (privacy-safe: rollups + session summaries)
// ---------------------------------------------------------------------------

/// Extract ticket ID from a git branch name (e.g. "feature/PROJ-1234-add-auth" → "PROJ-1234").
fn extract_ticket_from_branch(branch: &str) -> Option<String> {
    // Common patterns: PROJ-123, ABC-1234, etc.
    let re_like = |s: &str| -> Option<String> {
        let mut start = None;
        let chars: Vec<char> = s.chars().collect();
        for i in 0..chars.len() {
            if chars[i].is_ascii_uppercase() {
                if start.is_none() {
                    start = Some(i);
                }
            } else if chars[i] == '-' {
                if let Some(s) = start {
                    // Check if what follows is digits
                    let rest = &chars[i + 1..];
                    let digits: String = rest.iter().take_while(|c| c.is_ascii_digit()).collect();
                    if !digits.is_empty() {
                        let prefix: String = chars[s..i].iter().collect();
                        if prefix.len() >= 2 {
                            return Some(format!("{prefix}-{digits}"));
                        }
                    }
                }
                start = None;
            } else if !chars[i].is_ascii_alphanumeric() {
                start = None;
            }
        }
        None
    };

    // Try the branch name after any "/" delimiter
    for segment in branch.split('/') {
        if let Some(ticket) = re_like(segment) {
            return Some(ticket);
        }
    }
    re_like(branch)
}

/// Fetch daily rollups that need syncing.
/// Per ADR-0083 §5:
/// 1. All rollups where bucket_day > watermark
/// 2. Current day's rollups (always re-sent)
pub fn fetch_daily_rollups(
    conn: &Connection,
    watermark: Option<&str>,
) -> Result<Vec<DailyRollupRecord>> {
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();

    let mut records = Vec::new();

    // Build query based on watermark presence
    let rows: Vec<DailyRollupRecord> = if let Some(wm) = watermark {
        let mut stmt = conn.prepare(
            "SELECT bucket_day, role, provider, model, repo_id, git_branch,
                    message_count, input_tokens, output_tokens,
                    cache_creation_tokens, cache_read_tokens, cost_cents
             FROM message_rollups_daily
             WHERE bucket_day > ?1 OR bucket_day = ?2
             ORDER BY bucket_day",
        )?;
        stmt.query_map(params![wm, today], |row| {
            Ok(DailyRollupRecord {
                bucket_day: row.get(0)?,
                role: row.get(1)?,
                provider: row.get(2)?,
                model: row.get(3)?,
                repo_id: row.get(4)?,
                git_branch: row.get(5)?,
                ticket: None,
                message_count: row.get(6)?,
                input_tokens: row.get(7)?,
                output_tokens: row.get(8)?,
                cache_creation_tokens: row.get(9)?,
                cache_read_tokens: row.get(10)?,
                cost_cents: row.get(11)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect()
    } else {
        // No watermark: send everything
        let mut stmt = conn.prepare(
            "SELECT bucket_day, role, provider, model, repo_id, git_branch,
                    message_count, input_tokens, output_tokens,
                    cache_creation_tokens, cache_read_tokens, cost_cents
             FROM message_rollups_daily
             ORDER BY bucket_day",
        )?;
        stmt.query_map([], |row| {
            Ok(DailyRollupRecord {
                bucket_day: row.get(0)?,
                role: row.get(1)?,
                provider: row.get(2)?,
                model: row.get(3)?,
                repo_id: row.get(4)?,
                git_branch: row.get(5)?,
                ticket: None,
                message_count: row.get(6)?,
                input_tokens: row.get(7)?,
                output_tokens: row.get(8)?,
                cache_creation_tokens: row.get(9)?,
                cache_read_tokens: row.get(10)?,
                cost_cents: row.get(11)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect()
    };

    for mut record in rows {
        record.ticket = extract_ticket_from_branch(&record.git_branch);
        records.push(record);
    }

    Ok(records)
}

/// Fetch session summaries that need syncing.
/// Computes aggregates from messages per session — never reads sensitive session fields.
pub fn fetch_session_summaries(
    conn: &Connection,
    since: Option<&str>,
) -> Result<Vec<SessionSummaryRecord>> {
    let query = if since.is_some() {
        "SELECT s.id, s.provider, s.started_at, s.ended_at, s.duration_ms,
                s.repo_id, s.git_branch,
                COALESCE(m.msg_count, 0),
                COALESCE(m.total_input, 0),
                COALESCE(m.total_output, 0),
                COALESCE(m.total_cost, 0.0)
         FROM sessions s
         LEFT JOIN (
             SELECT session_id,
                    COUNT(*) as msg_count,
                    SUM(input_tokens) as total_input,
                    SUM(output_tokens) as total_output,
                    SUM(COALESCE(cost_cents, 0.0)) as total_cost
             FROM messages
             WHERE role = 'assistant'
             GROUP BY session_id
         ) m ON m.session_id = s.id
         WHERE s.started_at > ?1 OR s.ended_at > ?1
            OR (s.ended_at IS NULL AND s.started_at IS NOT NULL)
         ORDER BY s.started_at"
    } else {
        "SELECT s.id, s.provider, s.started_at, s.ended_at, s.duration_ms,
                s.repo_id, s.git_branch,
                COALESCE(m.msg_count, 0),
                COALESCE(m.total_input, 0),
                COALESCE(m.total_output, 0),
                COALESCE(m.total_cost, 0.0)
         FROM sessions s
         LEFT JOIN (
             SELECT session_id,
                    COUNT(*) as msg_count,
                    SUM(input_tokens) as total_input,
                    SUM(output_tokens) as total_output,
                    SUM(COALESCE(cost_cents, 0.0)) as total_cost
             FROM messages
             WHERE role = 'assistant'
             GROUP BY session_id
         ) m ON m.session_id = s.id
         WHERE s.started_at IS NOT NULL
         ORDER BY s.started_at"
    };

    let mut stmt = conn.prepare(query)?;
    let rows = if let Some(ts) = since {
        stmt.query_map(params![ts], map_session_row)?
    } else {
        stmt.query_map([], map_session_row)?
    };

    let mut summaries = Vec::new();
    for mut summary in rows.flatten() {
        summary.ticket = summary
            .git_branch
            .as_deref()
            .and_then(extract_ticket_from_branch);
        summaries.push(summary);
    }

    Ok(summaries)
}

fn map_session_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionSummaryRecord> {
    Ok(SessionSummaryRecord {
        session_id: row.get(0)?,
        provider: row.get(1)?,
        started_at: row.get(2)?,
        ended_at: row.get(3)?,
        duration_ms: row.get(4)?,
        repo_id: row.get(5)?,
        git_branch: row.get(6)?,
        ticket: None,
        message_count: row.get(7)?,
        total_input_tokens: row.get(8)?,
        total_output_tokens: row.get(9)?,
        total_cost_cents: row.get(10)?,
    })
}

/// Build the complete sync envelope from local data.
pub fn build_sync_envelope(conn: &Connection, config: &CloudConfig) -> Result<SyncEnvelope> {
    let device_id = config
        .device_id
        .as_ref()
        .context("device_id not configured")?
        .clone();
    let org_id = config
        .org_id
        .as_ref()
        .context("org_id not configured")?
        .clone();

    // Read watermarks
    let rollup_watermark = get_cloud_watermark_value(conn)?;
    let session_watermark = get_session_watermark(conn)?;

    // Fetch data
    let daily_rollups = fetch_daily_rollups(conn, rollup_watermark.as_deref())?;
    let session_summaries = fetch_session_summaries(conn, session_watermark.as_deref())?;

    Ok(SyncEnvelope {
        schema_version: 1,
        device_id,
        org_id,
        synced_at: chrono::Utc::now().to_rfc3339(),
        payload: SyncPayload {
            daily_rollups,
            session_summaries,
        },
    })
}

// ---------------------------------------------------------------------------
// HTTP client with HTTPS enforcement and retry logic (ADR-0083 §4)
// ---------------------------------------------------------------------------

/// Result of a sync attempt.
#[derive(Debug)]
pub enum SyncResult {
    /// Server accepted the payload.
    Success(IngestResponse),
    /// Auth failure (401) — should stop syncing and prompt re-auth.
    AuthFailure,
    /// Schema mismatch (422) — log warning, don't retry until updated.
    SchemaMismatch(String),
    /// Transient error (429/5xx/network) — should retry with backoff.
    TransientError(String),
    /// Nothing to sync (empty payload).
    EmptyPayload,
}

/// Validate that the endpoint is HTTPS. ADR-0083 §4: "The daemon refuses to sync
/// over plain HTTP (hard-coded check)."
pub fn validate_https_endpoint(endpoint: &str) -> Result<()> {
    if !endpoint.starts_with("https://") {
        anyhow::bail!("Cloud sync endpoint must use HTTPS. Refusing to sync to: {endpoint}");
    }
    Ok(())
}

/// Send the sync envelope to the cloud ingest API.
/// Uses `ureq` (blocking) — call from `spawn_blocking`.
pub fn send_sync_envelope(endpoint: &str, api_key: &str, envelope: &SyncEnvelope) -> SyncResult {
    if envelope.payload.daily_rollups.is_empty() && envelope.payload.session_summaries.is_empty() {
        return SyncResult::EmptyPayload;
    }

    if let Err(e) = validate_https_endpoint(endpoint) {
        return SyncResult::TransientError(e.to_string());
    }

    let url = format!("{endpoint}/v1/ingest");

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into();

    let result = agent
        .post(&url)
        .header("Authorization", &format!("Bearer {api_key}"))
        .send_json(envelope);

    match result {
        Ok(mut response) => match response.body_mut().read_json::<IngestResponse>() {
            Ok(resp) => SyncResult::Success(resp),
            Err(e) => SyncResult::TransientError(format!("Failed to parse response: {e}")),
        },
        Err(ureq::Error::StatusCode(401)) => SyncResult::AuthFailure,
        Err(ureq::Error::StatusCode(422)) => {
            SyncResult::SchemaMismatch("Server returned 422".to_string())
        }
        Err(ureq::Error::StatusCode(status)) if status == 429 || status >= 500 => {
            SyncResult::TransientError(format!("Server returned {status}"))
        }
        Err(ureq::Error::StatusCode(status)) => {
            SyncResult::TransientError(format!("Server returned {status}"))
        }
        Err(e) => SyncResult::TransientError(format!("Network error: {e}")),
    }
}

/// Execute a single sync tick: build envelope, send, update watermark.
/// Blocking — call from `spawn_blocking`.
pub fn sync_tick(db_path: &Path, config: &CloudConfig) -> SyncResult {
    let conn = match crate::analytics::open_db(db_path) {
        Ok(c) => c,
        Err(e) => return SyncResult::TransientError(format!("Failed to open DB: {e}")),
    };

    let envelope = match build_sync_envelope(&conn, config) {
        Ok(e) => e,
        Err(e) => return SyncResult::TransientError(format!("Failed to build envelope: {e}")),
    };

    if envelope.payload.daily_rollups.is_empty() && envelope.payload.session_summaries.is_empty() {
        return SyncResult::EmptyPayload;
    }

    let api_key = match config.effective_api_key() {
        Some(k) => k,
        None => return SyncResult::AuthFailure,
    };

    let endpoint = config.effective_endpoint();

    let result = send_sync_envelope(&endpoint, &api_key, &envelope);

    // On success, update watermarks (ADR-0083 §5)
    if let SyncResult::Success(ref resp) = result {
        if let Some(ref wm) = resp.watermark
            && let Err(e) = set_cloud_watermark(&conn, wm)
        {
            tracing::warn!("Failed to update cloud watermark: {e}");
        }
        // Update session watermark to current time
        let now = chrono::Utc::now().to_rfc3339();
        if let Err(e) = set_session_watermark(&conn, &now) {
            tracing::warn!("Failed to update session watermark: {e}");
        }
    }

    result
}

/// Calculate exponential backoff delay.
/// 1s → 2s → 4s → 8s → ... → retry_max_seconds cap.
pub fn backoff_delay(attempt: u32, retry_max_seconds: u64) -> Duration {
    let base_secs = 1u64.checked_shl(attempt).unwrap_or(retry_max_seconds);
    let capped = base_secs.min(retry_max_seconds);
    Duration::from_secs(capped)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_ticket_basic() {
        assert_eq!(
            extract_ticket_from_branch("feature/PROJ-1234-add-auth"),
            Some("PROJ-1234".to_string())
        );
        assert_eq!(
            extract_ticket_from_branch("PROJ-1234"),
            Some("PROJ-1234".to_string())
        );
        assert_eq!(
            extract_ticket_from_branch("fix/ABC-42-hotfix"),
            Some("ABC-42".to_string())
        );
        assert_eq!(extract_ticket_from_branch("main"), None);
        assert_eq!(extract_ticket_from_branch("(untagged)"), None);
    }

    #[test]
    fn https_enforcement() {
        assert!(validate_https_endpoint("https://app.getbudi.dev").is_ok());
        assert!(validate_https_endpoint("http://app.getbudi.dev").is_err());
        assert!(validate_https_endpoint("ftp://example.com").is_err());
    }

    #[test]
    fn backoff_delay_escalation() {
        assert_eq!(backoff_delay(0, 300), Duration::from_secs(1));
        assert_eq!(backoff_delay(1, 300), Duration::from_secs(2));
        assert_eq!(backoff_delay(2, 300), Duration::from_secs(4));
        assert_eq!(backoff_delay(3, 300), Duration::from_secs(8));
        assert_eq!(backoff_delay(10, 300), Duration::from_secs(300)); // Capped
        assert_eq!(backoff_delay(20, 300), Duration::from_secs(300)); // Capped
    }

    #[test]
    fn empty_payload_detected() {
        let result = send_sync_envelope(
            "https://app.getbudi.dev",
            "budi_test",
            &SyncEnvelope {
                schema_version: 1,
                device_id: "dev_test".into(),
                org_id: "org_test".into(),
                synced_at: "2026-04-12T00:00:00Z".into(),
                payload: SyncPayload {
                    daily_rollups: vec![],
                    session_summaries: vec![],
                },
            },
        );
        assert!(matches!(result, SyncResult::EmptyPayload));
    }

    #[test]
    fn watermark_round_trip() {
        let dir = std::env::temp_dir().join("budi-cloud-sync-test-wm");
        std::fs::create_dir_all(&dir).ok();
        let db_path = dir.join("test.db");
        let _ = std::fs::remove_file(&db_path);

        let conn = crate::analytics::open_db_with_migration(&db_path).unwrap();

        // Initially no watermark
        assert!(get_cloud_watermark_value(&conn).unwrap().is_none());
        assert!(get_session_watermark(&conn).unwrap().is_none());

        // Set and read back
        set_cloud_watermark(&conn, "2026-04-10").unwrap();
        assert_eq!(
            get_cloud_watermark_value(&conn).unwrap().as_deref(),
            Some("2026-04-10")
        );

        set_session_watermark(&conn, "2026-04-10T10:00:00Z").unwrap();
        assert_eq!(
            get_session_watermark(&conn).unwrap().as_deref(),
            Some("2026-04-10T10:00:00Z")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fetch_rollups_empty_db() {
        let dir = std::env::temp_dir().join("budi-cloud-sync-test-rollups");
        std::fs::create_dir_all(&dir).ok();
        let db_path = dir.join("test.db");
        let _ = std::fs::remove_file(&db_path);

        let conn = crate::analytics::open_db_with_migration(&db_path).unwrap();
        let rollups = fetch_daily_rollups(&conn, None).unwrap();
        assert!(rollups.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fetch_rollups_with_data() {
        let dir = std::env::temp_dir().join("budi-cloud-sync-test-rollups-data");
        std::fs::create_dir_all(&dir).ok();
        let db_path = dir.join("test.db");
        let _ = std::fs::remove_file(&db_path);

        let conn = crate::analytics::open_db_with_migration(&db_path).unwrap();

        // Insert a message to trigger the rollup trigger
        conn.execute(
            "INSERT INTO messages (id, role, timestamp, model, provider, repo_id, git_branch,
                                   input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens, cost_cents)
             VALUES ('msg-1', 'assistant', '2026-04-10T14:30:00Z', 'claude-sonnet-4-6', 'anthropic',
                     'sha256:abc123', 'feature/PROJ-42-auth', 100, 200, 10, 50, 1.5)",
            [],
        ).unwrap();

        // Fetch all rollups (no watermark)
        let rollups = fetch_daily_rollups(&conn, None).unwrap();
        assert_eq!(rollups.len(), 1);
        assert_eq!(rollups[0].bucket_day, "2026-04-10");
        assert_eq!(rollups[0].model, "claude-sonnet-4-6");
        assert_eq!(rollups[0].input_tokens, 100);
        assert_eq!(rollups[0].output_tokens, 200);
        assert_eq!(rollups[0].ticket.as_deref(), Some("PROJ-42"));

        // Fetch with watermark that excludes the data
        let rollups = fetch_daily_rollups(&conn, Some("2026-04-10")).unwrap();
        // The watermark is "2026-04-10" and today is after it, so we only get
        // records where bucket_day > watermark OR bucket_day == today.
        // Since the record is from 2026-04-10 and today != 2026-04-10,
        // we should get 0 (bucket_day is not > watermark, and it's not today).
        assert!(rollups.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fetch_session_summaries_empty_db() {
        let dir = std::env::temp_dir().join("budi-cloud-sync-test-sessions");
        std::fs::create_dir_all(&dir).ok();
        let db_path = dir.join("test.db");
        let _ = std::fs::remove_file(&db_path);

        let conn = crate::analytics::open_db_with_migration(&db_path).unwrap();
        let summaries = fetch_session_summaries(&conn, None).unwrap();
        assert!(summaries.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_envelope_requires_config() {
        let dir = std::env::temp_dir().join("budi-cloud-sync-test-envelope");
        std::fs::create_dir_all(&dir).ok();
        let db_path = dir.join("test.db");
        let _ = std::fs::remove_file(&db_path);

        let conn = crate::analytics::open_db_with_migration(&db_path).unwrap();
        let config = CloudConfig::default();

        // Should fail without device_id
        let result = build_sync_envelope(&conn, &config);
        assert!(result.is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_envelope_success() {
        let dir = std::env::temp_dir().join("budi-cloud-sync-test-envelope-ok");
        std::fs::create_dir_all(&dir).ok();
        let db_path = dir.join("test.db");
        let _ = std::fs::remove_file(&db_path);

        let conn = crate::analytics::open_db_with_migration(&db_path).unwrap();
        let config = CloudConfig {
            enabled: true,
            api_key: Some("budi_test".into()),
            device_id: Some("dev_test".into()),
            org_id: Some("org_test".into()),
            ..CloudConfig::default()
        };

        let envelope = build_sync_envelope(&conn, &config).unwrap();
        assert_eq!(envelope.schema_version, 1);
        assert_eq!(envelope.device_id, "dev_test");
        assert_eq!(envelope.org_id, "org_test");
        assert!(envelope.payload.daily_rollups.is_empty());
        assert!(envelope.payload.session_summaries.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn envelope_serializes_to_expected_shape() {
        let envelope = SyncEnvelope {
            schema_version: 1,
            device_id: "dev_test".into(),
            org_id: "org_test".into(),
            synced_at: "2026-04-12T00:00:00Z".into(),
            payload: SyncPayload {
                daily_rollups: vec![DailyRollupRecord {
                    bucket_day: "2026-04-10".into(),
                    role: "assistant".into(),
                    provider: "claude_code".into(),
                    model: "claude-sonnet-4-6".into(),
                    repo_id: "sha256:abc".into(),
                    git_branch: "main".into(),
                    ticket: None,
                    message_count: 5,
                    input_tokens: 1000,
                    output_tokens: 500,
                    cache_creation_tokens: 100,
                    cache_read_tokens: 200,
                    cost_cents: 2.5,
                }],
                session_summaries: vec![],
            },
        };

        let json = serde_json::to_value(&envelope).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["device_id"], "dev_test");
        assert_eq!(
            json["payload"]["daily_rollups"][0]["bucket_day"],
            "2026-04-10"
        );
        // ticket should be absent (None → skipped)
        assert!(json["payload"]["daily_rollups"][0].get("ticket").is_none());
    }
}
