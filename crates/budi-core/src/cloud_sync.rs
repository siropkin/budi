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
    /// Human-friendly device label (#552). Populated from
    /// [`CloudConfig::effective_label`] on every ingest, so a local
    /// rename propagates without the user having to re-link. Always
    /// serialized; an empty string is the explicit opt-out contract
    /// documented on `CloudConfig::label`.
    pub label: String,
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
    /// Surface dimension (#701, #723) — `vscode`, `cursor`, `jetbrains`,
    /// `terminal`, or `unknown`. The local `message_rollups_daily` PK
    /// already includes `surface`, so this is a projection-only change:
    /// per-(role, provider, model, repo, branch, surface) rows already
    /// exist correctly. Always serialized; the local column is
    /// `NOT NULL DEFAULT 'unknown'`.
    pub surface: String,
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
    /// Effective cost: what every read surface (CLI, statusline, extensions,
    /// dashboard) displays. Defaults to the LiteLLM-priced `cost_cents_ingested`
    /// at ingest; rewritten by the team-pricing worker (#731) once a cloud
    /// price list is active. ADR-0094 §1.
    #[serde(alias = "cost_cents")]
    pub cost_cents_effective: f64,
    /// LiteLLM-priced cost calculated at ingest time. ADR-0091 §5 immutable
    /// (never overwritten after insert), as amended by ADR-0094 Rule D.
    /// Sent so the cloud can populate its own `cost_cents_ingested` column.
    pub cost_cents_ingested: f64,
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
    /// Surface dimension (#701, #723). Matches the `sessions.surface`
    /// column which is `NOT NULL DEFAULT 'unknown'`, so the field is
    /// always present on the wire — the cloud's `normalizeSurface`
    /// already coalesces missing → `'unknown'`, but the daemon never
    /// relies on that.
    pub surface: String,
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
    /// Model that consumed the largest share of `input + output` tokens for
    /// the session, ties broken by latest-used (#638). Omitted when the
    /// session has zero scored messages — the cloud column is nullable for
    /// exactly that case (budi-cloud#140).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_model: Option<String>,
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

/// #564: drop the three cloud-sync sentinel rows so the next push falls
/// into the no-watermark path of `fetch_daily_rollups` /
/// `fetch_session_summaries` and re-uploads everything from
/// `message_rollups_daily` + `sessions`. Used by `budi cloud reset` after
/// the cloud loses historical rows (org switch, device_id rotation,
/// cloud-side wipe). Cloud-side dedup (ADR-0083 §6) makes the re-upload
/// safe even when records overlap with what the cloud already has.
///
/// Returns the number of sentinel rows that were removed (0..=3) so the
/// caller can decide whether to print "watermarks reset" vs "no watermarks
/// to reset".
pub fn reset_cloud_watermarks(conn: &Connection) -> Result<usize> {
    let removed = conn.execute(
        "DELETE FROM sync_state WHERE file_path IN (?1, ?2, ?3)",
        params![
            CLOUD_SYNC_WATERMARK_KEY,
            format!("{CLOUD_SYNC_WATERMARK_KEY}_value"),
            CLOUD_SYNC_SESSION_WATERMARK_KEY,
        ],
    )?;
    Ok(removed)
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
    /// Whether `~/.config/budi/cloud.toml` exists on disk. Lets `budi cloud
    /// status` and the dashboard distinguish "no config, run `budi cloud
    /// init`" from the other not-ready shapes without re-running the TOML
    /// loader on every render (#446).
    pub config_exists: bool,
    /// Whether the loaded `api_key` equals `CLOUD_API_KEY_STUB` — i.e. the
    /// user ran `budi cloud init` but did not paste a real key yet. Surfaced
    /// so the CLI can render "disabled (stub key)" separately from "disabled
    /// (no config)" (#446).
    pub api_key_stub: bool,
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
    // `effective_api_key()` already returns `api_key.clone().or_else(env lookups)`,
    // so the earlier `api_key.is_some() || effective_api_key().is_some()` was
    // strictly dominated by the second check (see #346).
    let configured = config.effective_api_key().is_some();
    let config_exists = crate::config::cloud_config_exists();
    let api_key_stub = config.is_api_key_stub();

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
        config_exists,
        api_key_stub,
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
            "SELECT bucket_day, role, provider, model, repo_id, git_branch, surface,
                    message_count, input_tokens, output_tokens,
                    cache_creation_tokens, cache_read_tokens,
                    cost_cents_effective, cost_cents_ingested
             FROM message_rollups_daily
             WHERE bucket_day > ?1 OR bucket_day = ?2
             ORDER BY bucket_day",
        )?;
        stmt.query_map(params![wm, today], map_rollup_row)?
            .filter_map(|r| r.ok())
            .collect()
    } else {
        // No watermark: send everything
        let mut stmt = conn.prepare(
            "SELECT bucket_day, role, provider, model, repo_id, git_branch, surface,
                    message_count, input_tokens, output_tokens,
                    cache_creation_tokens, cache_read_tokens,
                    cost_cents_effective, cost_cents_ingested
             FROM message_rollups_daily
             ORDER BY bucket_day",
        )?;
        stmt.query_map([], map_rollup_row)?
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
    // #638: `pm` picks the per-session "primary" model — argmax over
    // `input + output` tokens with latest-message-timestamp as the tie
    // breaker, matching the contract locked in budi-cloud#140. Window
    // function over the same per-(session, model) aggregation, restricted
    // to scored assistant rows so sessions whose only model rows are NULL
    // fall through to `primary_model = NULL`.
    let query = if since.is_some() {
        "SELECT s.id, s.provider, s.started_at, s.ended_at, s.duration_ms,
                s.repo_id, s.git_branch, s.surface,
                COALESCE(m.msg_count, 0),
                COALESCE(m.total_input, 0),
                COALESCE(m.total_output, 0),
                COALESCE(m.total_cost, 0.0),
                pm.model
         FROM sessions s
         LEFT JOIN (
             SELECT session_id,
                    COUNT(*) as msg_count,
                    SUM(input_tokens) as total_input,
                    SUM(output_tokens) as total_output,
                    SUM(COALESCE(cost_cents_effective, 0.0)) as total_cost
             FROM messages
             WHERE role = 'assistant'
             GROUP BY session_id
         ) m ON m.session_id = s.id
         LEFT JOIN (
             SELECT session_id, model
             FROM (
                 SELECT session_id, model,
                        SUM(input_tokens + output_tokens) as model_tokens,
                        MAX(timestamp) as last_ts,
                        ROW_NUMBER() OVER (
                            PARTITION BY session_id
                            ORDER BY SUM(input_tokens + output_tokens) DESC,
                                     MAX(timestamp) DESC
                        ) as rn
                 FROM messages
                 WHERE role = 'assistant' AND model IS NOT NULL
                 GROUP BY session_id, model
             ) ranked
             WHERE rn = 1
         ) pm ON pm.session_id = s.id
         WHERE s.started_at > ?1 OR s.ended_at > ?1
            OR (s.ended_at IS NULL AND s.started_at IS NOT NULL)
         ORDER BY s.started_at"
    } else {
        "SELECT s.id, s.provider, s.started_at, s.ended_at, s.duration_ms,
                s.repo_id, s.git_branch, s.surface,
                COALESCE(m.msg_count, 0),
                COALESCE(m.total_input, 0),
                COALESCE(m.total_output, 0),
                COALESCE(m.total_cost, 0.0),
                pm.model
         FROM sessions s
         LEFT JOIN (
             SELECT session_id,
                    COUNT(*) as msg_count,
                    SUM(input_tokens) as total_input,
                    SUM(output_tokens) as total_output,
                    SUM(COALESCE(cost_cents_effective, 0.0)) as total_cost
             FROM messages
             WHERE role = 'assistant'
             GROUP BY session_id
         ) m ON m.session_id = s.id
         LEFT JOIN (
             SELECT session_id, model
             FROM (
                 SELECT session_id, model,
                        SUM(input_tokens + output_tokens) as model_tokens,
                        MAX(timestamp) as last_ts,
                        ROW_NUMBER() OVER (
                            PARTITION BY session_id
                            ORDER BY SUM(input_tokens + output_tokens) DESC,
                                     MAX(timestamp) DESC
                        ) as rn
                 FROM messages
                 WHERE role = 'assistant' AND model IS NOT NULL
                 GROUP BY session_id, model
             ) ranked
             WHERE rn = 1
         ) pm ON pm.session_id = s.id
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

fn map_rollup_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DailyRollupRecord> {
    Ok(DailyRollupRecord {
        bucket_day: row.get(0)?,
        role: row.get(1)?,
        provider: row.get(2)?,
        model: row.get(3)?,
        repo_id: row.get(4)?,
        git_branch: row.get(5)?,
        surface: row.get(6)?,
        ticket: None,
        ticket_source: None,
        message_count: row.get(7)?,
        input_tokens: row.get(8)?,
        output_tokens: row.get(9)?,
        cache_creation_tokens: row.get(10)?,
        cache_read_tokens: row.get(11)?,
        cost_cents_effective: row.get(12)?,
        cost_cents_ingested: row.get(13)?,
    })
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
        surface: row.get(7)?,
        ticket: None,
        ticket_source: None,
        message_count: row.get(8)?,
        total_input_tokens: row.get(9)?,
        total_output_tokens: row.get(10)?,
        total_cost_cents: row.get(11)?,
        primary_model: row.get(12)?,
    })
}

/// #572: target chunk size that keeps each POST well under the cloud's
/// body-size limit. Pre-#572, a 1931 rollups + 2350 sessions envelope
/// (~8 MB) hit 413 on `budi cloud reset` re-uploads.
pub const MAX_RECORDS_PER_ENVELOPE: usize = 500;

/// #572: split a sync payload into ≤ [`MAX_RECORDS_PER_ENVELOPE`] chunks
/// so `sync_tick_report` can POST them one at a time.
///
/// Rollup chunks respect `bucket_day` boundaries so the local
/// "watermark = latest bucket_day fully synced" contract (ADR-0083 §5)
/// stays honest on partial-chunk failure. A single day larger than the
/// cap goes out as one oversized chunk rather than splitting mid-day;
/// in practice no real user has > 500 unique
/// `(role, provider, model, repo, branch)` tuples in one day. Sessions
/// chunk in fixed-size batches — the server keys on
/// `(device_id, session_id)` so partial overlap UPSERTs cleanly.
///
/// Always returns at least one (possibly empty) payload so callers can
/// iterate uniformly.
pub fn chunk_payload(payload: SyncPayload) -> Vec<SyncPayload> {
    let SyncPayload {
        mut daily_rollups,
        mut session_summaries,
    } = payload;

    // Defensive sort — the SQL queries already ORDER BY these, but the
    // function is `pub` and the day-aligned contract depends on it.
    daily_rollups.sort_by(|a, b| a.bucket_day.cmp(&b.bucket_day));
    session_summaries.sort_by(|a, b| a.started_at.cmp(&b.started_at));

    if daily_rollups.len() + session_summaries.len() <= MAX_RECORDS_PER_ENVELOPE {
        return vec![SyncPayload {
            daily_rollups,
            session_summaries,
        }];
    }

    let mut chunks: Vec<SyncPayload> = Vec::new();

    let mut current: Vec<DailyRollupRecord> = Vec::new();
    let mut current_day: Option<String> = None;
    for record in daily_rollups {
        let same_day = current_day.as_deref() == Some(&record.bucket_day);
        if !current.is_empty() && !same_day && current.len() >= MAX_RECORDS_PER_ENVELOPE {
            chunks.push(SyncPayload {
                daily_rollups: std::mem::take(&mut current),
                session_summaries: Vec::new(),
            });
        }
        current_day = Some(record.bucket_day.clone());
        current.push(record);
    }
    if !current.is_empty() {
        chunks.push(SyncPayload {
            daily_rollups: current,
            session_summaries: Vec::new(),
        });
    }

    for batch in session_summaries.chunks(MAX_RECORDS_PER_ENVELOPE) {
        chunks.push(SyncPayload {
            daily_rollups: Vec::new(),
            session_summaries: batch.to_vec(),
        });
    }

    if chunks.is_empty() {
        chunks.push(SyncPayload {
            daily_rollups: Vec::new(),
            session_summaries: Vec::new(),
        });
    }

    chunks
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
        // #723: bumped from 1 → 2 when the `surface` dimension joined the
        // `DailyRollupRecord` / `SessionSummaryRecord` wire structs. The
        // cloud schema (siropkin/budi-cloud migration 014) already accepts
        // the field and `normalizeSurface` coalesces missing → `'unknown'`,
        // so this is a logging marker for the cloud — not a forced break.
        // Old daemons → new cloud still works (column defaults). New
        // daemon → old cloud also works because column has landed since
        // 014.
        schema_version: 2,
        device_id,
        org_id,
        label: config.effective_label(),
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

/// #541: response shape of `GET /v1/whoami` (cloud PR siropkin/budi-cloud#56).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct WhoamiResponse {
    pub org_id: String,
}

/// #541: outcome of a `whoami` call — distinguishes the fatal cases the
/// CLI wants to surface (bad key) from the benign "cloud doesn't expose
/// this yet" case the CLI wants to fall through on. A fresh 8.3.x CLI
/// pointed at an old self-hosted cloud without `/v1/whoami` keeps the
/// pre-#541 behavior (template with commented `device_id` / `org_id`
/// lines) rather than hard-failing `budi cloud init`.
#[derive(Debug, Clone)]
pub enum WhoamiOutcome {
    /// Cloud authenticated the key and returned `org_id`.
    Ok(WhoamiResponse),
    /// 401 — the key is revoked, malformed, or doesn't belong to any
    /// user. The CLI should NOT write `enabled = true` on this path.
    Unauthorized,
    /// 404 / 405 — endpoint doesn't exist on this cloud (old or
    /// self-hosted). The CLI should fall back to the pre-#541 template
    /// shape. The tuple carries the status code for logging.
    EndpointAbsent(u16),
    /// 5xx, network error, timeout, malformed body, etc. Treated as
    /// "try again later" — the CLI falls back to the pre-#541 template
    /// shape so `budi cloud init` doesn't block on transient cloud
    /// downtime.
    TransientError(String),
}

/// #541: `GET /v1/whoami` — identifies the bearer of the api_key.
/// Used by `budi cloud init --api-key KEY` to auto-seed `org_id`
/// without making the user hand-copy it out of the dashboard.
///
/// Returns a structured [`WhoamiOutcome`] so the caller can distinguish
/// "key is bad, don't enable cloud" from "cloud doesn't expose this
/// endpoint yet, fall back". Blocking — `ureq` under the hood; call
/// from a sync context (`cmd_cloud_init` is already sync).
pub fn whoami(endpoint: &str, api_key: &str) -> WhoamiOutcome {
    if let Err(e) = validate_https_endpoint(endpoint) {
        return WhoamiOutcome::TransientError(e.to_string());
    }

    let url = format!("{endpoint}/v1/whoami");

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .into();

    let result = agent
        .get(&url)
        .header("Authorization", &format!("Bearer {api_key}"))
        .call();

    match result {
        Ok(mut response) => match response.body_mut().read_json::<WhoamiResponse>() {
            Ok(resp) => WhoamiOutcome::Ok(resp),
            Err(e) => {
                WhoamiOutcome::TransientError(format!("failed to parse whoami response: {e}"))
            }
        },
        Err(ureq::Error::StatusCode(401)) => WhoamiOutcome::Unauthorized,
        // 404 = route not wired, 405 = route exists at a different verb.
        // Either way, treat as "endpoint not available on this cloud".
        Err(ureq::Error::StatusCode(status)) if status == 404 || status == 405 => {
            WhoamiOutcome::EndpointAbsent(status)
        }
        Err(ureq::Error::StatusCode(status)) => {
            WhoamiOutcome::TransientError(format!("whoami returned {status}"))
        }
        Err(e) => WhoamiOutcome::TransientError(format!("whoami network error: {e}")),
    }
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
    /// #572: number of chunks the payload was split into. `1` on the
    /// steady-state path; `> 1` when the payload exceeds
    /// [`MAX_RECORDS_PER_ENVELOPE`].
    pub chunks_total: usize,
    /// #572: chunks the cloud confirmed. Less than `chunks_total` when
    /// the loop stops on a transient/auth/schema failure mid-stream.
    pub chunks_succeeded: usize,
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
                chunks_total: 0,
                chunks_succeeded: 0,
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
                chunks_total: 0,
                chunks_succeeded: 0,
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
            chunks_total: 0,
            chunks_succeeded: 0,
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
                chunks_total: 0,
                chunks_succeeded: 0,
            };
        }
    };

    // #572: chunk the envelope so a multi-month re-upload doesn't hit 413.
    // The steady-state tick produces exactly one chunk, matching the
    // pre-chunking wire shape.
    let chunks = chunk_payload(envelope.payload);
    let chunks_total = chunks.len();

    let mut chunks_succeeded = 0usize;
    let mut server_records_upserted: Option<i64> = None;
    let mut server_watermark: Option<String> = None;
    let mut last_result = SyncResult::TransientError("no chunks were sent".to_string());

    let device_id = envelope.device_id;
    let org_id = envelope.org_id;
    let label = envelope.label;
    let schema_version = envelope.schema_version;

    for chunk in chunks {
        let chunk_envelope = SyncEnvelope {
            schema_version,
            device_id: device_id.clone(),
            org_id: org_id.clone(),
            label: label.clone(),
            synced_at: chrono::Utc::now().to_rfc3339(),
            payload: chunk,
        };

        let result = send_sync_envelope(&endpoint, &api_key, &chunk_envelope);

        match &result {
            SyncResult::Success(resp) => {
                chunks_succeeded += 1;
                if let Some(n) = resp.records_upserted {
                    server_records_upserted = Some(server_records_upserted.unwrap_or(0) + n);
                }
                if let Some(wm) = &resp.watermark {
                    server_watermark = Some(wm.clone());
                    // ADR-0083 §5: persist per-chunk so partial failure
                    // leaves the watermark at the latest confirmed day.
                    if let Err(e) = set_cloud_watermark(&conn, wm) {
                        tracing::warn!("Failed to update cloud watermark: {e}");
                    }
                }
                last_result = result;
            }
            SyncResult::EmptyPayload => {
                chunks_succeeded += 1;
                last_result = result;
            }
            _ => {
                last_result = result;
                break;
            }
        }
    }

    // Advance session watermark only on full success. On partial-chunk
    // failure the next tick re-fetches the same window and the cloud
    // UPSERTs on `(device_id, session_id)` (ADR-0083 §6).
    if chunks_succeeded == chunks_total && matches!(last_result, SyncResult::Success(_)) {
        let now = chrono::Utc::now().to_rfc3339();
        if let Err(e) = set_session_watermark(&conn, &now) {
            tracing::warn!("Failed to update session watermark: {e}");
        }
    }

    SyncTickReport {
        result: last_result,
        endpoint,
        envelope_rollups,
        envelope_sessions,
        server_records_upserted,
        server_watermark,
        chunks_total,
        chunks_succeeded,
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
                label: "test-host".into(),
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
    fn reset_cloud_watermarks_drops_sentinel_rows() {
        // #564: dropping the three sentinel rows must move the daemon
        // back to the no-watermark path so the next sync re-sends every
        // local rollup + session summary. After reset, getters return
        // None — the same shape a fresh install reports.
        let dir = std::env::temp_dir().join("budi-cloud-sync-test-reset");
        std::fs::create_dir_all(&dir).ok();
        let db_path = dir.join("test.db");
        let _ = std::fs::remove_file(&db_path);

        let conn = crate::analytics::open_db_with_migration(&db_path).unwrap();
        set_cloud_watermark(&conn, "2026-04-10").unwrap();
        set_session_watermark(&conn, "2026-04-10T10:00:00Z").unwrap();

        let removed = reset_cloud_watermarks(&conn).unwrap();
        assert_eq!(
            removed, 3,
            "all three sentinels (rollup-completed, rollup-value, session) must be removed",
        );
        assert!(get_cloud_watermark_value(&conn).unwrap().is_none());
        assert!(get_session_watermark(&conn).unwrap().is_none());

        // Idempotent: a second reset is a no-op (returns 0 rows
        // removed). Lets the CLI render the right "nothing to reset"
        // line without an extra existence check.
        let removed_again = reset_cloud_watermarks(&conn).unwrap();
        assert_eq!(removed_again, 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reset_cloud_watermarks_leaves_unrelated_rows_alone() {
        // The DELETE must be scoped to the cloud sentinels — never
        // touch ingestion offsets / tail offsets / completion markers
        // that share `sync_state`. A regression here would silently
        // re-import every JSONL transcript on the next tick.
        let dir = std::env::temp_dir().join("budi-cloud-sync-test-reset-scope");
        std::fs::create_dir_all(&dir).ok();
        let db_path = dir.join("test.db");
        let _ = std::fs::remove_file(&db_path);

        let conn = crate::analytics::open_db_with_migration(&db_path).unwrap();
        crate::analytics::set_sync_offset(&conn, "/tmp/transcript.jsonl", 4096).unwrap();
        crate::analytics::mark_sync_completed(&conn).unwrap();
        set_cloud_watermark(&conn, "2026-04-10").unwrap();

        reset_cloud_watermarks(&conn).unwrap();

        // Ingestion offset survives.
        assert_eq!(
            crate::analytics::get_sync_offset(&conn, "/tmp/transcript.jsonl").unwrap(),
            4096,
        );
        // Sync-completion marker survives.
        assert!(
            crate::analytics::last_sync_completed_at(&conn)
                .unwrap()
                .is_some(),
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
                                   input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
                                   cost_cents_ingested, cost_cents_effective)
             VALUES ('msg-1', 'assistant', '2026-04-10T14:30:00Z', 'claude-sonnet-4-6', 'anthropic',
                     'sha256:abc123', 'feature/PROJ-42-auth', 100, 200, 10, 50, 1.5, 1.5)",
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

    // #638: helper for the primary_model tests below — seeds a session
    // and a configurable batch of assistant messages.
    fn seed_session_with_messages(
        conn: &Connection,
        session_id: &str,
        rows: &[(&str, Option<&str>, &str, i64, i64)],
    ) {
        conn.execute(
            "INSERT INTO sessions (id, provider, started_at, ended_at, duration_ms, repo_id, git_branch)
             VALUES (?1, 'claude_code', '2026-04-10T09:00:00Z', '2026-04-10T10:00:00Z', 3600000,
                     'sha256:pm', 'main')",
            params![session_id],
        )
        .unwrap();
        for (msg_id, model, ts, input, output) in rows {
            conn.execute(
                "INSERT INTO messages (id, session_id, role, timestamp, model, provider, repo_id, git_branch,
                                       input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
                                       cost_cents_ingested, cost_cents_effective)
                 VALUES (?1, ?2, 'assistant', ?3, ?4, 'anthropic', 'sha256:pm', 'main', ?5, ?6, 0, 0, 0.1, 0.1)",
                params![msg_id, session_id, ts, model, input, output],
            )
            .unwrap();
        }
    }

    /// #638: argmax over `input + output` tokens picks the high-token
    /// model even when it has fewer messages.
    #[test]
    fn primary_model_picks_argmax_by_tokens() {
        let dir = std::env::temp_dir().join("budi-cloud-sync-pm-argmax");
        std::fs::create_dir_all(&dir).ok();
        let db_path = dir.join("test.db");
        let _ = std::fs::remove_file(&db_path);

        let conn = crate::analytics::open_db_with_migration(&db_path).unwrap();
        // One Opus message (10k tokens) outweighs ten Haiku messages (100 tokens each).
        let mut rows: Vec<(&str, Option<&str>, &str, i64, i64)> = vec![(
            "opus-1",
            Some("claude-opus-4-7"),
            "2026-04-10T09:30:00Z",
            5_000,
            5_000,
        )];
        let haiku_ids: Vec<String> = (0..10).map(|i| format!("haiku-{i}")).collect();
        for id in &haiku_ids {
            rows.push((
                id.as_str(),
                Some("claude-haiku-4-5"),
                "2026-04-10T09:45:00Z",
                50,
                50,
            ));
        }
        seed_session_with_messages(&conn, "sess-pm-argmax", &rows);

        let summaries = fetch_session_summaries(&conn, None).unwrap();
        let s = summaries
            .iter()
            .find(|s| s.session_id == "sess-pm-argmax")
            .expect("session present");
        assert_eq!(s.primary_model.as_deref(), Some("claude-opus-4-7"));
    }

    /// #638: when two models tie on token count, the model with the
    /// latest message timestamp wins.
    #[test]
    fn primary_model_tie_broken_by_latest_used() {
        let dir = std::env::temp_dir().join("budi-cloud-sync-pm-tie");
        std::fs::create_dir_all(&dir).ok();
        let db_path = dir.join("test.db");
        let _ = std::fs::remove_file(&db_path);

        let conn = crate::analytics::open_db_with_migration(&db_path).unwrap();
        // Opus and Sonnet each consume exactly 1000 tokens; Sonnet's
        // latest message lands later, so Sonnet must win.
        seed_session_with_messages(
            &conn,
            "sess-pm-tie",
            &[
                (
                    "opus-1",
                    Some("claude-opus-4-7"),
                    "2026-04-10T09:10:00Z",
                    500,
                    500,
                ),
                (
                    "sonnet-1",
                    Some("claude-sonnet-4-6"),
                    "2026-04-10T09:50:00Z",
                    500,
                    500,
                ),
            ],
        );

        let summaries = fetch_session_summaries(&conn, None).unwrap();
        let s = summaries
            .iter()
            .find(|s| s.session_id == "sess-pm-tie")
            .expect("session present");
        assert_eq!(s.primary_model.as_deref(), Some("claude-sonnet-4-6"));
    }

    /// #638: a session with zero scored messages must omit `primary_model`
    /// entirely — the cloud column is nullable for exactly this case, and
    /// the daemon must not guess.
    #[test]
    fn primary_model_omitted_for_session_without_scored_messages() {
        let dir = std::env::temp_dir().join("budi-cloud-sync-pm-empty");
        std::fs::create_dir_all(&dir).ok();
        let db_path = dir.join("test.db");
        let _ = std::fs::remove_file(&db_path);

        let conn = crate::analytics::open_db_with_migration(&db_path).unwrap();
        conn.execute(
            "INSERT INTO sessions (id, provider, started_at, ended_at, duration_ms, repo_id, git_branch)
             VALUES ('sess-pm-empty', 'claude_code', '2026-04-10T09:00:00Z', '2026-04-10T10:00:00Z', 3600000,
                     'sha256:pm', 'main')",
            [],
        )
        .unwrap();

        let summaries = fetch_session_summaries(&conn, None).unwrap();
        let s = summaries
            .iter()
            .find(|s| s.session_id == "sess-pm-empty")
            .expect("session present");
        assert!(s.primary_model.is_none());

        // Serialization must drop the field entirely so the cloud row stays NULL.
        let json = serde_json::to_value(s).unwrap();
        assert!(json.get("primary_model").is_none());

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
        assert_eq!(envelope.schema_version, 2);
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
    fn current_cloud_status_reports_api_key_stub_when_placeholder() {
        let dir = std::env::temp_dir().join("budi-cloud-status-stub");
        std::fs::create_dir_all(&dir).ok();
        let db_path = dir.join("test.db");
        let _ = std::fs::remove_file(&db_path);
        let _ = crate::analytics::open_db_with_migration(&db_path).unwrap();

        let config = CloudConfig {
            api_key: Some(crate::config::CLOUD_API_KEY_STUB.to_string()),
            ..CloudConfig::default()
        };
        let status = current_cloud_status(&db_path, &config);
        assert!(
            status.api_key_stub,
            "placeholder api_key must surface as api_key_stub=true"
        );
        assert!(
            !status.ready,
            "stub key must never look ready even if enabled is true elsewhere"
        );

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
                                   input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
                                   cost_cents_ingested, cost_cents_effective)
             VALUES ('msg-status-1', 'assistant', '2026-04-10T14:30:00Z', 'claude-sonnet-4-6', 'anthropic',
                     'sha256:abc', 'main', 100, 200, 10, 50, 1.5, 1.5)",
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
            schema_version: 2,
            device_id: "dev_test".into(),
            org_id: "org_test".into(),
            label: "ivan-mbp".into(),
            synced_at: "2026-04-12T00:00:00Z".into(),
            payload: SyncPayload {
                daily_rollups: vec![DailyRollupRecord {
                    bucket_day: "2026-04-10".into(),
                    role: "assistant".into(),
                    provider: "claude_code".into(),
                    model: "claude-sonnet-4-6".into(),
                    repo_id: "sha256:abc".into(),
                    git_branch: "main".into(),
                    surface: "cursor".into(),
                    ticket: None,
                    ticket_source: None,
                    message_count: 5,
                    input_tokens: 1000,
                    output_tokens: 500,
                    cache_creation_tokens: 100,
                    cache_read_tokens: 200,
                    cost_cents_effective: 2.5,
                    cost_cents_ingested: 2.5,
                }],
                session_summaries: vec![],
            },
        };

        let json = serde_json::to_value(&envelope).unwrap();
        // #723: bumped to 2 alongside the `surface` field landing on both
        // wire structs.
        assert_eq!(json["schema_version"], 2);
        assert_eq!(json["device_id"], "dev_test");
        // #552: label travels alongside device_id / org_id / synced_at
        // on the envelope root.
        assert_eq!(json["label"], "ivan-mbp");
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
        // ADR-0094 §1: envelope carries both `cost_cents_effective` (read
        // surface, may be overridden by team pricing) and `cost_cents_ingested`
        // (LiteLLM-priced ingest cost, immutable per ADR-0091 §5 Rule D).
        // Cloud uses `_ingested` to populate its own ingested column on insert.
        assert_eq!(
            json["payload"]["daily_rollups"][0]["cost_cents_effective"],
            2.5
        );
        assert_eq!(
            json["payload"]["daily_rollups"][0]["cost_cents_ingested"],
            2.5
        );
        // #723: surface always emitted (NOT NULL on the local column).
        assert_eq!(json["payload"]["daily_rollups"][0]["surface"], "cursor");
    }

    /// #552: when `cloud.toml` omits `label`, `effective_label()` falls
    /// back to the local OS hostname. We don't pin the exact value —
    /// the test host's hostname is whatever the CI image decided — but
    /// it must match `get_hostname()` (same source of truth) so a
    /// hostname change propagates consistently across callers.
    #[test]
    fn effective_label_defaults_to_hostname_when_unset() {
        let config = CloudConfig::default();
        assert!(config.label.is_none());
        assert_eq!(
            config.effective_label(),
            crate::pipeline::enrichers::get_hostname(),
        );
    }

    /// #552: explicit TOML value is sent verbatim, including an empty
    /// string (documented as the opt-out contract on `CloudConfig::label`).
    #[test]
    fn effective_label_sends_explicit_value_verbatim() {
        let explicit = CloudConfig {
            label: Some("ivan-mbp".into()),
            ..CloudConfig::default()
        };
        assert_eq!(explicit.effective_label(), "ivan-mbp");

        let opt_out = CloudConfig {
            label: Some(String::new()),
            ..CloudConfig::default()
        };
        assert_eq!(
            opt_out.effective_label(),
            "",
            "opt-out must send empty label rather than silently \
             falling back to hostname — otherwise the user can't \
             actually hide their hostname",
        );
    }

    /// Round-trip through `build_sync_envelope` to confirm the label
    /// lands on the envelope with the same precedence.
    #[test]
    fn build_envelope_populates_label_from_config() {
        let dir = std::env::temp_dir().join("budi-cloud-sync-label-envelope");
        std::fs::create_dir_all(&dir).ok();
        let db_path = dir.join("test.db");
        let _ = std::fs::remove_file(&db_path);

        let conn = crate::analytics::open_db_with_migration(&db_path).unwrap();
        let config = CloudConfig {
            enabled: true,
            api_key: Some("budi_test".into()),
            device_id: Some("dev_test".into()),
            org_id: Some("org_test".into()),
            label: Some("ivan-mbp".into()),
            ..CloudConfig::default()
        };
        let envelope = build_sync_envelope(&conn, &config).unwrap();
        assert_eq!(envelope.label, "ivan-mbp");

        let _ = std::fs::remove_dir_all(&dir);
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
                                   input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
                                   cost_cents_ingested, cost_cents_effective)
             VALUES ('msg-num-1', 'assistant', '2026-04-10T14:30:00Z', 'claude-sonnet-4-6', 'anthropic',
                     'sha256:num', 'feature/1234', 10, 20, 0, 0, 0.1, 0.1)",
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
                                   input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
                                   cost_cents_ingested, cost_cents_effective)
             VALUES ('msg-count-1', 'assistant', '2026-04-10T14:30:00Z', 'claude-sonnet-4-6', 'anthropic',
                     'sha256:count', 'feature/PROJ-77-counts', 10, 20, 0, 0, 0.1, 0.1)",
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

    // -------- #723: surface dimension on cloud-sync wire structs --------

    /// #723: rows ingested for every canonical surface value must
    /// round-trip through the daily-rollup wire struct. Mirrors the
    /// parser-output set landed in #701 (`vscode` / `cursor` /
    /// `jetbrains` / `terminal` / `unknown`), so a regression that drops
    /// the column from the SELECT list trips here rather than silently
    /// re-landing 100% `'unknown'` on the cloud.
    #[test]
    fn rollup_round_trips_surface_for_every_canonical_value() {
        let dir = std::env::temp_dir().join("budi-cloud-sync-test-rollup-surface");
        std::fs::create_dir_all(&dir).ok();
        let db_path = dir.join("test.db");
        let _ = std::fs::remove_file(&db_path);

        let conn = crate::analytics::open_db_with_migration(&db_path).unwrap();

        let surfaces = ["vscode", "cursor", "jetbrains", "terminal", "unknown"];
        for (i, surface) in surfaces.iter().enumerate() {
            // One message per surface — the rollup trigger keys on
            // (bucket_day, role, provider, model, repo_id, git_branch,
            // surface), so distinct surfaces fan out to distinct rollup
            // rows even with identical provider/model/repo/branch.
            conn.execute(
                "INSERT INTO messages (id, role, timestamp, model, provider, repo_id, git_branch,
                                       surface, input_tokens, output_tokens,
                                       cache_creation_tokens, cache_read_tokens,
                                       cost_cents_ingested, cost_cents_effective)
                 VALUES (?1, 'assistant', '2026-04-10T14:30:00Z', 'claude-sonnet-4-6', 'anthropic',
                         'sha256:surface', 'main', ?2, 10, 20, 0, 0, 0.1, 0.1)",
                params![format!("msg-surface-{i}"), surface],
            )
            .unwrap();
        }

        let rollups = fetch_daily_rollups(&conn, None).unwrap();
        for surface in surfaces {
            let r = rollups
                .iter()
                .find(|r| r.surface == surface)
                .unwrap_or_else(|| panic!("rollup for surface={surface:?} present"));
            // JSON round-trip — the cloud parses the same shape.
            let json = serde_json::to_value(r).unwrap();
            assert_eq!(json["surface"], surface);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #723: same coverage on the session wire struct. The `sessions`
    /// table stores `surface` directly (no trigger), so the SELECT in
    /// `fetch_session_summaries` is the only thing that has to project it.
    #[test]
    fn session_round_trips_surface_for_every_canonical_value() {
        let dir = std::env::temp_dir().join("budi-cloud-sync-test-session-surface");
        std::fs::create_dir_all(&dir).ok();
        let db_path = dir.join("test.db");
        let _ = std::fs::remove_file(&db_path);

        let conn = crate::analytics::open_db_with_migration(&db_path).unwrap();

        let surfaces = ["vscode", "cursor", "jetbrains", "terminal", "unknown"];
        for (i, surface) in surfaces.iter().enumerate() {
            conn.execute(
                "INSERT INTO sessions (id, provider, started_at, ended_at, duration_ms,
                                       repo_id, git_branch, surface)
                 VALUES (?1, 'claude_code', '2026-04-10T09:00:00Z', '2026-04-10T10:00:00Z',
                         3600000, 'sha256:surface', 'main', ?2)",
                params![format!("sess-surface-{i}"), surface],
            )
            .unwrap();
        }

        let summaries = fetch_session_summaries(&conn, None).unwrap();
        for surface in surfaces {
            let s = summaries
                .iter()
                .find(|s| s.surface == surface)
                .unwrap_or_else(|| panic!("session for surface={surface:?} present"));
            let json = serde_json::to_value(s).unwrap();
            assert_eq!(json["surface"], surface);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #723: snapshot the on-wire JSON shape with `surface` populated so
    /// the wire payload is reviewable in PRs. The cloud ingest contract
    /// is "field is optional but, when present, must be the literal
    /// string surface value" — a regression where the daemon emits
    /// e.g. `{"surface": null}` or skips the field would land all rows
    /// back at `'unknown'` on the cloud (siropkin/budi-cloud#227).
    #[test]
    fn rollup_wire_snapshot_with_surface() {
        let record = DailyRollupRecord {
            bucket_day: "2026-04-10".into(),
            role: "assistant".into(),
            provider: "claude_code".into(),
            model: "claude-sonnet-4-6".into(),
            repo_id: "sha256:abc".into(),
            git_branch: "main".into(),
            surface: "jetbrains".into(),
            ticket: None,
            ticket_source: None,
            message_count: 5,
            input_tokens: 1000,
            output_tokens: 500,
            cache_creation_tokens: 100,
            cache_read_tokens: 200,
            cost_cents_effective: 2.5,
            cost_cents_ingested: 2.5,
        };
        let json = serde_json::to_string(&record).unwrap();
        let expected = "{\
            \"bucket_day\":\"2026-04-10\",\
            \"role\":\"assistant\",\
            \"provider\":\"claude_code\",\
            \"model\":\"claude-sonnet-4-6\",\
            \"repo_id\":\"sha256:abc\",\
            \"git_branch\":\"main\",\
            \"surface\":\"jetbrains\",\
            \"message_count\":5,\
            \"input_tokens\":1000,\
            \"output_tokens\":500,\
            \"cache_creation_tokens\":100,\
            \"cache_read_tokens\":200,\
            \"cost_cents_effective\":2.5,\
            \"cost_cents_ingested\":2.5\
        }";
        assert_eq!(json, expected);
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
                                   input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
                                   cost_cents_ingested, cost_cents_effective)
             VALUES ('msg-int-1', 'assistant', '2026-04-10T14:30:00Z', 'claude-sonnet-4-6', 'anthropic',
                     'sha256:int', 'main', 10, 20, 0, 0, 0.1, 0.1)",
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

    // -------- #572: chunked envelope tests --------

    fn make_rollup(day: &str, model: &str) -> DailyRollupRecord {
        DailyRollupRecord {
            bucket_day: day.into(),
            role: "assistant".into(),
            provider: "anthropic".into(),
            model: model.into(),
            repo_id: "sha256:test".into(),
            git_branch: "main".into(),
            surface: "unknown".into(),
            ticket: None,
            ticket_source: None,
            message_count: 1,
            input_tokens: 10,
            output_tokens: 20,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            cost_cents_effective: 0.1,
            cost_cents_ingested: 0.1,
        }
    }

    fn make_session(id: &str, started_at: &str) -> SessionSummaryRecord {
        SessionSummaryRecord {
            session_id: id.into(),
            provider: "anthropic".into(),
            started_at: Some(started_at.into()),
            ended_at: None,
            duration_ms: None,
            repo_id: None,
            git_branch: None,
            surface: "unknown".into(),
            ticket: None,
            ticket_source: None,
            message_count: 1,
            total_input_tokens: 10,
            total_output_tokens: 20,
            total_cost_cents: 0.1,
            primary_model: None,
        }
    }

    #[test]
    fn chunk_payload_below_threshold_returns_single_chunk() {
        // Steady-state ticks must keep the pre-#572 single-POST shape.
        let payload = SyncPayload {
            daily_rollups: vec![make_rollup("2026-04-10", "claude-sonnet-4-6")],
            session_summaries: vec![make_session("s1", "2026-04-10T10:00:00Z")],
        };
        let chunks = chunk_payload(payload);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].daily_rollups.len(), 1);
        assert_eq!(chunks[0].session_summaries.len(), 1);
    }

    #[test]
    fn chunk_payload_empty_returns_one_empty_chunk() {
        // Callers iterate the returned vec; one empty chunk keeps the
        // call site uniform with the non-empty path.
        let chunks = chunk_payload(SyncPayload {
            daily_rollups: vec![],
            session_summaries: vec![],
        });
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].daily_rollups.is_empty());
        assert!(chunks[0].session_summaries.is_empty());
    }

    #[test]
    fn chunk_payload_splits_large_rollup_set_at_day_boundaries() {
        // 12 days × 50 rollups = 600 records → at least 2 chunks. The
        // contract under test: no bucket_day spans two chunks.
        let mut rollups: Vec<DailyRollupRecord> = Vec::new();
        for d in 1..=12 {
            for i in 0..50 {
                let model = format!("model-{i:02}");
                rollups.push(make_rollup(&format!("2026-04-{d:02}"), &model));
            }
        }
        let total = rollups.len();
        let chunks = chunk_payload(SyncPayload {
            daily_rollups: rollups,
            session_summaries: vec![],
        });

        let chunked_total: usize = chunks.iter().map(|c| c.daily_rollups.len()).sum();
        assert_eq!(chunked_total, total);

        let seen_days_per_chunk: Vec<Vec<String>> = chunks
            .iter()
            .map(|c| {
                let mut days: Vec<String> = c
                    .daily_rollups
                    .iter()
                    .map(|r| r.bucket_day.clone())
                    .collect();
                days.sort();
                days.dedup();
                days
            })
            .collect();
        let total_unique = {
            let mut all: Vec<String> = seen_days_per_chunk.iter().flatten().cloned().collect();
            all.sort();
            all.dedup();
            all.len()
        };
        let pair_count: usize = seen_days_per_chunk.iter().map(|d| d.len()).sum();
        assert_eq!(
            pair_count, total_unique,
            "a single bucket_day must not span multiple chunks"
        );

        assert!(chunks.len() >= 2);
    }

    #[test]
    fn chunk_payload_keeps_oversized_single_day_intact() {
        // A pathological single day > MAX records goes out as one
        // oversized chunk to preserve "watermark = day fully synced".
        let mut rollups = Vec::new();
        for i in 0..(MAX_RECORDS_PER_ENVELOPE + 50) {
            rollups.push(make_rollup("2026-04-01", &format!("model-{i}")));
        }
        let chunks = chunk_payload(SyncPayload {
            daily_rollups: rollups,
            session_summaries: vec![],
        });
        // All records for the single day land in a single chunk.
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].daily_rollups.len(), MAX_RECORDS_PER_ENVELOPE + 50);
    }

    #[test]
    fn chunk_payload_chunks_sessions_separately_from_rollups() {
        // Sessions chunk in fixed-size batches, isolated from rollups.
        let mut sessions = Vec::new();
        for i in 0..(MAX_RECORDS_PER_ENVELOPE * 2 + 100) {
            sessions.push(make_session(&format!("s-{i}"), "2026-04-10T10:00:00Z"));
        }
        let chunks = chunk_payload(SyncPayload {
            daily_rollups: vec![],
            session_summaries: sessions,
        });
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].session_summaries.len(), MAX_RECORDS_PER_ENVELOPE);
        assert_eq!(chunks[1].session_summaries.len(), MAX_RECORDS_PER_ENVELOPE);
        assert_eq!(chunks[2].session_summaries.len(), 100);
        for chunk in &chunks {
            assert!(chunk.daily_rollups.is_empty());
        }
    }

    #[test]
    fn chunk_payload_simulates_dogfood_db_shape() {
        // Recreates the issue's failing case (~1920 rollups + 2350
        // sessions, pre-#572 a single 8+ MB POST → 413).
        let mut rollups = Vec::new();
        for d in 0..240 {
            let day = format!("2025-08-{:02}", (d % 28) + 1);
            for i in 0..8 {
                rollups.push(make_rollup(&day, &format!("model-{i}-{d}")));
            }
        }

        let mut sessions = Vec::new();
        for i in 0..2350 {
            sessions.push(make_session(&format!("s-{i}"), "2026-04-10T10:00:00Z"));
        }

        let chunks = chunk_payload(SyncPayload {
            daily_rollups: rollups,
            session_summaries: sessions,
        });

        assert!(
            chunks.len() >= 5,
            "dogfood-sized payload should split into many chunks; got {}",
            chunks.len(),
        );
        // ⌈2350 / 500⌉ = 5 session chunks.
        let session_chunks: usize = chunks
            .iter()
            .filter(|c| !c.session_summaries.is_empty())
            .count();
        assert_eq!(session_chunks, 5);
    }
}
