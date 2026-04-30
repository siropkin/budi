//! SQLite-backed analytics storage for AI coding agent usage data.
//!
//! Stores sessions, messages, and tool usage extracted from JSONL transcript
//! files across all providers. Supports incremental ingestion via sync state
//! tracking (byte offset per file).

mod health;
mod queries;
mod sessions;
mod sync;
#[cfg(test)]
mod tests;

pub use health::*;
pub use queries::*;
pub use sessions::*;
pub use sync::*;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};

use crate::jsonl::ParsedMessage;

/// Sentinel row key in `sync_state` that stores the timestamp of the most
/// recent successful sync completion (independent of per-file offsets).
pub const SYNC_COMPLETION_MARKER_KEY: &str = "__budi_sync_completed__";

/// Open the analytics database with pragmas only (no migration).
/// Use `open_db_with_migration` for paths that should auto-migrate.
pub fn open_db(db_path: &Path) -> Result<Connection> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create dir {}", parent.display()))?;
    }
    let conn = Connection::open(db_path)?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA foreign_keys=ON;
         PRAGMA cache_size=-40000;
         PRAGMA mmap_size=268435456;
         PRAGMA synchronous=NORMAL;
         PRAGMA busy_timeout=5000;",
    )?;
    Ok(conn)
}

/// Open the analytics database and run pending migrations.
/// Used by `budi init`, `budi update`, and `budi db migrate`.
pub fn open_db_with_migration(db_path: &Path) -> Result<Connection> {
    let conn = open_db(db_path)?;
    crate::migration::migrate(&conn)?;
    Ok(conn)
}

/// Returns the stored byte offset for a given JSONL file path, or 0 if unseen.
pub fn get_sync_offset(conn: &Connection, file_path: &str) -> Result<usize> {
    let result = conn.query_row(
        "SELECT byte_offset FROM sync_state WHERE file_path = ?1",
        params![file_path],
        |row| row.get::<_, i64>(0),
    );
    match result {
        Ok(offset) => Ok(offset.max(0) as usize),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(0),
        Err(e) => Err(e.into()),
    }
}

/// Update the sync offset for a JSONL file.
pub fn set_sync_offset(conn: &Connection, file_path: &str, offset: usize) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO sync_state (file_path, byte_offset, last_synced)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(file_path) DO UPDATE SET byte_offset = ?2, last_synced = ?3",
        params![file_path, offset as i64, now],
    )?;
    Ok(())
}

/// Look up the live tailer's stored byte offset for `(provider, path)`.
///
/// Returns `Ok(None)` when no row exists, signalling that the tailer has
/// never observed this file before. Callers use that signal to decide
/// whether to seek to end-of-file (the daemon's "skip the backfill"
/// behaviour, owned by `budi db import`) or to resume from the stored
/// offset. See [ADR-0089] §1 / #319.
///
/// [ADR-0089]: https://github.com/siropkin/budi/blob/main/docs/adr/0089-reverse-proxy-first-jsonl-tailing-as-sole-live-path.md
pub fn get_tail_offset(conn: &Connection, provider: &str, path: &str) -> Result<Option<usize>> {
    let result = conn.query_row(
        "SELECT byte_offset FROM tail_offsets WHERE provider = ?1 AND path = ?2",
        params![provider, path],
        |row| row.get::<_, i64>(0),
    );
    match result {
        Ok(offset) => Ok(Some(offset.max(0) as usize)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Persist the tailer's byte offset for `(provider, path)`. Inserts when
/// missing, updates otherwise; `last_seen` is refreshed on every call so
/// stale entries are easy to identify later.
///
/// Distinct from [`set_sync_offset`]: that table is shared with `budi
/// import` and keyed on path alone. The tailer's offsets must not collide
/// with import's offsets — if a user ever runs both for the same file,
/// they should advance independently.
pub fn set_tail_offset(conn: &Connection, provider: &str, path: &str, offset: usize) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO tail_offsets (provider, path, byte_offset, last_seen)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(provider, path) DO UPDATE SET byte_offset = ?3, last_seen = ?4",
        params![provider, path, offset as i64, now],
    )?;
    Ok(())
}

/// Record that a full sync run completed successfully at the current time.
pub fn mark_sync_completed(conn: &Connection) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO sync_state (file_path, byte_offset, last_synced)
         VALUES (?1, 0, ?2)
         ON CONFLICT(file_path) DO UPDATE SET last_synced = ?2",
        params![SYNC_COMPLETION_MARKER_KEY, now],
    )?;
    Ok(())
}

/// Return the timestamp of the latest successful sync completion.
pub fn last_sync_completed_at(conn: &Connection) -> Result<Option<String>> {
    match conn.query_row(
        "SELECT last_synced FROM sync_state WHERE file_path = ?1",
        params![SYNC_COMPLETION_MARKER_KEY],
        |r| r.get::<_, String>(0),
    ) {
        Ok(ts) => Ok(Some(ts)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Return the timestamp of the newest ingested assistant message, if any.
pub fn newest_ingested_data_at(conn: &Connection) -> Result<Option<String>> {
    conn.query_row(
        "SELECT MAX(timestamp) FROM messages WHERE role = 'assistant'",
        [],
        |r| r.get::<_, Option<String>>(0),
    )
    .map_err(Into::into)
}

/// Reset sync state and re-ingested data so the next sync starts from scratch.
/// Used by `budi db import --force` after schema/parser changes.
pub fn reset_sync_state(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "DELETE FROM sync_state;
         DELETE FROM tags;
         DELETE FROM messages;
         DELETE FROM sessions;",
    )?;
    Ok(())
}

/// A tag to be stored alongside a message.
#[derive(Debug, Clone)]
pub struct Tag {
    pub key: String,
    pub value: String,
}

/// A single message row for the messages list endpoint.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MessageRow {
    #[serde(alias = "uuid")]
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub timestamp: String,
    pub role: String,
    pub model: Option<String>,
    pub provider: String,
    pub repo_id: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub cost_cents: f64,
    pub cost_confidence: String,
    pub git_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assistant_sequence: Option<u64>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub tags: Vec<SessionTag>,
}

#[derive(Debug, Clone)]
struct OtelMatchCandidate {
    id: String,
    request_id: Option<String>,
    timestamp: String,
    input_tokens: i64,
    output_tokens: i64,
    cache_creation_tokens: i64,
    cache_read_tokens: i64,
}

#[derive(Debug, Clone, Copy)]
enum OtelMatchStrategy {
    ExactRequestId,
    SourceFingerprint,
    TimestampFallback,
}

impl OtelMatchStrategy {
    fn as_str(self) -> &'static str {
        match self {
            Self::ExactRequestId => "exact_request_id",
            Self::SourceFingerprint => "source_fingerprint",
            Self::TimestampFallback => "timestamp_fallback",
        }
    }
}

fn normalize_nonempty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|s| !s.is_empty())
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

fn fingerprint_matches(candidate: &OtelMatchCandidate, msg: &ParsedMessage) -> bool {
    candidate.input_tokens == msg.input_tokens as i64
        && candidate.output_tokens == msg.output_tokens as i64
        && candidate.cache_creation_tokens == msg.cache_creation_tokens as i64
        && candidate.cache_read_tokens == msg.cache_read_tokens as i64
}

fn choose_otel_match_candidate<'a>(
    candidates: &'a [OtelMatchCandidate],
    msg: &ParsedMessage,
) -> Option<(&'a OtelMatchCandidate, OtelMatchStrategy)> {
    if candidates.is_empty() {
        return None;
    }

    if let Some(request_id) = normalize_nonempty(msg.request_id.as_deref()) {
        let by_request_id: Vec<&OtelMatchCandidate> = candidates
            .iter()
            .filter(|candidate| {
                normalize_nonempty(candidate.request_id.as_deref()) == Some(request_id)
            })
            .collect();
        if let Some(best) = by_request_id
            .into_iter()
            .min_by_key(|candidate| timestamp_distance_millis(&candidate.timestamp, msg.timestamp))
        {
            return Some((best, OtelMatchStrategy::ExactRequestId));
        }
    }

    let by_fingerprint: Vec<&OtelMatchCandidate> = candidates
        .iter()
        .filter(|candidate| fingerprint_matches(candidate, msg))
        .collect();
    if by_fingerprint.len() == 1 {
        return Some((by_fingerprint[0], OtelMatchStrategy::SourceFingerprint));
    }
    if by_fingerprint.len() > 1 {
        return None;
    }

    if candidates.len() == 1 {
        return Some((&candidates[0], OtelMatchStrategy::TimestampFallback));
    }

    None
}

/// Ingest a batch of parsed messages into the database.
/// `tags` is parallel to `messages` — each entry is the list of tags for that message.
/// If `sync_file` is provided, atomically updates the sync offset in the same transaction.
pub fn ingest_messages(
    conn: &mut Connection,
    messages: &[ParsedMessage],
    tags: Option<&[Vec<Tag>]>,
) -> Result<usize> {
    ingest_messages_with_sync(conn, messages, tags, None, None)
}

/// Ingest messages and optionally update one or both offset tables atomically.
///
/// `sync_file` writes a `(file_path, byte_offset)` row into `sync_state` and is
/// what `budi db import` uses. `tail_file` writes a `(provider, path, byte_offset)`
/// row into `tail_offsets` and is what the live tailer uses (#319, #382).
///
/// When both are `Some`, both writes happen inside the same transaction as the
/// message inserts, so the daemon cannot crash between persisting messages and
/// advancing its offset and end up reprocessing the same byte range on restart.
pub fn ingest_messages_with_sync(
    conn: &mut Connection,
    messages: &[ParsedMessage],
    tags: Option<&[Vec<Tag>]>,
    sync_file: Option<(&str, usize)>,
    tail_file: Option<(&str, &str, usize)>,
) -> Result<usize> {
    let tx = conn.transaction()?;
    let mut count = 0;

    for (i, msg) in messages.iter().enumerate() {
        // Insert message (skip duplicates).
        let ts = msg.timestamp.to_rfc3339();
        let normalized_session_id = msg
            .session_id
            .as_deref()
            .map(crate::identity::normalize_session_id);
        // cost_cents is set by CostEnricher in the pipeline before ingest
        let cost_cents = msg.cost_cents;
        // Strip refs/heads/ prefix from git_branch at write time
        let git_branch = msg
            .git_branch
            .as_deref()
            .map(|b| b.strip_prefix("refs/heads/").unwrap_or(b));

        // OTEL dedup: prefer request_id and token fingerprint matching before
        // constrained timestamp fallback so nearby same-model calls do not merge.
        if msg.role == "assistant" && normalized_session_id.is_some() && msg.model.is_some() {
            let session_id = normalized_session_id.as_deref().unwrap_or_default();
            let model = msg.model.as_deref().unwrap_or_default();
            // Pre-compute ±1 second window for index-friendly range predicates
            let ts_lo = (msg.timestamp - chrono::Duration::seconds(1)).to_rfc3339();
            let ts_hi = (msg.timestamp + chrono::Duration::seconds(1)).to_rfc3339();
            let mut stmt = tx.prepare_cached(
                "SELECT id, request_id, timestamp, input_tokens, output_tokens,
                        cache_creation_tokens, cache_read_tokens
                 FROM messages
                 WHERE session_id = ?1
                   AND model = ?2
                   AND role = 'assistant'
                   AND cost_confidence = 'otel_exact'
                   AND timestamp BETWEEN ?3 AND ?4",
            )?;
            let otel_candidates: Vec<OtelMatchCandidate> = stmt
                .query_map(params![session_id, model, ts_lo, ts_hi], |row| {
                    Ok(OtelMatchCandidate {
                        id: row.get(0)?,
                        request_id: row.get(1)?,
                        timestamp: row.get(2)?,
                        input_tokens: row.get(3)?,
                        output_tokens: row.get(4)?,
                        cache_creation_tokens: row.get(5)?,
                        cache_read_tokens: row.get(6)?,
                    })
                })?
                .filter_map(|r| r.ok())
                .collect();

            let otel_selection = choose_otel_match_candidate(&otel_candidates, msg);
            if otel_candidates.len() > 1 && otel_selection.is_none() {
                let fingerprint_matches = otel_candidates
                    .iter()
                    .filter(|candidate| fingerprint_matches(candidate, msg))
                    .count();
                tracing::warn!(
                    session_id = session_id,
                    model = model,
                    candidate_count = otel_candidates.len(),
                    fingerprint_matches,
                    "JSONL dedup found ambiguous OTEL candidates; preserving separate message"
                );
            }

            if let Some((candidate, strategy)) = otel_selection {
                let otel_id = candidate.id.clone();
                tracing::debug!(
                    session_id = session_id,
                    model = model,
                    strategy = strategy.as_str(),
                    message_id = %otel_id,
                    "JSONL dedup matched OTEL row for enrichment"
                );
                // Enrich the OTEL row with JSONL context (fill NULLs and empty sentinels)
                tx.execute(
                    "UPDATE messages SET
                        parent_uuid = COALESCE(parent_uuid, ?1),
                        cwd = COALESCE(NULLIF(cwd, ''), ?2),
                        git_branch = COALESCE(NULLIF(git_branch, ''), ?3),
                        repo_id = COALESCE(NULLIF(NULLIF(repo_id, ''), 'unknown'), ?4),
                        request_id = COALESCE(request_id, ?5)
                     WHERE id = ?6",
                    params![
                        msg.parent_uuid,
                        msg.cwd,
                        git_branch,
                        msg.repo_id,
                        msg.request_id,
                        otel_id
                    ],
                )?;
                // Insert tags for this message even though we skipped the INSERT
                if let Some(msg_tags) = tags.and_then(|t| t.get(i)) {
                    for tag in msg_tags {
                        tx.execute(
                            "INSERT OR IGNORE INTO tags (message_id, key, value) VALUES (?1, ?2, ?3)",
                            params![otel_id, tag.key, tag.value],
                        )?;
                    }
                }
                relink_unlinked_events_for_message(&tx, &otel_id)?;
                continue;
            }
        }

        // Cross-parse dedup: when Claude Code streams a multi-content-block response
        // (thinking → text → tool_use), each block is a separate JSONL entry with a
        // different UUID but the same request_id (message.id). If budi syncs mid-stream,
        // intermediate entries can be ingested in one parse, and the final entry in the
        // next. Without this check, both get inserted — double-counting input/cache tokens.
        // We keep the entry with the highest output_tokens (the final, authoritative one).
        if let Some(ref request_id) = msg.request_id {
            let existing: Option<(String, i64)> = tx
                .query_row(
                    "SELECT id, output_tokens FROM messages WHERE request_id = ?1 AND (?2 IS NULL OR session_id = ?2) LIMIT 1",
                    params![request_id, normalized_session_id.as_deref()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .ok();
            if let Some((existing_uuid, existing_output)) = existing {
                if (msg.output_tokens as i64) > existing_output {
                    // New entry has more output tokens — update the existing row in-place
                    // (keep its UUID to avoid FK violations on tags)
                    tx.execute(
                        "UPDATE messages SET
                            output_tokens = ?1,
                            cost_cents = ?2
                         WHERE id = ?3",
                        params![msg.output_tokens as i64, cost_cents, existing_uuid,],
                    )?;
                }
                // Either way, add tags to the surviving row and skip INSERT
                if let Some(msg_tags) = tags.and_then(|t| t.get(i)) {
                    for tag in msg_tags {
                        tx.execute(
                            "INSERT OR IGNORE INTO tags (message_id, key, value) VALUES (?1, ?2, ?3)",
                            params![existing_uuid, tag.key, tag.value],
                        )?;
                    }
                }
                relink_unlinked_events_for_message(&tx, &existing_uuid)?;
                continue;
            }
        }

        // `pricing_source` falls through to the column DEFAULT
        // (`'legacy:pre-manifest'`) when the pipeline hasn't set one.
        // In the normal live / import path CostEnricher and the Cursor
        // ingest arm always set it — this COALESCE-style fallback exists
        // so a hypothetical future writer that skips the enricher still
        // produces a valid tag rather than NULL.
        let pricing_source: Option<&str> = msg.pricing_source.as_deref();
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO messages
             (id, session_id, role, timestamp, model,
              input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
              cwd, repo_id, provider,
              cost_cents,
              parent_uuid, git_branch, cost_confidence, request_id, pricing_source)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                     COALESCE(?18, 'legacy:pre-manifest'))",
            params![
                msg.uuid,
                normalized_session_id.as_deref(),
                msg.role,
                ts,
                msg.model,
                msg.input_tokens as i64,
                msg.output_tokens as i64,
                msg.cache_creation_tokens as i64,
                msg.cache_read_tokens as i64,
                msg.cwd,
                msg.repo_id,
                msg.provider,
                cost_cents,
                msg.parent_uuid,
                git_branch,
                msg.cost_confidence,
                msg.request_id,
                pricing_source,
            ],
        )?;

        if inserted > 0 {
            count += 1;
            // Insert tags.
            if let Some(msg_tags) = tags.and_then(|t| t.get(i)) {
                for tag in msg_tags {
                    tx.execute(
                        "INSERT OR IGNORE INTO tags (message_id, key, value) VALUES (?1, ?2, ?3)",
                        params![msg.uuid, tag.key, tag.value],
                    )?;
                }
            }
            relink_unlinked_events_for_message(&tx, &msg.uuid)?;
        }
    }

    // Ensure stub session rows exist for every session_id we just ingested.
    // This makes `sessions` a merged metadata table populated from any source,
    // not only hooks. Hooks/OTEL will later enrich these stubs with metadata.
    {
        let mut seen_sessions: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::new();
        let mut session_categories: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for msg in messages {
            if let Some(sid) = msg
                .session_id
                .as_deref()
                .map(crate::identity::normalize_session_id)
                .filter(|sid| !sid.is_empty())
            {
                seen_sessions.insert((sid.clone(), msg.provider.clone()));
                if let Some(ref cat) = msg.prompt_category {
                    session_categories.entry(sid).or_insert_with(|| cat.clone());
                }
            }
        }
        for (sid, provider) in &seen_sessions {
            tx.execute(
                "INSERT OR IGNORE INTO sessions (id, provider) VALUES (?1, ?2)",
                params![sid, provider],
            )?;
            // Without this, claude_code/codex sessions keep started_at/ended_at/
            // repo_id/git_branch = NULL forever (#569 / #577) and never reach
            // `fetch_session_summaries`, so `session_summaries` on cloud goes
            // silently empty. `started_at` is immutable so COALESCE preserves
            // any pre-set value; `ended_at` must keep advancing for in-flight
            // sessions so we always recompute MAX(messages.timestamp) (#578 —
            // pre-fix `COALESCE(ended_at, MAX)` froze it at the first tick's
            // MAX, leaving every active session rendered as `<1m` on cloud).
            tx.execute(
                "UPDATE sessions SET
                    started_at = COALESCE(started_at,
                        (SELECT MIN(timestamp) FROM messages WHERE session_id = ?1)),
                    ended_at =
                        (SELECT MAX(timestamp) FROM messages WHERE session_id = ?1),
                    repo_id = COALESCE(NULLIF(sessions.repo_id, ''),
                        (SELECT m.repo_id FROM messages m
                          WHERE m.session_id = ?1
                            AND m.repo_id IS NOT NULL AND m.repo_id <> ''
                          ORDER BY m.timestamp DESC LIMIT 1)),
                    git_branch = COALESCE(NULLIF(sessions.git_branch, ''),
                        (SELECT m.git_branch FROM messages m
                          WHERE m.session_id = ?1
                            AND m.git_branch IS NOT NULL AND m.git_branch <> ''
                          ORDER BY m.timestamp DESC LIMIT 1))
                 WHERE id = ?1",
                params![sid],
            )?;
        }
        for (sid, category) in &session_categories {
            tx.execute(
                "UPDATE sessions SET prompt_category = ?2
                 WHERE id = ?1 AND (prompt_category IS NULL OR prompt_category = '')",
                params![sid, category],
            )?;
        }
    }

    // Atomically update sync offset in the same transaction
    if let Some((file_path, offset)) = sync_file {
        let now = Utc::now().to_rfc3339();
        tx.execute(
            "INSERT INTO sync_state (file_path, byte_offset, last_synced)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(file_path) DO UPDATE SET byte_offset = ?2, last_synced = ?3",
            params![file_path, offset as i64, now],
        )?;
    }

    // Atomically update the live tailer's per-(provider, path) offset.
    // Mirrors `set_tail_offset`'s upsert exactly so the in-tx and
    // standalone code paths stay byte-for-byte equivalent (#382).
    if let Some((provider, path, offset)) = tail_file {
        let now = Utc::now().to_rfc3339();
        tx.execute(
            "INSERT INTO tail_offsets (provider, path, byte_offset, last_seen)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(provider, path) DO UPDATE SET byte_offset = ?3, last_seen = ?4",
            params![provider, path, offset as i64, now],
        )?;
    }

    tx.commit()?;
    Ok(count)
}

fn relink_unlinked_events_for_message(_conn: &Connection, _message_id: &str) -> Result<()> {
    Ok(())
}

/// Resolve the default analytics DB path.
pub fn db_path() -> Result<PathBuf> {
    let home_dir = crate::config::budi_home_dir()?;
    Ok(home_dir.join("analytics.db"))
}
