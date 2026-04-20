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
    /// Provenance marker matching the canonical pipeline extractor
    /// (`branch` or `branch_numeric`). Only set when `ticket` is `Some`,
    /// so cloud-side dashboards can distinguish the two sources the same
    /// way local `budi stats --tickets` does.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ticket_source: Option<String>,
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
    /// Provenance marker matching the canonical pipeline extractor
    /// (`branch` or `branch_numeric`). Only set when `ticket` is `Some`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ticket_source: Option<String>,
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

/// Snapshot of the cloud sync state for reporting via `budi cloud status`.
///
/// Captures configuration readiness, the last successful sync watermarks, and
/// how many records are waiting to be pushed on the next tick. Counts are
/// best-effort and can be reported without a live network call.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CloudSyncStatus {
    pub enabled: bool,
    pub configured: bool,
    pub ready: bool,
    pub endpoint: String,
    pub last_synced_at: Option<String>,
    pub rollup_watermark: Option<String>,
    pub pending_rollups: usize,
    pub pending_sessions: usize,
}

/// Read the current cloud sync status from local config and SQLite.
///
/// Never makes a network call — this is used by `budi cloud status` and the
/// daemon `/cloud/status` endpoint to report readiness and freshness at a
/// glance. Pending counts are computed by running the envelope builder
/// against the current watermarks; if envelope construction fails (e.g.
/// device_id/org_id missing), pending counts fall back to 0 and the caller
/// can still rely on `ready=false` to explain what is missing.
pub fn current_cloud_status(db_path: &Path, config: &CloudConfig) -> CloudSyncStatus {
    let endpoint = config.effective_endpoint();
    let enabled = config.effective_enabled();
    let ready = config.is_ready();
    let configured = config.api_key.is_some() || config.effective_api_key().is_some();

    let mut last_synced_at = None;
    let mut rollup_watermark = None;
    let mut pending_rollups = 0usize;
    let mut pending_sessions = 0usize;

    if let Ok(conn) = crate::analytics::open_db(db_path) {
        last_synced_at = get_session_watermark(&conn).ok().flatten();
        rollup_watermark = get_cloud_watermark_value(&conn).ok().flatten();
        if ready {
            // Per #344: avoid running `build_sync_envelope` just to take
            // two `.len()`s. Two bounded `COUNT(*)` queries against the
            // same predicates the envelope uses are far cheaper for
            // pollers that hit `/cloud/status` frequently.
            pending_rollups = count_pending_rollups(&conn, rollup_watermark.as_deref())
                .ok()
                .unwrap_or(0);
            pending_sessions = count_pending_sessions(&conn, last_synced_at.as_deref())
                .ok()
                .unwrap_or(0);
        }
    }

    CloudSyncStatus {
        enabled,
        configured,
        ready,
        endpoint,
        last_synced_at,
        rollup_watermark,
        pending_rollups,
        pending_sessions,
    }
}

// ---------------------------------------------------------------------------
// Data extraction from local SQLite (privacy-safe: rollups + session summaries)
// ---------------------------------------------------------------------------

/// Extract ticket ID and source provenance from a git branch name.
///
/// Delegates to `pipeline::extract_ticket_from_branch` so cloud rollups
/// apply the same filter / alpha-first / numeric-fallback rules as
/// analytics and `budi stats --tickets`. See ADR-0082 §9 and
/// issue #333 for context — cloud previously carried its own helper
/// that diverged on integration-branch filtering and the numeric
/// fallback.
fn extract_ticket(branch: &str) -> Option<(String, &'static str)> {
    crate::pipeline::extract_ticket_from_branch(branch)
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
                ticket_source: None,
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
                ticket_source: None,
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
        if let Some((id, source)) = extract_ticket(&record.git_branch) {
            record.ticket = Some(id);
            record.ticket_source = Some(source.to_string());
        }
        records.push(record);
    }

    Ok(records)
}

/// Count the number of daily rollups that would be pushed on the next sync.
///
/// Mirrors the predicate in [`fetch_daily_rollups`] exactly — rows where
/// `bucket_day > watermark` or `bucket_day = today` — so
/// `count_pending_rollups` and the envelope returned by
/// [`build_sync_envelope`] always agree on row count. Used by
/// [`current_cloud_status`] so frequent `/cloud/status` pollers avoid
/// materializing every unsynced row just to take `.len()` (#344).
pub fn count_pending_rollups(conn: &Connection, watermark: Option<&str>) -> Result<usize> {
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let count: i64 = if let Some(wm) = watermark {
        conn.query_row(
            "SELECT COUNT(*) FROM message_rollups_daily
             WHERE bucket_day > ?1 OR bucket_day = ?2",
            params![wm, today],
            |row| row.get(0),
        )?
    } else {
        conn.query_row("SELECT COUNT(*) FROM message_rollups_daily", [], |row| {
            row.get(0)
        })?
    };
    Ok(count.max(0) as usize)
}

/// Count the number of session summaries that would be pushed on the next
/// sync. Mirrors the predicate in [`fetch_session_summaries`] so the count
/// stays in lockstep with the envelope (#344).
pub fn count_pending_sessions(conn: &Connection, since: Option<&str>) -> Result<usize> {
    let count: i64 = if let Some(ts) = since {
        conn.query_row(
            "SELECT COUNT(*) FROM sessions s
             WHERE s.started_at > ?1 OR s.ended_at > ?1
                OR (s.ended_at IS NULL AND s.started_at IS NOT NULL)",
            params![ts],
            |row| row.get(0),
        )?
    } else {
        conn.query_row(
            "SELECT COUNT(*) FROM sessions s
             WHERE s.started_at IS NOT NULL",
            [],
            |row| row.get(0),
        )?
    };
    Ok(count.max(0) as usize)
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
        if let Some((id, source)) = summary.git_branch.as_deref().and_then(extract_ticket) {
            summary.ticket = Some(id);
            summary.ticket_source = Some(source.to_string());
        }
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
        ticket_source: None,
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

/// Structured report of a single sync tick, suitable for rendering in the
/// CLI (`budi cloud sync`) or returning as JSON from the daemon. Carries the
/// underlying [`SyncResult`] plus the envelope counts that were attempted and
/// any server-confirmed response fields.
///
/// Envelope counts are included so surfaces can say "pushed N records" even
/// when the server omits `records_upserted`, and so `EmptyPayload` responses
/// can still explain *why* nothing was pushed.
#[derive(Debug)]
pub struct SyncTickReport {
    pub result: SyncResult,
    pub endpoint: String,
    pub envelope_rollups: usize,
    pub envelope_sessions: usize,
    pub server_records_upserted: Option<i64>,
    pub server_watermark: Option<String>,
}

/// Execute a single sync tick: build envelope, send, update watermark.
/// Blocking — call from `spawn_blocking`.
pub fn sync_tick(db_path: &Path, config: &CloudConfig) -> SyncResult {
    sync_tick_report(db_path, config).result
}

/// Execute a single sync tick and return a structured report.
/// Used by the manual `budi cloud sync` path so the CLI can report how many
/// records were attempted and confirmed, not just the coarse [`SyncResult`]
/// variant. Shares all behavior with [`sync_tick`] so the manual and
/// background paths stay in lockstep.
pub fn sync_tick_report(db_path: &Path, config: &CloudConfig) -> SyncTickReport {
    let endpoint = config.effective_endpoint();

    let conn = match crate::analytics::open_db(db_path) {
        Ok(c) => c,
        Err(e) => {
            return SyncTickReport {
                result: SyncResult::TransientError(format!("Failed to open DB: {e}")),
                endpoint,
                envelope_rollups: 0,
                envelope_sessions: 0,
                server_records_upserted: None,
                server_watermark: None,
            };
        }
    };

    let envelope = match build_sync_envelope(&conn, config) {
        Ok(e) => e,
        Err(e) => {
            return SyncTickReport {
                result: SyncResult::TransientError(format!("Failed to build envelope: {e}")),
                endpoint,
                envelope_rollups: 0,
                envelope_sessions: 0,
                server_records_upserted: None,
                server_watermark: None,
            };
        }
    };

    let envelope_rollups = envelope.payload.daily_rollups.len();
    let envelope_sessions = envelope.payload.session_summaries.len();

    if envelope_rollups == 0 && envelope_sessions == 0 {
        return SyncTickReport {
            result: SyncResult::EmptyPayload,
            endpoint,
            envelope_rollups,
            envelope_sessions,
            server_records_upserted: None,
            server_watermark: None,
        };
    }

    let api_key = match config.effective_api_key() {
        Some(k) => k,
        None => {
            return SyncTickReport {
                result: SyncResult::AuthFailure,
                endpoint,
                envelope_rollups,
                envelope_sessions,
                server_records_upserted: None,
                server_watermark: None,
            };
        }
    };

    let result = send_sync_envelope(&endpoint, &api_key, &envelope);

    let mut server_records_upserted = None;
    let mut server_watermark = None;

    // On success, update watermarks (ADR-0083 §5)
    if let SyncResult::Success(ref resp) = result {
        server_records_upserted = resp.records_upserted;
        server_watermark = resp.watermark.clone();
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

    SyncTickReport {
        result,
        endpoint,
        envelope_rollups,
        envelope_sessions,
        server_records_upserted,
        server_watermark,
    }
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
        // After #333, cloud_sync delegates to `pipeline::extract_ticket_from_branch`;
        // keep the spot-checks in place to confirm the thin wrapper preserves
        // alpha-pattern, integration-branch, and non-branch-like behavior.
        assert_eq!(
            extract_ticket("feature/PROJ-1234-add-auth").map(|(id, _)| id),
            Some("PROJ-1234".to_string())
        );
        assert_eq!(
            extract_ticket("PROJ-1234").map(|(id, _)| id),
            Some("PROJ-1234".to_string())
        );
        assert_eq!(
            extract_ticket("fix/ABC-42-hotfix").map(|(id, _)| id),
            Some("ABC-42".to_string())
        );
        assert_eq!(extract_ticket("main"), None);
        assert_eq!(extract_ticket("(untagged)"), None);
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
        assert_eq!(
            rollups[0].ticket_source.as_deref(),
            Some(crate::pipeline::TICKET_SOURCE_BRANCH)
        );

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
    fn current_cloud_status_reports_disabled_when_config_default() {
        let dir = std::env::temp_dir().join("budi-cloud-status-disabled");
        std::fs::create_dir_all(&dir).ok();
        let db_path = dir.join("test.db");
        let _ = std::fs::remove_file(&db_path);
        let _ = crate::analytics::open_db_with_migration(&db_path).unwrap();

        let status = current_cloud_status(&db_path, &CloudConfig::default());
        assert!(!status.enabled);
        assert!(!status.ready);
        assert_eq!(status.pending_rollups, 0);
        assert_eq!(status.pending_sessions, 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn current_cloud_status_reports_pending_counts_when_ready() {
        let dir = std::env::temp_dir().join("budi-cloud-status-ready");
        std::fs::create_dir_all(&dir).ok();
        let db_path = dir.join("test.db");
        let _ = std::fs::remove_file(&db_path);
        let conn = crate::analytics::open_db_with_migration(&db_path).unwrap();

        conn.execute(
            "INSERT INTO messages (id, role, timestamp, model, provider, repo_id, git_branch,
                                   input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens, cost_cents)
             VALUES ('msg-status-1', 'assistant', '2026-04-10T14:30:00Z', 'claude-sonnet-4-6', 'anthropic',
                     'sha256:abc', 'main', 100, 200, 10, 50, 1.5)",
            [],
        )
        .unwrap();

        let config = CloudConfig {
            enabled: true,
            api_key: Some("budi_test".into()),
            device_id: Some("dev_test".into()),
            org_id: Some("org_test".into()),
            ..CloudConfig::default()
        };
        let status = current_cloud_status(&db_path, &config);
        assert!(status.enabled);
        assert!(status.ready);
        assert!(status.pending_rollups >= 1);

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
                    ticket_source: None,
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
        // ticket_source should also be absent when ticket is None
        assert!(
            json["payload"]["daily_rollups"][0]
                .get("ticket_source")
                .is_none()
        );
    }

    // Regression for #333: cloud_sync must produce the same ticket_id as the
    // canonical pipeline extractor on the divergent cases that motivated the
    // ticket — the numeric fallback and the nested alphanumeric form — and
    // integration branches must not leak a ticket to the cloud.
    #[test]
    fn rollup_extraction_matches_pipeline_extractor() {
        let cases = [
            "feature/1234",
            "bugfix/ENG-99/refactor",
            "feature/PROJ-42-auth",
            "42-stabilize-auth",
            "main",
            "master",
            "develop",
            "HEAD",
            "kiyoshi/pava-searchbars", // no ticket at all
        ];
        for branch in cases {
            let pipeline = crate::pipeline::extract_ticket_from_branch(branch);
            let local = extract_ticket(branch);
            assert_eq!(
                pipeline, local,
                "cloud_sync extractor diverged from pipeline for {branch:?}"
            );
        }
    }

    #[test]
    fn rollup_numeric_branch_preserves_source_marker() {
        let dir = std::env::temp_dir().join("budi-cloud-sync-test-ticket-source");
        std::fs::create_dir_all(&dir).ok();
        let db_path = dir.join("test.db");
        let _ = std::fs::remove_file(&db_path);

        let conn = crate::analytics::open_db_with_migration(&db_path).unwrap();

        // A numeric-only branch — previously the local helper returned
        // None here, so cloud ticket buckets disagreed with local CLI.
        conn.execute(
            "INSERT INTO messages (id, role, timestamp, model, provider, repo_id, git_branch,
                                   input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens, cost_cents)
             VALUES ('msg-num-1', 'assistant', '2026-04-10T14:30:00Z', 'claude-sonnet-4-6', 'anthropic',
                     'sha256:num', 'feature/1234', 10, 20, 0, 0, 0.1)",
            [],
        )
        .unwrap();

        let rollups = fetch_daily_rollups(&conn, None).unwrap();
        let numeric = rollups
            .iter()
            .find(|r| r.git_branch == "feature/1234")
            .expect("numeric rollup present");
        assert_eq!(numeric.ticket.as_deref(), Some("1234"));
        assert_eq!(
            numeric.ticket_source.as_deref(),
            Some(crate::pipeline::TICKET_SOURCE_BRANCH_NUMERIC)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // Regression for #344: `count_pending_*` must return the same row
    // counts as `build_sync_envelope` so `/cloud/status` pollers and the
    // actual sync tick never disagree about what is pending.
    #[test]
    fn count_pending_matches_envelope() {
        let dir = std::env::temp_dir().join("budi-cloud-sync-test-counts");
        std::fs::create_dir_all(&dir).ok();
        let db_path = dir.join("test.db");
        let _ = std::fs::remove_file(&db_path);

        let conn = crate::analytics::open_db_with_migration(&db_path).unwrap();

        // Seed a rollup via the message trigger, plus an explicit session row.
        conn.execute(
            "INSERT INTO messages (id, role, timestamp, model, provider, repo_id, git_branch,
                                   input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens, cost_cents)
             VALUES ('msg-count-1', 'assistant', '2026-04-10T14:30:00Z', 'claude-sonnet-4-6', 'anthropic',
                     'sha256:count', 'feature/PROJ-77-counts', 10, 20, 0, 0, 0.1)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO sessions (id, provider, started_at, ended_at, duration_ms, repo_id, git_branch)
             VALUES ('sess-count-1', 'claude_code', '2026-04-10T14:00:00Z', '2026-04-10T14:30:00Z', 1800000,
                     'sha256:count', 'feature/PROJ-77-counts')",
            [],
        ).unwrap();

        let rollups = fetch_daily_rollups(&conn, None).unwrap();
        let sessions = fetch_session_summaries(&conn, None).unwrap();
        assert_eq!(count_pending_rollups(&conn, None).unwrap(), rollups.len());
        assert_eq!(count_pending_sessions(&conn, None).unwrap(), sessions.len());

        // Same contract holds once watermarks are in place.
        let wm_rollup = "2026-04-10";
        let wm_session = "2026-04-10T14:15:00Z";
        let rollups_wm = fetch_daily_rollups(&conn, Some(wm_rollup)).unwrap();
        let sessions_wm = fetch_session_summaries(&conn, Some(wm_session)).unwrap();
        assert_eq!(
            count_pending_rollups(&conn, Some(wm_rollup)).unwrap(),
            rollups_wm.len()
        );
        assert_eq!(
            count_pending_sessions(&conn, Some(wm_session)).unwrap(),
            sessions_wm.len()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rollup_integration_branches_do_not_emit_ticket() {
        let dir = std::env::temp_dir().join("budi-cloud-sync-test-integration");
        std::fs::create_dir_all(&dir).ok();
        let db_path = dir.join("test.db");
        let _ = std::fs::remove_file(&db_path);

        let conn = crate::analytics::open_db_with_migration(&db_path).unwrap();
        conn.execute(
            "INSERT INTO messages (id, role, timestamp, model, provider, repo_id, git_branch,
                                   input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens, cost_cents)
             VALUES ('msg-int-1', 'assistant', '2026-04-10T14:30:00Z', 'claude-sonnet-4-6', 'anthropic',
                     'sha256:int', 'main', 10, 20, 0, 0, 0.1)",
            [],
        )
        .unwrap();

        let rollups = fetch_daily_rollups(&conn, None).unwrap();
        let main_rollup = rollups
            .iter()
            .find(|r| r.git_branch == "main")
            .expect("rollup for main present");
        assert!(main_rollup.ticket.is_none());
        assert!(main_rollup.ticket_source.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
