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
/// Used by `budi init`, `budi update`, and `budi migrate`.
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

/// Reset sync state and re-ingested data so the next sync starts from scratch.
/// Used by `budi sync --force` after schema/parser changes.
///
/// Preserves `hook_events` — they come from real-time stdin hooks and cannot be
/// reconstructed from source files. Sessions are rebuilt from the preserved
/// hook_events so Cursor session attribution survives the reset.
pub fn reset_sync_state(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "DELETE FROM sync_state;
         DELETE FROM tags;
         DELETE FROM messages;
         DELETE FROM sessions;",
    )?;
    rebuild_sessions_from_hooks(conn)?;
    Ok(())
}

/// Replay all hook_events to rebuild the sessions table.
/// Re-parses each event's raw_json and calls `upsert_session`, preserving
/// the original timestamp ordering so session metadata accumulates correctly.
fn rebuild_sessions_from_hooks(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT raw_json, provider, timestamp FROM hook_events
         WHERE session_id IS NOT NULL
         ORDER BY timestamp ASC",
    )?;

    let rows: Vec<(String, String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .context("Failed to query hook_events for session rebuild")?
        .filter_map(|r| {
            r.inspect_err(|e| tracing::warn!("Failed to map hook_event row: {e}"))
                .ok()
        })
        .collect();

    let mut rebuilt = 0;
    for (raw_json, _provider, stored_ts) in &rows {
        let json: serde_json::Value = match serde_json::from_str(raw_json) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let mut event = match crate::hooks::parse_hook_event(&json) {
            Ok(e) => e,
            Err(_) => continue,
        };
        // Use the stored timestamp (when the hook was originally received)
        // instead of Utc::now() which parse_hook_event would set.
        if let Ok(ts) = stored_ts.parse::<chrono::DateTime<chrono::Utc>>() {
            event.timestamp = ts;
        }
        if crate::hooks::upsert_session(conn, &event).is_ok() {
            rebuilt += 1;
        }
    }

    if rebuilt > 0 {
        tracing::info!("Rebuilt sessions from {rebuilt} hook events");
    }
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
    pub uuid: String,
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
    #[serde(default)]
    pub tools: Vec<String>,
}

/// Ingest a batch of parsed messages into the database.
/// `tags` is parallel to `messages` — each entry is the list of tags for that message.
/// If `sync_file` is provided, atomically updates the sync offset in the same transaction.
pub fn ingest_messages(
    conn: &mut Connection,
    messages: &[ParsedMessage],
    tags: Option<&[Vec<Tag>]>,
) -> Result<usize> {
    ingest_messages_with_sync(conn, messages, tags, None)
}

/// Ingest messages and optionally update sync offset atomically.
pub fn ingest_messages_with_sync(
    conn: &mut Connection,
    messages: &[ParsedMessage],
    tags: Option<&[Vec<Tag>]>,
    sync_file: Option<(&str, usize)>,
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

        // OTEL dedup: if an otel_exact row already covers this API call (same session +
        // model + close timestamp but different UUID), don't insert a duplicate. Instead,
        // enrich the OTEL row with JSONL-only context (parent_uuid, cwd, git_branch)
        // that OTEL doesn't carry.
        if msg.role == "assistant" && normalized_session_id.is_some() && msg.model.is_some() {
            // Pre-compute ±1 second window for index-friendly range predicates
            let ts_lo = (msg.timestamp - chrono::Duration::seconds(1)).to_rfc3339();
            let ts_hi = (msg.timestamp + chrono::Duration::seconds(1)).to_rfc3339();
            let otel_uuid: Option<String> = tx
                .query_row(
                    "SELECT uuid FROM messages
                    WHERE session_id = ?1
                       AND model = ?2
                       AND role = 'assistant'
                       AND cost_confidence = 'otel_exact'
                       AND timestamp BETWEEN ?3 AND ?4
                     LIMIT 1",
                    params![normalized_session_id.as_deref(), msg.model, ts_lo, ts_hi],
                    |row| row.get(0),
                )
                .ok();
            if let Some(otel_id) = otel_uuid {
                // Enrich the OTEL row with JSONL context (fill NULLs and empty sentinels)
                tx.execute(
                    "UPDATE messages SET
                        parent_uuid = COALESCE(parent_uuid, ?1),
                        cwd = COALESCE(NULLIF(cwd, ''), ?2),
                        git_branch = COALESCE(NULLIF(git_branch, ''), ?3),
                        repo_id = COALESCE(NULLIF(NULLIF(repo_id, ''), 'unknown'), ?4),
                        request_id = COALESCE(request_id, ?5)
                     WHERE uuid = ?6",
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
                            "INSERT OR IGNORE INTO tags (message_uuid, key, value) VALUES (?1, ?2, ?3)",
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
                    "SELECT uuid, output_tokens FROM messages WHERE request_id = ?1 AND (?2 IS NULL OR session_id = ?2) LIMIT 1",
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
                         WHERE uuid = ?3",
                        params![msg.output_tokens as i64, cost_cents, existing_uuid,],
                    )?;
                }
                // Either way, add tags to the surviving row and skip INSERT
                if let Some(msg_tags) = tags.and_then(|t| t.get(i)) {
                    for tag in msg_tags {
                        tx.execute(
                            "INSERT OR IGNORE INTO tags (message_uuid, key, value) VALUES (?1, ?2, ?3)",
                            params![existing_uuid, tag.key, tag.value],
                        )?;
                    }
                }
                relink_unlinked_events_for_message(&tx, &existing_uuid)?;
                continue;
            }
        }

        let inserted = tx.execute(
            "INSERT OR IGNORE INTO messages
             (uuid, session_id, role, timestamp, model,
              input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
              cwd, repo_id, provider,
              cost_cents,
              parent_uuid, git_branch, cost_confidence, request_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
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
            ],
        )?;

        if inserted > 0 {
            count += 1;
            // Insert tags.
            if let Some(msg_tags) = tags.and_then(|t| t.get(i)) {
                for tag in msg_tags {
                    tx.execute(
                        "INSERT OR IGNORE INTO tags (message_uuid, key, value) VALUES (?1, ?2, ?3)",
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
                "INSERT OR IGNORE INTO sessions (session_id, provider) VALUES (?1, ?2)",
                params![sid, provider],
            )?;
        }
        for (sid, category) in &session_categories {
            tx.execute(
                "UPDATE sessions SET prompt_category = ?2
                 WHERE session_id = ?1 AND (prompt_category IS NULL OR prompt_category = '')",
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

    tx.commit()?;
    Ok(count)
}

fn relink_unlinked_events_for_message(conn: &Connection, message_id: &str) -> Result<()> {
    let row: Option<(
        Option<String>,
        Option<String>,
        String,
        String,
        String,
        Option<String>,
        Option<f64>,
    )> = conn
        .query_row(
            "SELECT session_id, request_id, timestamp, role, cost_confidence, model, cost_cents
             FROM messages
             WHERE uuid = ?1",
            params![message_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .ok();

    let Some((session_id, request_id, timestamp, role, cost_confidence, model, cost_cents)) = row
    else {
        return Ok(());
    };
    let Some(session_id) = session_id.filter(|sid| !sid.trim().is_empty()) else {
        return Ok(());
    };

    if let Some(request_id) = request_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        conn.execute(
            "UPDATE hook_events
             SET message_id = ?1,
                 link_confidence = ?2
             WHERE message_id IS NULL
               AND session_id = ?3
               AND message_request_id = ?4",
            params![
                message_id,
                crate::hooks::HOOK_LINK_EXACT_REQUEST_ID,
                session_id,
                request_id
            ],
        )?;
    }

    let mut tool_stmt = conn.prepare(
        "SELECT DISTINCT value
         FROM tags
         WHERE message_uuid = ?1
           AND key = 'tool_use_id'
           AND TRIM(value) <> ''",
    )?;
    let tool_use_ids: Vec<String> = tool_stmt
        .query_map(params![message_id], |row| row.get::<_, String>(0))?
        .filter_map(|r| r.ok())
        .collect();
    for tool_use_id in tool_use_ids {
        conn.execute(
            "UPDATE hook_events
             SET message_id = ?1,
                 link_confidence = CASE
                    WHEN link_confidence IS NULL OR TRIM(link_confidence) = '' OR link_confidence = ?2
                    THEN ?3
                    ELSE link_confidence
                 END
             WHERE message_id IS NULL
               AND session_id = ?4
               AND tool_use_id = ?5",
            params![
                message_id,
                crate::hooks::HOOK_LINK_UNLINKED,
                crate::hooks::HOOK_LINK_EXACT_TOOL_USE_ID,
                session_id,
                tool_use_id
            ],
        )?;
    }

    if role == "assistant"
        && cost_confidence == "otel_exact"
        && let Ok(event_ts) = DateTime::parse_from_rfc3339(&timestamp)
    {
        let event_ts = event_ts.with_timezone(&Utc);
        let ts_lo = (event_ts - chrono::Duration::seconds(1)).to_rfc3339();
        let ts_hi = (event_ts + chrono::Duration::seconds(1)).to_rfc3339();
        let otel_id: Option<i64> = conn
            .query_row(
                "SELECT id
                 FROM otel_events
                 WHERE session_id = ?1
                   AND message_id IS NULL
                   AND timestamp BETWEEN ?2 AND ?3
                 ORDER BY ABS(strftime('%s', timestamp) - strftime('%s', ?4)), id DESC
                 LIMIT 1",
                params![session_id, ts_lo, ts_hi, timestamp],
                |row| row.get(0),
            )
            .ok();
        if let Some(otel_id) = otel_id {
            conn.execute(
                "UPDATE otel_events
                 SET message_id = ?2,
                     model = COALESCE(model, ?3),
                     cost_cents_computed = COALESCE(cost_cents_computed, ?4)
                 WHERE id = ?1",
                params![otel_id, message_id, model, cost_cents],
            )?;
        }
    }

    Ok(())
}

/// Resolve the default analytics DB path.
pub fn db_path() -> Result<PathBuf> {
    let home_dir = crate::config::budi_home_dir()?;
    Ok(home_dir.join("analytics.db"))
}
