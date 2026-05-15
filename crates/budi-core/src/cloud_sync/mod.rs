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

/// Top-level wire envelope POSTed to `/v1/ingest`.
///
/// Custom [`Serialize`] impl: the workspace identifier is emitted under
/// **both** `workspace_id` (the post-rename key) and the legacy `org_id`
/// alias during the deprecation window described in ADR-0083 §2 (#836).
/// The legacy alias is dropped after siropkin/budi-cloud#321 lands and
/// one release cycle of mixed-version operation has passed.
#[derive(Debug, Clone)]
pub struct SyncEnvelope {
    pub schema_version: u32,
    pub device_id: String,
    /// Workspace identifier on the cloud dashboard.
    pub workspace_id: String,
    /// Human-friendly device label (#552). Populated from
    /// [`CloudConfig::effective_label`] on every ingest, so a local
    /// rename propagates without the user having to re-link. Always
    /// serialized; an empty string is the explicit opt-out contract
    /// documented on `CloudConfig::label`.
    pub label: String,
    pub synced_at: String,
    pub payload: SyncPayload,
}

impl Serialize for SyncEnvelope {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        // #836: dual-emit `workspace_id` (new) and `org_id` (legacy) from
        // the single in-memory `workspace_id` source so cloud ingest still
        // accepting the old key keeps working against new daemons. Field
        // count stays in lockstep with the entries actually serialized
        // below.
        let mut s = serializer.serialize_struct("SyncEnvelope", 7)?;
        s.serialize_field("schema_version", &self.schema_version)?;
        s.serialize_field("device_id", &self.device_id)?;
        s.serialize_field("workspace_id", &self.workspace_id)?;
        s.serialize_field("org_id", &self.workspace_id)?;
        s.serialize_field("label", &self.label)?;
        s.serialize_field("synced_at", &self.synced_at)?;
        s.serialize_field("payload", &self.payload)?;
        s.end()
    }
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

/// #767: wire-shape version embedded in this binary for the
/// `message_rollups_daily` upload projection. Bump this constant any time a
/// new column joins [`DailyRollupRecord`] (or an existing one changes
/// semantics on the wire). On boot, [`reset_stale_shape_watermarks`]
/// compares this against the local rows' `wire_shape_version`; any drift
/// drops [`CLOUD_SYNC_WATERMARK_KEY`] so the next sync re-emits history
/// under the current shape.
///
/// History: `1` = pre-surface (≤ 8.4.2). `2` = surface dimension joined the
/// wire (#723, shipped in 8.4.3).
pub const WIRE_SHAPE_VERSION_ROLLUPS: u32 = 2;

/// #767: wire-shape version embedded in this binary for the `sessions`
/// upload projection. Bump any time a new column joins
/// [`SessionSummaryRecord`].
///
/// History: `1` = pre-surface (≤ 8.4.2). `2` = surface dimension joined the
/// wire (#723, shipped in 8.4.3).
pub const WIRE_SHAPE_VERSION_SESSIONS: u32 = 2;

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

/// #767: outcome of a [`reset_stale_shape_watermarks`] call. Logged at INFO
/// at daemon boot so on-call can correlate "the cloud dashboard suddenly
/// reflowed all my history" with the wire-shape upgrade event.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StaleShapeReset {
    /// True when the rollup watermark (`__budi_cloud_sync__` + its `_value`
    /// row) was dropped because at least one row in `message_rollups_daily`
    /// carried a `wire_shape_version` other than
    /// [`WIRE_SHAPE_VERSION_ROLLUPS`].
    pub rollups_reset: bool,
    /// Same for the session watermark
    /// (`__budi_cloud_sync_sessions__`) and `sessions`.
    pub sessions_reset: bool,
    /// Number of `message_rollups_daily` rows whose `wire_shape_version`
    /// was bumped to the current binary expectation.
    pub rollup_rows_updated: usize,
    /// Number of `sessions` rows whose `wire_shape_version` was bumped.
    pub session_rows_updated: usize,
    /// Maximum `wire_shape_version` observed in `message_rollups_daily`
    /// before the reset. `None` when the table is empty.
    pub rollup_local_max: Option<u32>,
    /// Maximum `wire_shape_version` observed in `sessions` before the
    /// reset. `None` when the table is empty.
    pub sessions_local_max: Option<u32>,
}

impl StaleShapeReset {
    /// True iff either watermark was dropped. Used by the daemon's boot
    /// log to decide whether to emit the INFO line announcing the upgrade
    /// dance.
    pub fn any_reset(&self) -> bool {
        self.rollups_reset || self.sessions_reset
    }
}

/// #767: on daemon boot, detect rows whose `wire_shape_version` is below
/// what the current binary will emit, and force a re-upload of all
/// affected rows by dropping the matching cloud-sync watermark
/// ([`CLOUD_SYNC_WATERMARK_KEY`] for rollups, [`CLOUD_SYNC_SESSION_WATERMARK_KEY`]
/// for sessions). Each affected row's `wire_shape_version` is bulk-updated
/// to the binary's expected version so the check doesn't fire again on
/// the next boot.
///
/// Safe on databases that predate the column (returns an all-false
/// [`StaleShapeReset`]; the migration's `reconcile_schema` runs before this
/// function in the daemon's startup order, so a missing column means the
/// DB pre-dates 8.4.6 *and* `repair` was never invoked — neither path
/// the daemon takes on a healthy boot).
pub fn reset_stale_shape_watermarks(conn: &Connection) -> Result<StaleShapeReset> {
    let mut out = StaleShapeReset::default();

    if crate::migration::table_exists(conn, "sessions")?
        && crate::migration::has_column(conn, "sessions", "wire_shape_version")?
    {
        let max_local: Option<u32> = conn
            .query_row("SELECT MAX(wire_shape_version) FROM sessions", [], |row| {
                row.get::<_, Option<i64>>(0)
            })?
            .map(|v| v as u32);
        out.sessions_local_max = max_local;
        if let Some(max) = max_local
            && max != WIRE_SHAPE_VERSION_SESSIONS
        {
            conn.execute(
                "DELETE FROM sync_state WHERE file_path = ?1",
                params![CLOUD_SYNC_SESSION_WATERMARK_KEY],
            )?;
            let updated = conn.execute(
                "UPDATE sessions SET wire_shape_version = ?1 WHERE wire_shape_version != ?1",
                params![WIRE_SHAPE_VERSION_SESSIONS],
            )?;
            out.sessions_reset = true;
            out.session_rows_updated = updated;
        }
    }

    if crate::migration::table_exists(conn, "message_rollups_daily")?
        && crate::migration::has_column(conn, "message_rollups_daily", "wire_shape_version")?
    {
        let max_local: Option<u32> = conn
            .query_row(
                "SELECT MAX(wire_shape_version) FROM message_rollups_daily",
                [],
                |row| row.get::<_, Option<i64>>(0),
            )?
            .map(|v| v as u32);
        out.rollup_local_max = max_local;
        if let Some(max) = max_local
            && max != WIRE_SHAPE_VERSION_ROLLUPS
        {
            conn.execute(
                "DELETE FROM sync_state WHERE file_path IN (?1, ?2)",
                params![
                    CLOUD_SYNC_WATERMARK_KEY,
                    format!("{CLOUD_SYNC_WATERMARK_KEY}_value"),
                ],
            )?;
            let updated = conn.execute(
                "UPDATE message_rollups_daily SET wire_shape_version = ?1 \
                 WHERE wire_shape_version != ?1",
                params![WIRE_SHAPE_VERSION_ROLLUPS],
            )?;
            out.rollups_reset = true;
            out.rollup_rows_updated = updated;
        }
    }

    Ok(out)
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
/// device_id/workspace_id missing), pending counts fall back to 0 and the caller
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
    let workspace_id = config
        .workspace_id
        .as_ref()
        .context("workspace_id not configured")?
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
        workspace_id,
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
    /// 422 — server rejected the envelope. #756: the body is captured
    /// verbatim and classified so the daemon/CLI can show the right
    /// recovery advice. The previous shape dropped the body and assumed
    /// every 422 was a schema-version mismatch; the v8.4.4 smoke test
    /// proved that wrong (the cloud's `validateIngestMetrics` rejected
    /// the `cost_cents` rename with a non-version 422 and budi told the
    /// user to "update budi" when the cloud was the lagging side).
    SchemaMismatch(SchemaMismatch),
    /// Transient error (429/5xx/network) — should retry with backoff.
    TransientError(String),
    /// Nothing to sync (empty payload).
    EmptyPayload,
}

/// #756: structured 422 outcome. Wraps the server's exact response body
/// plus a classification of *why* the cloud rejected it. The daemon
/// renders the body verbatim and chooses the recovery advice from
/// [`SchemaMismatchKind`] (so a "cloud is lagging" 422 stops telling the
/// user to update budi when budi isn't the problem).
#[derive(Debug, Clone)]
pub struct SchemaMismatch {
    pub body: String,
    pub kind: SchemaMismatchKind,
}

/// #756: classification of a 422 from the cloud ingest endpoint.
///
/// The cloud's `validateIngestMetrics` produces two distinct shapes of
/// 422 — one that names `schema_version` directly, and a catch-all for
/// per-field validation (`cost_cents must be a finite, non-negative
/// number`, `device_id must be a UUID`, etc.). Only the first kind
/// implies "the client is older than the cloud and needs to update", so
/// the daemon needs to know which it is before printing recovery advice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaMismatchKind {
    /// Body matched `Unsupported schema_version: <c>. Expected one of:
    /// [<min>, …, <max>]` and the client version is below the minimum
    /// accepted version. The user should update budi.
    ClientTooOld { client: u32, expected_min: u32 },
    /// Body matched the same pattern, but the client version is above
    /// the maximum the cloud accepts. The cloud is the lagging side;
    /// no amount of `brew upgrade budi` will help. Telling the user to
    /// update budi here was the exact failure flagged in #749's body.
    CloudTooOld { client: u32, expected_max: u32 },
    /// Body did not match the schema_version pattern at all — usually a
    /// per-field validation error from the cloud's envelope validator.
    /// Surface the body verbatim and do not pretend it is a version
    /// issue.
    NotSchemaRelated,
}

/// #756: parse the cloud's 422 response body for a schema_version
/// mismatch and classify it against the client's current schema_version.
/// Returns [`SchemaMismatchKind::NotSchemaRelated`] when the body
/// doesn't match the expected pattern.
///
/// The cloud emits the pattern `Unsupported schema_version: <c>.
/// Expected one of: [<v1>, <v2>, …]` (see siropkin/budi-cloud
/// `validateEnvelope`). We pull out the integer client version and the
/// list to decide whether the client is below or above the accepted
/// set.
pub fn classify_schema_mismatch(body: &str, client_schema_version: u32) -> SchemaMismatchKind {
    let Some(prefix) = body.find("Unsupported schema_version:") else {
        return SchemaMismatchKind::NotSchemaRelated;
    };
    let tail = &body[prefix + "Unsupported schema_version:".len()..];
    let Some(dot) = tail.find('.') else {
        return SchemaMismatchKind::NotSchemaRelated;
    };
    let client_str = tail[..dot].trim();
    let Ok(_client_from_body) = client_str.parse::<u32>() else {
        return SchemaMismatchKind::NotSchemaRelated;
    };
    // We trust the *client's* known version over whatever the cloud echoed
    // back (the wire could carry a typo, future cloud format change, etc.).
    let client = client_schema_version;

    let after_dot = &tail[dot + 1..];
    let Some(open) = after_dot.find('[') else {
        return SchemaMismatchKind::NotSchemaRelated;
    };
    let Some(close) = after_dot[open..].find(']') else {
        return SchemaMismatchKind::NotSchemaRelated;
    };
    let list_str = &after_dot[open + 1..open + close];
    let expected: Vec<u32> = list_str
        .split(',')
        .filter_map(|s| s.trim().parse::<u32>().ok())
        .collect();
    if expected.is_empty() {
        return SchemaMismatchKind::NotSchemaRelated;
    }
    let min = *expected.iter().min().unwrap();
    let max = *expected.iter().max().unwrap();
    if client < min {
        SchemaMismatchKind::ClientTooOld {
            client,
            expected_min: min,
        }
    } else if client > max {
        SchemaMismatchKind::CloudTooOld {
            client,
            expected_max: max,
        }
    } else {
        // The body claims the client's schema_version is unsupported
        // but the parsed expected set contains it. The daemon shouldn't
        // be hitting this arm at all; treat as a generic rejection so
        // the body is at least surfaced verbatim.
        SchemaMismatchKind::NotSchemaRelated
    }
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
    /// Workspace identifier returned by `GET /v1/whoami`.
    ///
    /// #836: dual-accept the legacy `org_id` key via `serde(alias)` so a
    /// fresh CLI can still seed `cloud.toml` from an older cloud that hasn't
    /// shipped the workspace rename yet (siropkin/budi-cloud#321). The alias
    /// is dropped once the cloud-side rename lands and one release cycle of
    /// mixed-version operation has passed. ADR-0083 §2.
    #[serde(alias = "org_id")]
    pub workspace_id: String,
}

/// #541: outcome of a `whoami` call — distinguishes the fatal cases the
/// CLI wants to surface (bad key) from the benign "cloud doesn't expose
/// this yet" case the CLI wants to fall through on. A fresh 8.3.x CLI
/// pointed at an old self-hosted cloud without `/v1/whoami` keeps the
/// pre-#541 behavior (template with commented `device_id` / `workspace_id`
/// lines) rather than hard-failing `budi cloud init`.
#[derive(Debug, Clone)]
pub enum WhoamiOutcome {
    /// Cloud authenticated the key and returned `workspace_id`.
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
/// Used by `budi cloud init --api-key KEY` to auto-seed `workspace_id`
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

    // #756: ureq's default classifies any 4xx/5xx as
    // `Err(Error::StatusCode(n))` and drops the response body, which is
    // exactly what we need to read to surface the cloud's real rejection
    // message. Disable the auto-error so the response always reaches us
    // and we can branch on `status()` while still owning the body.
    // Mirrors the post-#751 pattern in `workers/team_pricing.rs`.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .http_status_as_error(false)
        .build()
        .into();

    let result = agent
        .post(&url)
        .header("Authorization", &format!("Bearer {api_key}"))
        .send_json(envelope);

    match result {
        Ok(mut response) => {
            let status = response.status().as_u16();
            if (200..300).contains(&status) {
                match response.body_mut().read_json::<IngestResponse>() {
                    Ok(resp) => SyncResult::Success(resp),
                    Err(e) => SyncResult::TransientError(format!("Failed to parse response: {e}")),
                }
            } else if status == 401 {
                SyncResult::AuthFailure
            } else if status == 422 {
                let body = response
                    .body_mut()
                    .read_to_string()
                    .unwrap_or_else(|_| String::new());
                let kind = classify_schema_mismatch(&body, envelope.schema_version);
                SyncResult::SchemaMismatch(SchemaMismatch { body, kind })
            } else if status == 429 || status >= 500 {
                SyncResult::TransientError(format!("Server returned {status}"))
            } else {
                let body = response
                    .body_mut()
                    .read_to_string()
                    .unwrap_or_else(|_| String::new());
                if body.is_empty() {
                    SyncResult::TransientError(format!("Server returned {status}"))
                } else {
                    SyncResult::TransientError(format!("Server returned {status}: {body}"))
                }
            }
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
    let workspace_id = envelope.workspace_id;
    let label = envelope.label;
    let schema_version = envelope.schema_version;

    for chunk in chunks {
        let chunk_envelope = SyncEnvelope {
            schema_version,
            device_id: device_id.clone(),
            workspace_id: workspace_id.clone(),
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

#[cfg(test)]
mod tests;
