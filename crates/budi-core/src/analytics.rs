//! SQLite-backed analytics storage for AI coding agent usage data.
//!
//! Stores sessions, messages, and tool usage extracted from JSONL transcript
//! files across all providers. Supports incremental ingestion via sync state
//! tracking (byte offset per file).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;
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
        Ok(offset) => Ok(offset as usize),
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

/// Reset all sync state and data so the next sync re-ingests everything from scratch.
/// Used by `budi sync --force` after schema/parser changes.
pub fn reset_sync_state(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "DELETE FROM sync_state;
         DELETE FROM tags;
         DELETE FROM messages;
         DELETE FROM sessions;
         DELETE FROM hook_events;",
    )?;
    Ok(())
}

/// A tag to be stored alongside a message.
#[derive(Debug, Clone)]
pub struct Tag {
    pub key: String,
    pub value: String,
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
        if msg.role == "assistant" && msg.session_id.is_some() && msg.model.is_some() {
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
                    params![msg.session_id, msg.model, ts_lo, ts_hi],
                    |row| row.get(0),
                )
                .ok();
            if let Some(otel_id) = otel_uuid {
                // Enrich the OTEL row with JSONL context (only fill NULLs)
                tx.execute(
                    "UPDATE messages SET
                        parent_uuid = COALESCE(parent_uuid, ?1),
                        cwd = COALESCE(cwd, ?2),
                        git_branch = COALESCE(git_branch, ?3),
                        repo_id = COALESCE(repo_id, ?4),
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
                    "SELECT uuid, output_tokens FROM messages WHERE request_id = ?1 LIMIT 1",
                    params![request_id],
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
                msg.session_id,
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

/// Path to the analytics database file.
pub fn db_path() -> Result<PathBuf> {
    let home_dir = crate::config::budi_home_dir()?;
    Ok(home_dir.join("analytics.db"))
}

/// Quick sync: only files modified in the last 30 days.
/// Used by `budi sync` and the daemon's 30s auto-sync.
pub fn sync_all(conn: &mut Connection) -> Result<(usize, usize, Vec<String>)> {
    sync_with_max_age(conn, Some(30))
}

/// Full history sync: process ALL transcript files regardless of age.
/// Used by `budi history` — may take minutes on large histories.
pub fn sync_history(conn: &mut Connection) -> Result<(usize, usize, Vec<String>)> {
    sync_with_max_age(conn, None)
}

/// Internal sync implementation with optional max_age filter.
/// When `max_age_days` is Some(N), only files modified in the last N days are processed.
/// When None, all files are processed.
fn sync_with_max_age(
    conn: &mut Connection,
    max_age_days: Option<u64>,
) -> Result<(usize, usize, Vec<String>)> {
    let providers = crate::provider::available_providers();
    let tags_config = crate::config::load_tags_config();
    let session_cache = crate::hooks::load_session_meta(conn, max_age_days).unwrap_or_default();
    let mut pipeline = crate::pipeline::Pipeline::default_pipeline(tags_config, session_cache);
    let mut total_files = 0;
    let mut total_messages = 0;
    let mut warnings: Vec<String> = Vec::new();

    let cutoff = max_age_days
        .map(|days| std::time::SystemTime::now() - std::time::Duration::from_secs(days * 86400));

    for provider in &providers {
        // Try direct sync first (e.g. Cursor Usage API).
        if let Some(result) = provider.sync_direct(conn, &mut pipeline, max_age_days) {
            match result {
                Ok((files, messages, w)) => {
                    total_files += files;
                    total_messages += messages;
                    warnings.extend(w);
                    continue;
                }
                Err(e) => {
                    tracing::warn!("Provider sync_direct failed: {e:#}");
                    continue;
                }
            }
        }

        let files = provider.discover_files()?;

        for discovered in &files {
            let file_path = &discovered.path;

            // Skip files older than cutoff (if set)
            if let Some(cutoff_time) = cutoff {
                let mtime = file_path
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                if mtime < cutoff_time {
                    continue; // Too old for quick sync
                }
            }

            let path_str = file_path.display().to_string();
            let offset = get_sync_offset(conn, &path_str)?;

            let content = match std::fs::read_to_string(file_path) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("Skipping {}: {e}", file_path.display());
                    warnings.push(format!("Skipped {}: {e}", file_path.display()));
                    continue;
                }
            };

            if offset >= content.len() {
                continue; // Already fully synced.
            }

            let (mut messages, new_offset) = provider.parse_file(file_path, &content, offset)?;
            if messages.is_empty() {
                set_sync_offset(conn, &path_str, new_offset)?;
                continue;
            }

            let tags = pipeline.process(&mut messages);
            let count = ingest_messages_with_sync(
                conn,
                &messages,
                Some(&tags),
                Some((&path_str, new_offset)),
            )?;

            if count > 0 {
                total_files += 1;
                total_messages += count;
            }
        }
    }

    Ok((total_files, total_messages, warnings))
}

/// Summary statistics for display.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UsageSummary {
    pub total_messages: u64,
    pub total_user_messages: u64,
    pub total_assistant_messages: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cache_creation_tokens: u64,
    pub total_cache_read_tokens: u64,
}

/// Validate an ISO 8601 date/datetime string.
/// Accepts formats: "2026-03-14", "2026-03-14T18:00:00Z", "2026-03-14T18:00:00+00:00".
fn is_valid_timestamp(s: &str) -> bool {
    // Must start with YYYY-MM-DD pattern
    if s.len() < 10 {
        return false;
    }
    let bytes = s.as_bytes();
    bytes[0..4].iter().all(|b| b.is_ascii_digit())
        && bytes[4] == b'-'
        && bytes[5..7].iter().all(|b| b.is_ascii_digit())
        && bytes[7] == b'-'
        && bytes[8..10].iter().all(|b| b.is_ascii_digit())
}

/// Build a parameterized date filter clause and its bind values.
/// Returns (clause_str, params_vec) where clause_str uses ?N placeholders.
/// Invalid timestamps are silently skipped (treated as None).
fn date_filter(since: Option<&str>, until: Option<&str>, keyword: &str) -> (String, Vec<String>) {
    let mut conditions = Vec::new();
    let mut param_values = Vec::new();
    if let Some(s) = since {
        if is_valid_timestamp(s) {
            param_values.push(s.to_string());
            conditions.push(format!("timestamp >= ?{}", param_values.len()));
        } else {
            tracing::warn!("date_filter: invalid 'since' timestamp ignored: {s}");
        }
    }
    if let Some(u) = until {
        if is_valid_timestamp(u) {
            param_values.push(u.to_string());
            conditions.push(format!("timestamp < ?{}", param_values.len()));
        } else {
            tracing::warn!("date_filter: invalid 'until' timestamp ignored: {u}");
        }
    }
    if conditions.is_empty() {
        (String::new(), param_values)
    } else {
        (
            format!("{} {}", keyword, conditions.join(" AND ")),
            param_values,
        )
    }
}

/// Query a usage summary, optionally filtered by date range.
/// Consolidated into a single scan of the messages table.
#[cfg(test)]
pub fn usage_summary(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<UsageSummary> {
    let (where_clause, date_params) = date_filter(since, until, "WHERE");
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = date_params
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    // Single scan: all aggregates in one query
    let sql = format!(
        "SELECT COUNT(*),
                COALESCE(SUM(CASE WHEN role = 'user' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN role = 'assistant' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(input_tokens), 0),
                COALESCE(SUM(output_tokens), 0),
                COALESCE(SUM(cache_creation_tokens), 0),
                COALESCE(SUM(cache_read_tokens), 0)
         FROM messages {}",
        where_clause
    );
    let (
        total_messages,
        total_user_messages,
        total_assistant_messages,
        total_input,
        total_output,
        total_cache_create,
        total_cache_read,
    ): (u64, u64, u64, u64, u64, u64, u64) = conn.query_row(&sql, param_refs.as_slice(), |r| {
        Ok((
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
            r.get(6)?,
        ))
    })?;

    Ok(UsageSummary {
        total_messages,
        total_user_messages,
        total_assistant_messages,
        total_input_tokens: total_input,
        total_output_tokens: total_output,
        total_cache_creation_tokens: total_cache_create,
        total_cache_read_tokens: total_cache_read,
    })
}

/// A single message row for the messages list endpoint.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MessageRow {
    pub uuid: String,
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
}

/// Paginated message list result.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PaginatedMessages {
    pub messages: Vec<MessageRow>,
    pub total_count: u64,
}

/// Parameters for paginated message queries.
pub struct MessageListParams<'a> {
    pub since: Option<&'a str>,
    pub until: Option<&'a str>,
    pub search: Option<&'a str>,
    pub sort_by: Option<&'a str>,
    pub sort_asc: bool,
    pub limit: usize,
    pub offset: usize,
}

/// List messages with pagination, search, and sorting.
pub fn message_list(conn: &Connection, p: &MessageListParams) -> Result<PaginatedMessages> {
    let mut conditions = vec!["messages.role = 'assistant'".to_string()];
    let mut param_values: Vec<String> = Vec::new();
    if let Some(s) = p.since {
        param_values.push(s.to_string());
        conditions.push(format!("messages.timestamp >= ?{}", param_values.len()));
    }
    if let Some(u) = p.until {
        param_values.push(u.to_string());
        conditions.push(format!("messages.timestamp < ?{}", param_values.len()));
    }
    if let Some(q) = p.search
        && !q.is_empty()
    {
        param_values.push(format!("%{q}%"));
        let idx = param_values.len();
        conditions.push(format!(
            "(messages.model LIKE ?{idx} OR messages.repo_id LIKE ?{idx} OR messages.provider LIKE ?{idx} OR COALESCE(messages.git_branch, s.git_branch) LIKE ?{idx} OR EXISTS (SELECT 1 FROM tags WHERE tags.message_uuid = messages.uuid AND tags.key = 'ticket_id' AND tags.value LIKE ?{idx}))"
        ));
    }
    let where_clause = format!("WHERE {}", conditions.join(" AND "));
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    let dir = if p.sort_asc { "ASC" } else { "DESC" };
    // For nullable text columns in ASC order, push NULLs/empty to the bottom.
    let order_expr = match p.sort_by.unwrap_or("timestamp") {
        col @ ("model" | "provider") => {
            let qcol = format!("messages.{col}");
            if p.sort_asc {
                format!("({qcol} IS NULL OR {qcol} = '') ASC, {qcol} {dir}")
            } else {
                format!("{qcol} {dir}")
            }
        }
        "tokens" => format!("(messages.input_tokens + messages.output_tokens) {dir}"),
        "cost" => format!("COALESCE(messages.cost_cents, 0.0) {dir}"),
        "branch" | "git_branch" | "ticket" => {
            let col = "COALESCE(messages.git_branch, s.git_branch)";
            if p.sort_asc {
                format!("({col} IS NULL OR {col} = '') ASC, {col} {dir}")
            } else {
                format!("{col} {dir}")
            }
        }
        "repo_id" => {
            let col = "COALESCE(messages.repo_id, s.repo_id)";
            if p.sort_asc {
                format!("({col} IS NULL OR {col} = '') ASC, {col} {dir}")
            } else {
                format!("{col} {dir}")
            }
        }
        _ => format!("messages.timestamp {dir}"),
    };

    let sql = format!(
        "SELECT COUNT(*) OVER() as total_count,
                messages.uuid, messages.timestamp, messages.role, messages.model,
                COALESCE(messages.provider, 'claude_code'),
                COALESCE(messages.repo_id, s.repo_id),
                messages.input_tokens, messages.output_tokens,
                messages.cache_creation_tokens, messages.cache_read_tokens,
                COALESCE(messages.cost_cents, 0.0),
                COALESCE(messages.cost_confidence, 'estimated'),
                COALESCE(messages.git_branch, s.git_branch)
         FROM messages
         LEFT JOIN sessions s ON s.conversation_id = messages.session_id
         {}
         ORDER BY {order_expr}
         LIMIT {} OFFSET {}",
        where_clause, p.limit, p.offset
    );

    // Count total matching rows separately so it's correct even when offset exceeds data
    let count_sql = format!(
        "SELECT COUNT(*)
         FROM messages
         LEFT JOIN sessions s ON s.conversation_id = messages.session_id
         {where_clause}"
    );
    let total_count: u64 = conn.query_row(&count_sql, param_refs.as_slice(), |row| row.get(0))?;

    let mut stmt = conn.prepare(&sql)?;
    let messages: Vec<MessageRow> = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(MessageRow {
                uuid: row.get(1)?,
                timestamp: row.get(2)?,
                role: row.get(3)?,
                model: row.get(4)?,
                provider: row.get(5)?,
                repo_id: row.get(6)?,
                input_tokens: row.get(7)?,
                output_tokens: row.get(8)?,
                cache_creation_tokens: row.get(9)?,
                cache_read_tokens: row.get(10)?,
                cost_cents: row.get(11)?,
                cost_confidence: row.get(12)?,
                git_branch: row.get(13)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(PaginatedMessages {
        messages,
        total_count,
    })
}

/// Repository usage stats, grouped by repo_id.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RepoUsage {
    pub repo_id: String,
    pub display_path: String,
    pub message_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_cents: f64,
}

/// Query repository usage, grouped by repo_id, optionally filtered by date.
pub fn repo_usage(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    limit: usize,
) -> Result<Vec<RepoUsage>> {
    // Build parameterized date filter. Limit param index starts after date params.
    // Single-query approach: COALESCE NULL repo_id into "(untagged)"
    let mut conditions = vec!["role = 'assistant'".to_string()];
    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(s) = since {
        param_values.push(Box::new(s.to_string()));
        conditions.push(format!("timestamp >= ?{}", param_values.len()));
    }
    if let Some(u) = until {
        param_values.push(Box::new(u.to_string()));
        conditions.push(format!("timestamp < ?{}", param_values.len()));
    }
    param_values.push(Box::new(limit as i64));
    let limit_idx = param_values.len();

    let sql = format!(
        "SELECT COALESCE(repo_id, '(untagged)') as repo,
                COALESCE(MIN(cwd), '(untagged)') as display_path,
                COUNT(*) as cnt,
                COALESCE(SUM(input_tokens), 0) as inp,
                COALESCE(SUM(output_tokens), 0) as outp,
                COALESCE(SUM(cost_cents), 0.0) as cost
         FROM messages
         WHERE {}
         GROUP BY repo
         ORDER BY cost DESC
         LIMIT ?{}",
        conditions.join(" AND "),
        limit_idx
    );

    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        param_values.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<RepoUsage> = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(RepoUsage {
                repo_id: row.get(0)?,
                display_path: row.get(1)?,
                message_count: row.get(2)?,
                input_tokens: row.get(3)?,
                output_tokens: row.get(4)?,
                cost_cents: row.get(5)?,
            })
        })?
        .filter_map(|r| match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("skipping row: {e}");
                None
            }
        })
        .collect();

    Ok(rows)
}

/// Activity data bucketed by time granularity.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ActivityBucket {
    pub label: String,
    pub message_count: u64,
    pub tool_call_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_cents: f64,
}

/// Query activity data with adaptive time granularity.
/// `granularity`: "hour", "day", "week", or "month"
/// `tz_offset_min`: timezone offset in minutes (e.g. -420 for PDT)
pub fn activity_chart(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    granularity: &str,
    tz_offset_min: i32,
) -> Result<Vec<ActivityBucket>> {
    let (where_clause, date_params) = date_filter(since, until, "WHERE");
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = date_params
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    // Apply timezone offset to get local time grouping
    let hours = tz_offset_min / 60;
    let mins = (tz_offset_min % 60).abs();
    let sign = if tz_offset_min >= 0 { "+" } else { "-" };
    let tz_adjust = if tz_offset_min != 0 {
        format!(
            "datetime(timestamp, '{}{:02}:{:02}')",
            sign,
            hours.abs(),
            mins
        )
    } else {
        "timestamp".to_string()
    };

    // Validate granularity to prevent any future SQL injection risk
    let group_expr = match granularity {
        "hour" => format!("strftime('%H:00', {})", tz_adjust),
        "month" => format!("strftime('%Y-%m', {})", tz_adjust),
        _ => format!("date({})", tz_adjust),
    };

    // Add role = 'assistant' to the WHERE clause
    let role_clause = if where_clause.is_empty() {
        "WHERE role = 'assistant'"
    } else {
        "AND role = 'assistant'"
    };

    let sql = format!(
        "SELECT {group_expr} as bucket, COUNT(*) as cnt,
                COALESCE(SUM(input_tokens), 0) as inp,
                COALESCE(SUM(output_tokens), 0) as outp,
                COALESCE(SUM(cost_cents), 0.0) as cost
         FROM messages {where_clause} {role_clause}
         GROUP BY bucket
         ORDER BY bucket",
    );

    let mut stmt = conn.prepare(&sql)?;
    let results = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(ActivityBucket {
                label: row.get(0)?,
                message_count: row.get(1)?,
                input_tokens: row.get(2)?,
                output_tokens: row.get(3)?,
                cost_cents: row.get(4)?,
                tool_call_count: 0,
            })
        })?
        .filter_map(|r| match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("skipping row: {e}");
                None
            }
        })
        .collect();

    Ok(results)
}

// Branch cost queries removed — use tag_stats(key="branch") instead.
// BranchCost struct kept as a thin wrapper for backward compatibility.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BranchCost {
    pub git_branch: String,
    pub repo_id: String,
    pub session_count: u64,
    pub message_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cost_cents: f64,
}

/// Query cost grouped by branch+repo using the denormalized git_branch column.
/// Groups by (git_branch, repo_id) so branches with the same name in different repos
/// are kept separate.
pub fn branch_cost(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    limit: usize,
) -> Result<Vec<BranchCost>> {
    let mut conditions = vec!["role = 'assistant'".to_string()];
    let mut param_values: Vec<String> = Vec::new();
    let mut idx = 0usize;

    if let Some(s) = since {
        idx += 1;
        conditions.push(format!("timestamp >= ?{idx}"));
        param_values.push(s.to_string());
    }
    if let Some(u) = until {
        idx += 1;
        conditions.push(format!("timestamp < ?{idx}"));
        param_values.push(u.to_string());
    }
    param_values.push(limit.to_string());
    let limit_idx = param_values.len();

    let where_clause = format!("WHERE {}", conditions.join(" AND "));
    // Single-query approach: COALESCE NULL/empty branches into "(untagged)"
    let sql = format!(
        "SELECT COALESCE(NULLIF(git_branch, ''), '(untagged)') as branch,
                CASE WHEN COALESCE(NULLIF(git_branch, ''), '(untagged)') = '(untagged)'
                     THEN '' ELSE COALESCE(repo_id, '') END as repo,
                COUNT(DISTINCT session_id) as sess,
                COUNT(*) as cnt,
                COALESCE(SUM(input_tokens), 0) as inp,
                COALESCE(SUM(output_tokens), 0) as outp,
                COALESCE(SUM(cache_read_tokens), 0) as cache_r,
                COALESCE(SUM(cache_creation_tokens), 0) as cache_c,
                COALESCE(SUM(cost_cents), 0.0) as cost
         FROM messages
         {where_clause}
         GROUP BY branch, repo
         ORDER BY cost DESC
         LIMIT ?{limit_idx}",
    );

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<BranchCost> = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(BranchCost {
                git_branch: row.get(0)?,
                repo_id: row.get(1)?,
                session_count: row.get(2)?,
                message_count: row.get(3)?,
                input_tokens: row.get(4)?,
                output_tokens: row.get(5)?,
                cache_read_tokens: row.get(6)?,
                cache_creation_tokens: row.get(7)?,
                cost_cents: row.get(8)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(rows)
}

/// Query cost for a single branch using a dedicated SQL query.
/// Unlike `branch_cost()` (which has LIMIT 20), this searches all branches.
pub fn branch_cost_single(
    conn: &Connection,
    branch: &str,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Option<BranchCost>> {
    let branch_stripped = branch.strip_prefix("refs/heads/").unwrap_or(branch);

    let mut conditions = vec![
        "role = 'assistant'".to_string(),
        "git_branch = ?1".to_string(),
    ];
    let mut param_values: Vec<String> = vec![branch_stripped.to_string()];
    let mut idx = 1usize;

    if let Some(s) = since {
        idx += 1;
        conditions.push(format!("timestamp >= ?{idx}"));
        param_values.push(s.to_string());
    }
    if let Some(u) = until {
        idx += 1;
        conditions.push(format!("timestamp < ?{idx}"));
        param_values.push(u.to_string());
    }

    let where_clause = format!("WHERE {}", conditions.join(" AND "));
    let sql = format!(
        "SELECT git_branch, COALESCE(repo_id, '') as repo,
                COUNT(DISTINCT session_id) as sess,
                COUNT(*) as cnt,
                COALESCE(SUM(input_tokens), 0) as inp,
                COALESCE(SUM(output_tokens), 0) as outp,
                COALESCE(SUM(cache_read_tokens), 0) as cache_r,
                COALESCE(SUM(cache_creation_tokens), 0) as cache_c,
                COALESCE(SUM(cost_cents), 0.0) as cost
         FROM messages
         {where_clause}
         GROUP BY git_branch, COALESCE(repo_id, '')
         ORDER BY cost DESC
         LIMIT 1",
    );

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map(param_refs.as_slice(), |row| {
        Ok(BranchCost {
            git_branch: row.get(0)?,
            repo_id: row.get(1)?,
            session_count: row.get(2)?,
            message_count: row.get(3)?,
            input_tokens: row.get(4)?,
            output_tokens: row.get(5)?,
            cache_read_tokens: row.get(6)?,
            cache_creation_tokens: row.get(7)?,
            cost_cents: row.get(8)?,
        })
    })?;

    match rows.next() {
        Some(Ok(bc)) => Ok(Some(bc)),
        Some(Err(e)) => Err(e.into()),
        None => Ok(None),
    }
}

/// A single tag key-value pair.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionTag {
    pub key: String,
    pub value: String,
}

/// Tag-based cost breakdown: cost grouped by tag key+value.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TagCost {
    pub key: String,
    pub value: String,
    pub session_count: u64,
    pub cost_cents: f64,
}

/// Query cost breakdown by tag, optionally filtered by tag key and date range.
/// Cost is per-message: sums cost_cents of all messages in sessions carrying each tag.
pub fn tag_stats(
    conn: &Connection,
    tag_key: Option<&str>,
    since: Option<&str>,
    until: Option<&str>,
    limit: usize,
) -> Result<Vec<TagCost>> {
    // Build params with sequential indices
    let mut param_values: Vec<String> = Vec::new();

    if let Some(k) = tag_key {
        param_values.push(k.to_string());
    }
    if let Some(s) = since {
        param_values.push(s.to_string());
    }
    if let Some(u) = until {
        param_values.push(u.to_string());
    }

    // ?last: limit
    param_values.push(limit.to_string());
    let limit_idx = param_values.len();

    // Param indices: tag_key is ?1 (if present), since/until follow, limit is last.
    // Build the WHERE for tag filter — goes on the tags subquery.
    let mut tag_where_parts = Vec::new();
    let mut date_where = String::new();
    {
        let mut idx = 0usize;
        if tag_key.is_some() {
            idx += 1;
            tag_where_parts.push(format!("t.key = ?{idx}"));
        }
        let mut dconds = Vec::new();
        let mut dconds_tm = Vec::new();
        if since.is_some() {
            idx += 1;
            dconds.push(format!("timestamp >= ?{idx}"));
            dconds_tm.push(format!("tm.timestamp >= ?{idx}"));
        }
        if until.is_some() {
            idx += 1;
            dconds.push(format!("timestamp < ?{idx}"));
            dconds_tm.push(format!("tm.timestamp < ?{idx}"));
        }
        if !dconds.is_empty() {
            date_where = format!("WHERE {}", dconds.join(" AND "));
        }
        // Date filter on tag_sessions CTE (applied to joined messages)
        if !dconds_tm.is_empty() {
            for c in &dconds_tm {
                tag_where_parts.push(c.clone());
            }
        }
    }
    let tag_where = if tag_where_parts.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", tag_where_parts.join(" AND "))
    };

    let role_filter = if date_where.is_empty() {
        "WHERE role = 'assistant'"
    } else {
        "AND role = 'assistant'"
    };

    // Build the untagged UNION clause for single-key queries.
    // This computes untagged cost in the same query instead of a separate total_assistant_cost() call.
    let untagged_union = if let Some(k) = tag_key {
        // Build date conditions for the untagged subquery using the same param indices
        let mut untagged_date_parts = Vec::new();
        {
            let mut uidx = 0usize;
            if tag_key.is_some() {
                uidx += 1; // skip tag_key param
            }
            if since.is_some() {
                uidx += 1;
                untagged_date_parts.push(format!("m.timestamp >= ?{uidx}"));
            }
            if until.is_some() {
                uidx += 1;
                untagged_date_parts.push(format!("m.timestamp < ?{uidx}"));
            }
        }
        let untagged_date_filter = if untagged_date_parts.is_empty() {
            String::new()
        } else {
            format!("AND {}", untagged_date_parts.join(" AND "))
        };
        // Build matching date filter for the tagged sessions subquery
        // so we only consider tags within the same date range
        let mut tagged_date_parts = Vec::new();
        {
            let mut tidx = 0usize;
            if tag_key.is_some() {
                tidx += 1;
            }
            if since.is_some() {
                tidx += 1;
                tagged_date_parts.push(format!("tm2.timestamp >= ?{tidx}"));
            }
            if until.is_some() {
                tidx += 1;
                tagged_date_parts.push(format!("tm2.timestamp < ?{tidx}"));
            }
        }
        let tagged_date_filter = if tagged_date_parts.is_empty() {
            String::new()
        } else {
            format!("AND {}", tagged_date_parts.join(" AND "))
        };
        format!(
            "UNION ALL
             SELECT '{k}' as key, '(untagged)' as value, 0 as session_count,
                    COALESCE(SUM(m.cost_cents), 0.0) as total_cost_cents
             FROM messages m
             LEFT JOIN (
                 SELECT DISTINCT tm2.session_id
                 FROM tags t2
                 JOIN messages tm2 ON t2.message_uuid = tm2.uuid
                 WHERE t2.key = ?1 {tagged_date_filter}
             ) tagged ON tagged.session_id = m.session_id
             WHERE m.role = 'assistant' {untagged_date_filter}
               AND tagged.session_id IS NULL"
        )
    } else {
        String::new()
    };

    // Use CTEs to compute tag-based cost splitting.
    // value_counts: how many distinct tag values per (key, session) — for even cost splitting.
    // tag_sessions: distinct (key, value, session_id) triples joined with value_count.
    let sql = format!(
        "WITH value_counts AS (
             SELECT t.key, tm.session_id, COUNT(DISTINCT t.value) as value_count
             FROM tags t
             JOIN messages tm ON t.message_uuid = tm.uuid
             {tag_where}
             GROUP BY t.key, tm.session_id
         ),
         tag_sessions AS (
             SELECT DISTINCT t.key, t.value, tm.session_id, vc.value_count
             FROM tags t
             JOIN messages tm ON t.message_uuid = tm.uuid
             JOIN value_counts vc ON vc.key = t.key AND vc.session_id = tm.session_id
             {tag_where}
         ),
         session_costs AS (
             SELECT session_id, COALESCE(SUM(cost_cents), 0.0) as session_cost
             FROM messages
             {date_where} {role_filter}
             GROUP BY session_id
         ),
         tagged_results AS (
             SELECT ts.key, ts.value,
                    COUNT(DISTINCT ts.session_id) as session_count,
                    COALESCE(SUM(sc.session_cost / ts.value_count), 0.0) as total_cost_cents
             FROM tag_sessions ts
             JOIN session_costs sc ON sc.session_id = ts.session_id
             GROUP BY ts.key, ts.value
         )
         SELECT key, value, session_count, total_cost_cents FROM tagged_results
         {untagged_union}
         ORDER BY total_cost_cents DESC
         LIMIT ?{limit_idx}",
    );

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<TagCost> = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(TagCost {
                key: row.get(0)?,
                value: row.get(1)?,
                session_count: row.get(2)?,
                cost_cents: row.get(3)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(rows)
}

/// Model usage breakdown: tokens grouped by model name.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ModelUsage {
    pub model: String,
    pub provider: String,
    pub message_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cost_cents: f64,
}

/// Query model usage stats, optionally filtered by date range.
pub fn model_usage(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    limit: usize,
) -> Result<Vec<ModelUsage>> {
    let (where_clause, date_params) = date_filter(since, until, "WHERE");
    let mut param_values: Vec<String> = date_params;
    param_values.push(limit.to_string());
    let limit_idx = param_values.len();
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    // Single-query approach: COALESCE NULL/empty/template models into "(untagged)"
    let sql = format!(
        "SELECT CASE WHEN model IS NULL OR model = '' OR SUBSTR(model, 1, 1) = '<' THEN '(untagged)'
                     ELSE model END as m,
                COALESCE(provider, '') as p,
                COUNT(*) as cnt,
                COALESCE(SUM(input_tokens), 0) as total_input,
                COALESCE(SUM(output_tokens), 0) as total_output,
                COALESCE(SUM(cache_read_tokens), 0),
                COALESCE(SUM(cache_creation_tokens), 0),
                COALESCE(SUM(cost_cents), 0.0)
         FROM messages
         {} {} role = 'assistant'
         GROUP BY m, p
         ORDER BY 8 DESC
         LIMIT ?{limit_idx}",
        where_clause,
        if where_clause.is_empty() {
            "WHERE"
        } else {
            "AND"
        }
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<ModelUsage> = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(ModelUsage {
                model: row.get(0)?,
                provider: row.get(1)?,
                message_count: row.get(2)?,
                input_tokens: row.get(3)?,
                output_tokens: row.get(4)?,
                cache_read_tokens: row.get(5)?,
                cache_creation_tokens: row.get(6)?,
                cost_cents: row.get(7)?,
            })
        })?
        .filter_map(|r| match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("skipping row: {e}");
                None
            }
        })
        .collect();

    Ok(rows)
}

/// Compact stats for the status line display.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StatuslineStats {
    pub today_cost: f64,
    pub week_cost: f64,
    pub month_cost: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_provider: Option<String>,
}

/// Parameters for requesting extra statusline data.
#[derive(Debug, Default, Clone, serde::Deserialize)]
pub struct StatuslineParams {
    pub session_id: Option<String>,
    pub branch: Option<String>,
    pub project_dir: Option<String>,
}

/// Compute cost stats for today/week/month, suitable for the CLI status line.
/// Optionally computes session/branch/project costs when params are provided.
pub fn statusline_stats(
    conn: &Connection,
    today: &str,
    week_start: &str,
    month_start: &str,
    params: &StatuslineParams,
) -> Result<StatuslineStats> {
    fn cost_since(conn: &Connection, since: &str) -> f64 {
        conn.query_row(
            "SELECT COALESCE(SUM(cost_cents), 0.0) FROM messages WHERE timestamp >= ?1 AND role = 'assistant'",
            [since],
            |r| r.get::<_, f64>(0),
        )
        .unwrap_or(0.0)
            / 100.0
    }

    let today_cost = cost_since(conn, today);
    let week_cost = cost_since(conn, week_start);
    let month_cost = cost_since(conn, month_start);

    // Session cost: total cost for a specific session
    let session_cost = params.session_id.as_ref().map(|sid| {
        conn.query_row(
            "SELECT COALESCE(SUM(cost_cents), 0.0) FROM messages WHERE session_id = ?1 AND role = 'assistant'",
            [sid],
            |r| r.get::<_, f64>(0),
        )
        .unwrap_or(0.0)
            / 100.0
    });

    // Branch cost: total cost for messages on a specific branch
    let branch_cost = params.branch.as_ref().map(|branch| {
        conn.query_row(
            "SELECT COALESCE(SUM(m.cost_cents), 0.0) \
             FROM messages m \
             WHERE m.git_branch = ?1 AND m.role = 'assistant'",
            [branch],
            |r| r.get::<_, f64>(0),
        )
        .unwrap_or(0.0)
            / 100.0
    });

    // Project cost: total cost for messages in a specific directory
    let project_cost = params.project_dir.as_ref().map(|dir| {
        conn.query_row(
            "SELECT COALESCE(SUM(cost_cents), 0.0) FROM messages WHERE cwd = ?1 AND role = 'assistant'",
            [dir],
            |r| r.get::<_, f64>(0),
        )
        .unwrap_or(0.0)
            / 100.0
    });

    // Active provider: most recent provider used today
    let active_provider: Option<String> = conn
        .query_row(
            "SELECT provider FROM messages \
             WHERE timestamp >= ?1 ORDER BY timestamp DESC LIMIT 1",
            [today],
            |r| r.get(0),
        )
        .ok();

    Ok(StatuslineStats {
        today_cost,
        week_cost,
        month_cost,
        session_cost,
        branch_cost,
        project_cost,
        active_provider,
    })
}

/// Per-provider aggregate stats for the /analytics/providers endpoint.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProviderStats {
    pub provider: String,
    pub display_name: String,
    pub message_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub estimated_cost: f64,
    pub total_cost_cents: f64,
}

/// Query per-provider aggregate stats.
pub fn provider_stats(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Vec<ProviderStats>> {
    let (where_clause, date_params) = date_filter(since, until, "WHERE");
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = date_params
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    let role_filter = if where_clause.is_empty() {
        "WHERE role = 'assistant'"
    } else {
        "AND role = 'assistant'"
    };
    let sql = format!(
        "SELECT provider as p,
                COUNT(*) as msgs,
                COALESCE(SUM(input_tokens), 0),
                COALESCE(SUM(output_tokens), 0),
                COALESCE(SUM(cache_creation_tokens), 0),
                COALESCE(SUM(cache_read_tokens), 0),
                COALESCE(SUM(cost_cents), 0.0)
         FROM messages {} {}
         GROUP BY p ORDER BY msgs DESC",
        where_clause, role_filter
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, u64>(2)?,
                row.get::<_, u64>(3)?,
                row.get::<_, u64>(4)?,
                row.get::<_, u64>(5)?,
                row.get::<_, f64>(6)?,
            ))
        })?
        .filter_map(|r| match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("skipping row: {e}");
                None
            }
        })
        .collect::<Vec<_>>();

    let providers = crate::provider::all_providers();
    let mut result = Vec::new();

    for (prov, messages, input, output, cache_create, cache_read, sum_cost_cents) in rows {
        let display_name = providers
            .iter()
            .find(|p| p.name() == prov)
            .map(|p| p.display_name().to_string())
            .unwrap_or_else(|| prov.clone());

        // sum_cost_cents is in cents; estimated_cost is in dollars (rounded to nearest cent).
        let estimated_cost = sum_cost_cents.round() / 100.0;

        result.push(ProviderStats {
            provider: prov,
            display_name,
            message_count: messages,
            input_tokens: input,
            output_tokens: output,
            cache_creation_tokens: cache_create,
            cache_read_tokens: cache_read,
            estimated_cost,
            total_cost_cents: sum_cost_cents,
        });
    }

    Ok(result)
}

/// Build a parameterized filter clause that includes optional date range and provider.
fn date_provider_filter(
    since: Option<&str>,
    until: Option<&str>,
    provider: Option<&str>,
    keyword: &str,
) -> (String, Vec<String>) {
    let mut conditions = Vec::new();
    let mut param_values = Vec::new();
    if let Some(s) = since {
        param_values.push(s.to_string());
        conditions.push(format!("timestamp >= ?{}", param_values.len()));
    }
    if let Some(u) = until {
        param_values.push(u.to_string());
        conditions.push(format!("timestamp < ?{}", param_values.len()));
    }
    if let Some(p) = provider {
        param_values.push(p.to_string());
        conditions.push(format!("provider = ?{}", param_values.len()));
    }
    if conditions.is_empty() {
        (String::new(), param_values)
    } else {
        (
            format!("{} {}", keyword, conditions.join(" AND ")),
            param_values,
        )
    }
}

/// Query a usage summary, optionally filtered by date range and provider.
pub fn usage_summary_filtered(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    provider: Option<&str>,
) -> Result<UsageSummary> {
    let (where_clause, params) = date_provider_filter(since, until, provider, "WHERE");
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    // Single scan: all aggregates in one query
    let sql = format!(
        "SELECT COUNT(*),
                COALESCE(SUM(CASE WHEN role = 'user' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN role = 'assistant' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(input_tokens), 0),
                COALESCE(SUM(output_tokens), 0),
                COALESCE(SUM(cache_creation_tokens), 0),
                COALESCE(SUM(cache_read_tokens), 0)
         FROM messages {}",
        where_clause
    );
    let (
        total_messages,
        total_user_messages,
        total_assistant_messages,
        total_input,
        total_output,
        total_cache_create,
        total_cache_read,
    ): (u64, u64, u64, u64, u64, u64, u64) = conn.query_row(&sql, param_refs.as_slice(), |r| {
        Ok((
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
            r.get(6)?,
        ))
    })?;

    Ok(UsageSummary {
        total_messages,
        total_user_messages,
        total_assistant_messages,
        total_input_tokens: total_input,
        total_output_tokens: total_output,
        total_cache_creation_tokens: total_cache_create,
        total_cache_read_tokens: total_cache_read,
    })
}

/// Cache efficiency stats for a date range.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CacheEfficiency {
    pub total_input_tokens: u64,
    pub total_cache_read_tokens: u64,
    pub total_cache_creation_tokens: u64,
    pub cache_hit_rate: f64,
    pub cache_savings_cents: f64,
}

/// Query cache efficiency stats, optionally filtered by date range.
pub fn cache_efficiency(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<CacheEfficiency> {
    let (where_clause, date_params) = date_filter(since, until, "WHERE");
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = date_params
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    let role_filter = if where_clause.is_empty() {
        "WHERE role = 'assistant'"
    } else {
        "AND role = 'assistant'"
    };

    let sql = format!(
        "SELECT COALESCE(SUM(input_tokens), 0),
                COALESCE(SUM(cache_read_tokens), 0),
                COALESCE(SUM(cache_creation_tokens), 0),
                provider,
                COALESCE(model, 'unknown')
         FROM messages {where_clause} {role_filter}
         GROUP BY provider, COALESCE(model, 'unknown')",
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok((
                row.get::<_, u64>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, u64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect::<Vec<_>>();

    let mut total_input: u64 = 0;
    let mut total_cache_read: u64 = 0;
    let mut total_cache_creation: u64 = 0;
    let mut total_savings_cents: f64 = 0.0;

    for (input, cache_read, cache_creation, prov, model) in &rows {
        total_input += input;
        total_cache_read += cache_read;
        total_cache_creation += cache_creation;
        let pricing = match prov.as_str() {
            "cursor" => crate::providers::cursor::cursor_pricing_for_model(model),
            _ => crate::providers::claude_code::claude_pricing_for_model(model),
        };
        // Savings: what cache reads would have cost at full input price minus what they actually cost
        let savings = *cache_read as f64 * (pricing.input - pricing.cache_read) / 1_000_000.0;
        total_savings_cents += savings * 100.0;
    }

    let denominator = total_input + total_cache_read;
    let cache_hit_rate = if denominator > 0 {
        total_cache_read as f64 / denominator as f64
    } else {
        0.0
    };

    Ok(CacheEfficiency {
        total_input_tokens: total_input + total_cache_read,
        total_cache_read_tokens: total_cache_read,
        total_cache_creation_tokens: total_cache_creation,
        cache_hit_rate,
        cache_savings_cents: (total_savings_cents * 100.0).round() / 100.0,
    })
}

/// Session cost curve: average cost per message by session length bucket.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionCostBucket {
    pub bucket: String,
    pub session_count: u64,
    pub avg_messages: f64,
    pub avg_cost_per_message_cents: f64,
    pub total_cost_cents: f64,
}

/// Query session cost curve: average cost per message grouped by session length.
pub fn session_cost_curve(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Vec<SessionCostBucket>> {
    let mut conditions = vec!["role = 'assistant'".to_string()];
    let mut param_values: Vec<String> = Vec::new();
    if let Some(s) = since {
        param_values.push(s.to_string());
        conditions.push(format!("timestamp >= ?{}", param_values.len()));
    }
    if let Some(u) = until {
        param_values.push(u.to_string());
        conditions.push(format!("timestamp < ?{}", param_values.len()));
    }
    let where_clause = format!("WHERE {}", conditions.join(" AND "));
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    // First compute per-session stats, then bucket by message count
    let sql = format!(
        "WITH session_stats AS (
             SELECT session_id,
                    COUNT(*) as msg_count,
                    COALESCE(SUM(cost_cents), 0.0) as total_cost
             FROM messages
             {where_clause}
             AND session_id IS NOT NULL
             GROUP BY session_id
         )
         SELECT CASE
                    WHEN msg_count <= 5 THEN '1-5'
                    WHEN msg_count <= 15 THEN '6-15'
                    WHEN msg_count <= 30 THEN '16-30'
                    WHEN msg_count <= 60 THEN '31-60'
                    WHEN msg_count <= 100 THEN '61-100'
                    ELSE '100+'
                END as bucket,
                COUNT(*) as session_count,
                AVG(msg_count) as avg_messages,
                AVG(total_cost / msg_count) as avg_cost_per_msg,
                SUM(total_cost) as total_cost
         FROM session_stats
         GROUP BY bucket
         ORDER BY MIN(msg_count)",
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(SessionCostBucket {
                bucket: row.get(0)?,
                session_count: row.get(1)?,
                avg_messages: row.get(2)?,
                avg_cost_per_message_cents: row.get(3)?,
                total_cost_cents: row.get(4)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(rows)
}

/// Cost confidence distribution.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CostConfidenceStat {
    pub confidence: String,
    pub message_count: u64,
    pub cost_cents: f64,
}

/// Query cost breakdown by cost_confidence level.
pub fn cost_confidence_stats(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Vec<CostConfidenceStat>> {
    let (where_clause, date_params) = date_filter(since, until, "WHERE");
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = date_params
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    let role_filter = if where_clause.is_empty() {
        "WHERE role = 'assistant'"
    } else {
        "AND role = 'assistant'"
    };

    let sql = format!(
        "SELECT COALESCE(cost_confidence, 'estimated') as conf,
                COUNT(*) as cnt,
                COALESCE(SUM(cost_cents), 0.0) as cost
         FROM messages {where_clause} {role_filter}
         GROUP BY conf
         ORDER BY cost DESC",
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(CostConfidenceStat {
                confidence: row.get(0)?,
                message_count: row.get(1)?,
                cost_cents: row.get(2)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(rows)
}

/// Subagent vs main conversation cost breakdown.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SubagentCostStat {
    pub category: String,
    pub message_count: u64,
    pub cost_cents: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Query cost split between main conversation and subagent messages.
pub fn subagent_cost_stats(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Vec<SubagentCostStat>> {
    let (where_clause, date_params) = date_filter(since, until, "WHERE");
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = date_params
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    let role_filter = if where_clause.is_empty() {
        "WHERE role = 'assistant'"
    } else {
        "AND role = 'assistant'"
    };

    let sql = format!(
        "SELECT CASE WHEN parent_uuid IS NOT NULL THEN 'subagent' ELSE 'main' END as category,
                COUNT(*) as cnt,
                COALESCE(SUM(cost_cents), 0.0) as cost,
                COALESCE(SUM(input_tokens), 0) as inp,
                COALESCE(SUM(output_tokens), 0) as outp
         FROM messages {where_clause} {role_filter}
         GROUP BY category
         ORDER BY cost DESC",
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(SubagentCostStat {
                category: row.get(0)?,
                message_count: row.get(1)?,
                cost_cents: row.get(2)?,
                input_tokens: row.get(3)?,
                output_tokens: row.get(4)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(rows)
}

/// Session list entry for the Sessions page.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionListEntry {
    pub session_id: String,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub duration_ms: Option<i64>,
    pub message_count: u64,
    pub cost_cents: f64,
    pub model: Option<String>,
    pub provider: String,
    pub repo_id: Option<String>,
    pub git_branch: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Paginated session list result.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PaginatedSessions {
    pub sessions: Vec<SessionListEntry>,
    pub total_count: u64,
}

/// Parameters for session list queries.
pub struct SessionListParams<'a> {
    pub since: Option<&'a str>,
    pub until: Option<&'a str>,
    pub search: Option<&'a str>,
    pub sort_by: Option<&'a str>,
    pub sort_asc: bool,
    pub limit: usize,
    pub offset: usize,
}

/// Query sessions with cost aggregated from messages.
pub fn session_list(conn: &Connection, p: &SessionListParams) -> Result<PaginatedSessions> {
    let mut conditions = vec!["m.role = 'assistant'".to_string()];
    let mut param_values: Vec<String> = Vec::new();
    if let Some(s) = p.since {
        param_values.push(s.to_string());
        conditions.push(format!("m.timestamp >= ?{}", param_values.len()));
    }
    if let Some(u) = p.until {
        param_values.push(u.to_string());
        conditions.push(format!("m.timestamp < ?{}", param_values.len()));
    }
    if let Some(q) = p.search
        && !q.is_empty()
    {
        param_values.push(format!("%{q}%"));
        let idx = param_values.len();
        conditions.push(format!(
            "(m.model LIKE ?{idx} OR m.repo_id LIKE ?{idx} OR m.provider LIKE ?{idx} OR COALESCE(m.git_branch, s.git_branch) LIKE ?{idx})"
        ));
    }
    let where_clause = format!("WHERE {}", conditions.join(" AND "));

    // Count total matching sessions using only the filter params (no limit/offset)
    let count_param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();
    let count_sql = format!(
        "WITH session_agg AS (
             SELECT m.session_id
             FROM messages m
             LEFT JOIN sessions s ON s.conversation_id = m.session_id
             {where_clause}
             AND m.session_id IS NOT NULL
             GROUP BY m.session_id
         )
         SELECT COUNT(*) FROM session_agg"
    );
    let total_count: u64 =
        conn.query_row(&count_sql, count_param_refs.as_slice(), |row| row.get(0))?;

    param_values.push(p.limit.to_string());
    let limit_idx = param_values.len();
    param_values.push(p.offset.to_string());
    let offset_idx = param_values.len();

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    let dir = if p.sort_asc { "ASC" } else { "DESC" };
    let order_expr = match p.sort_by.unwrap_or("started_at") {
        "started_at" => format!("sa.started_at {dir}"),
        "duration" => {
            // duration_ms from hooks, fallback to computed from timestamps
            let col = "COALESCE(sa.duration_ms, (julianday(sa.ended_at) - julianday(sa.started_at)) * 86400000)";
            if p.sort_asc {
                format!("({col} IS NULL) ASC, {col} {dir}")
            } else {
                format!("{col} {dir}")
            }
        }
        "model" => format!("sa.models_by_cost {dir}"),
        "provider" => format!("sa.provider {dir}"),
        "repo_id" => {
            if p.sort_asc {
                format!("(sa.repo_id IS NULL OR sa.repo_id = '') ASC, sa.repo_id {dir}")
            } else {
                format!("sa.repo_id {dir}")
            }
        }
        "git_branch" | "branch" => {
            if p.sort_asc {
                format!("(sa.git_branch IS NULL OR sa.git_branch = '') ASC, sa.git_branch {dir}")
            } else {
                format!("sa.git_branch {dir}")
            }
        }
        "tokens" => format!("(sa.inp + sa.outp) {dir}"),
        _ => format!("sa.cost {dir}"),
    };

    let sql = format!(
        "WITH session_agg AS (
             SELECT m.session_id,
                    MIN(m.timestamp) as started_at,
                    MAX(m.timestamp) as ended_at,
                    COUNT(*) as msg_count,
                    COALESCE(SUM(m.cost_cents), 0.0) as cost,
                    (SELECT GROUP_CONCAT(sub.model, ',') FROM (
                         SELECT m2.model FROM messages m2
                         WHERE m2.session_id = m.session_id AND m2.role = 'assistant'
                           AND m2.model IS NOT NULL AND m2.model != '' AND SUBSTR(m2.model, 1, 1) != '<'
                         GROUP BY m2.model ORDER BY SUM(m2.cost_cents) DESC
                     ) sub) as models_by_cost,
                    COALESCE(MAX(m.provider), 'claude_code') as provider,
                    COALESCE(MAX(m.repo_id), MAX(s.repo_id)) as repo_id,
                    COALESCE(MAX(m.git_branch), MAX(s.git_branch)) as git_branch,
                    COALESCE(SUM(m.input_tokens), 0) as inp,
                    COALESCE(SUM(m.output_tokens), 0) as outp,
                    COALESCE(s.duration_ms,
                        CAST((julianday(MAX(m.timestamp)) - julianday(MIN(m.timestamp))) * 86400000 AS INTEGER)
                    ) as duration_ms
             FROM messages m
             LEFT JOIN sessions s ON s.conversation_id = m.session_id
             {where_clause}
             AND m.session_id IS NOT NULL
             GROUP BY m.session_id
         )
         SELECT COUNT(*) OVER() as total,
                sa.session_id, sa.started_at, sa.ended_at, sa.duration_ms,
                sa.msg_count, sa.cost, sa.models_by_cost, sa.provider, sa.repo_id, sa.git_branch,
                sa.inp, sa.outp
         FROM session_agg sa
         ORDER BY {order_expr}
         LIMIT ?{limit_idx} OFFSET ?{offset_idx}",
    );

    let mut stmt = conn.prepare(&sql)?;
    let sessions: Vec<SessionListEntry> = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(SessionListEntry {
                session_id: row.get(1)?,
                started_at: row.get(2)?,
                ended_at: row.get(3)?,
                duration_ms: row.get(4)?,
                message_count: row.get(5)?,
                cost_cents: row.get(6)?,
                model: row.get(7)?,
                provider: row.get::<_, String>(8)?,
                repo_id: row.get(9)?,
                git_branch: row.get(10)?,
                input_tokens: row.get(11)?,
                output_tokens: row.get(12)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(PaginatedSessions {
        sessions,
        total_count,
    })
}

/// Messages within a specific session for drill-down.
/// Get distinct tags for a session.
pub fn session_tags(conn: &Connection, session_id: &str) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT t.key, t.value
         FROM tags t
         JOIN messages m ON t.message_uuid = m.uuid
         WHERE m.session_id = ?1
         ORDER BY t.key, t.value",
    )?;
    let rows = stmt
        .query_map(params![session_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

/// Messages within a specific session for drill-down.
pub fn session_messages(conn: &Connection, session_id: &str) -> Result<Vec<MessageRow>> {
    let mut stmt = conn.prepare(
        "SELECT uuid, timestamp, role, model,
                COALESCE(provider, 'claude_code'),
                repo_id,
                input_tokens, output_tokens,
                cache_creation_tokens, cache_read_tokens,
                COALESCE(cost_cents, 0.0),
                COALESCE(cost_confidence, 'estimated'),
                git_branch
         FROM messages
         WHERE session_id = ?1 AND role = 'assistant'
         ORDER BY timestamp ASC",
    )?;

    let rows = stmt
        .query_map(params![session_id], |row| {
            Ok(MessageRow {
                uuid: row.get(0)?,
                timestamp: row.get(1)?,
                role: row.get(2)?,
                model: row.get(3)?,
                provider: row.get(4)?,
                repo_id: row.get(5)?,
                input_tokens: row.get(6)?,
                output_tokens: row.get(7)?,
                cache_creation_tokens: row.get(8)?,
                cache_read_tokens: row.get(9)?,
                cost_cents: row.get(10)?,
                cost_confidence: row.get(11)?,
                git_branch: row.get(12)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache_stats(
        conn: &Connection,
        since: Option<&str>,
        until: Option<&str>,
    ) -> Result<CacheEfficiency> {
        cache_efficiency(conn, since, until)
    }

    fn test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .unwrap();
        crate::migration::migrate(&conn).unwrap();
        conn
    }

    #[test]
    fn schema_creates_tables() {
        let conn = test_db();
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| match r {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!("skipping row: {e}");
                    None
                }
            })
            .collect();
        assert!(tables.contains(&"sessions".to_string()));
        assert!(tables.contains(&"hook_events".to_string()));
        assert!(tables.contains(&"messages".to_string()));
        assert!(tables.contains(&"sync_state".to_string()));
    }

    #[test]
    fn ingest_and_query() {
        let mut conn = test_db();
        let msgs = vec![
            ParsedMessage {
                uuid: "u1".to_string(),
                session_id: Some("s1".to_string()),
                timestamp: "2026-03-14T18:13:42Z".parse().unwrap(),
                cwd: Some("/tmp/proj".to_string()),
                role: "user".to_string(),
                model: None,
                input_tokens: 0,
                output_tokens: 0,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                git_branch: Some("main".to_string()),
                repo_id: None,
                provider: "claude_code".to_string(),
                cost_cents: None,
                session_title: None,
                parent_uuid: None,
                user_name: None,
                machine_name: None,
                cost_confidence: "exact".to_string(),
                request_id: None,
                speed: None,
                cache_creation_1h_tokens: 0,
                web_search_requests: 0,
            },
            ParsedMessage {
                uuid: "a1".to_string(),
                session_id: Some("s1".to_string()),
                timestamp: "2026-03-14T18:14:00Z".parse().unwrap(),
                cwd: Some("/tmp/proj".to_string()),
                role: "assistant".to_string(),
                model: Some("claude-opus-4-6".to_string()),
                input_tokens: 100,
                output_tokens: 50,
                cache_creation_tokens: 200,
                cache_read_tokens: 300,
                git_branch: None,
                repo_id: None,
                provider: "claude_code".to_string(),
                cost_cents: None,
                session_title: None,
                parent_uuid: None,
                user_name: None,
                machine_name: None,
                cost_confidence: "exact".to_string(),
                request_id: None,
                speed: None,
                cache_creation_1h_tokens: 0,
                web_search_requests: 0,
            },
        ];

        let count = ingest_messages(&mut conn, &msgs, None).unwrap();
        assert_eq!(count, 2);

        // Duplicate insert should be skipped.
        let count2 = ingest_messages(&mut conn, &msgs, None).unwrap();
        assert_eq!(count2, 0);

        let summary = usage_summary(&conn, None, None).unwrap();
        assert_eq!(summary.total_messages, 2);
        assert_eq!(summary.total_user_messages, 1);
        assert_eq!(summary.total_assistant_messages, 1);
        assert_eq!(summary.total_input_tokens, 100);
        assert_eq!(summary.total_output_tokens, 50);
    }

    #[test]
    fn cost_cents_baked_at_ingest() {
        use crate::pipeline::Enricher;
        use crate::pipeline::enrichers::CostEnricher;

        let mut conn = test_db();
        let mut msg = ParsedMessage {
            uuid: "cost-test-1".to_string(),
            session_id: Some("s1".to_string()),
            timestamp: "2026-03-14T10:00:00Z".parse().unwrap(),
            cwd: None,
            role: "assistant".to_string(),
            model: Some("claude-opus-4-6".to_string()),
            input_tokens: 1_000_000,
            output_tokens: 100_000,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            git_branch: None,
            repo_id: None,
            provider: "claude_code".to_string(),
            cost_cents: None,
            session_title: None,
            parent_uuid: None,
            user_name: None,
            machine_name: None,
            cost_confidence: "exact".to_string(),
            request_id: None,
            speed: None,
            cache_creation_1h_tokens: 0,
            web_search_requests: 0,
        };
        // CostEnricher is the single source of truth for cost_cents
        CostEnricher.enrich(&mut msg);
        ingest_messages(&mut conn, &[msg], None).unwrap();

        // Verify cost_cents was baked in: 1M input * $5/M + 100K output * $25/M = $5 + $2.50 = $7.50 = 750 cents
        let cost_cents: f64 = conn
            .query_row(
                "SELECT cost_cents FROM messages WHERE uuid = 'cost-test-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            (cost_cents - 750.0).abs() < 1.0,
            "expected ~750 cents, got {cost_cents}"
        );
    }

    #[test]
    fn sync_offset_round_trip() {
        let conn = test_db();
        assert_eq!(get_sync_offset(&conn, "/tmp/test.jsonl").unwrap(), 0);
        set_sync_offset(&conn, "/tmp/test.jsonl", 1234).unwrap();
        assert_eq!(get_sync_offset(&conn, "/tmp/test.jsonl").unwrap(), 1234);
        set_sync_offset(&conn, "/tmp/test.jsonl", 5678).unwrap();
        assert_eq!(get_sync_offset(&conn, "/tmp/test.jsonl").unwrap(), 5678);
    }

    #[test]
    fn last_seen_derived_from_messages() {
        let mut conn = test_db();
        let msgs = vec![
            ParsedMessage {
                uuid: "m1".to_string(),
                session_id: Some("s1".to_string()),
                timestamp: "2026-03-14T10:00:00Z".parse().unwrap(),
                cwd: Some("/tmp".to_string()),
                role: "user".to_string(),
                model: None,
                input_tokens: 0,
                output_tokens: 0,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                git_branch: Some("main".to_string()),
                repo_id: None,
                provider: "claude_code".to_string(),
                cost_cents: None,
                session_title: None,
                parent_uuid: None,
                user_name: None,
                machine_name: None,
                cost_confidence: "exact".to_string(),
                request_id: None,
                speed: None,
                cache_creation_1h_tokens: 0,
                web_search_requests: 0,
            },
            ParsedMessage {
                uuid: "m2".to_string(),
                session_id: Some("s1".to_string()),
                timestamp: "2026-03-14T12:00:00Z".parse().unwrap(),
                cwd: Some("/tmp".to_string()),
                role: "user".to_string(),
                model: None,
                input_tokens: 0,
                output_tokens: 0,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                git_branch: None,
                repo_id: None,
                provider: "claude_code".to_string(),
                cost_cents: None,
                session_title: None,
                parent_uuid: None,
                user_name: None,
                machine_name: None,
                cost_confidence: "exact".to_string(),
                request_id: None,
                speed: None,
                cache_creation_1h_tokens: 0,
                web_search_requests: 0,
            },
        ];
        ingest_messages(&mut conn, &msgs, None).unwrap();

        let last_seen: String = conn
            .query_row(
                "SELECT MAX(timestamp) FROM messages WHERE session_id = 's1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(last_seen.contains("12:00:00"));
    }

    fn sample_messages() -> Vec<ParsedMessage> {
        vec![
            ParsedMessage {
                uuid: "u1".to_string(),
                session_id: Some("sess-abc".to_string()),
                timestamp: "2026-03-14T18:13:42Z".parse().unwrap(),
                cwd: Some("/home/user/project-a".to_string()),
                role: "user".to_string(),
                model: None,
                input_tokens: 0,
                output_tokens: 0,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                git_branch: Some("main".to_string()),
                repo_id: None,
                provider: "claude_code".to_string(),
                cost_cents: None,
                session_title: None,

                parent_uuid: None,
                user_name: None,
                machine_name: None,
                cost_confidence: "exact".to_string(),
                request_id: None,
                speed: None,
                cache_creation_1h_tokens: 0,
                web_search_requests: 0,
            },
            ParsedMessage {
                uuid: "a1".to_string(),
                session_id: Some("sess-abc".to_string()),
                timestamp: "2026-03-14T18:14:00Z".parse().unwrap(),
                cwd: Some("/home/user/project-a".to_string()),
                role: "assistant".to_string(),
                model: Some("claude-opus-4-6".to_string()),
                input_tokens: 100,
                output_tokens: 50,
                cache_creation_tokens: 200,
                cache_read_tokens: 300,

                git_branch: None,
                repo_id: None,
                provider: "claude_code".to_string(),
                cost_cents: Some(2.0), // Pre-calculated by CostEnricher in production
                session_title: None,

                parent_uuid: None,
                user_name: None,
                machine_name: None,
                cost_confidence: "exact".to_string(),
                request_id: None,
                speed: None,
                cache_creation_1h_tokens: 0,
                web_search_requests: 0,
            },
            ParsedMessage {
                uuid: "u2".to_string(),
                session_id: Some("sess-def".to_string()),
                timestamp: "2026-03-14T19:00:00Z".parse().unwrap(),
                cwd: Some("/home/user/project-b".to_string()),
                role: "user".to_string(),
                model: None,
                input_tokens: 0,
                output_tokens: 0,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                git_branch: None,
                repo_id: None,
                provider: "claude_code".to_string(),
                cost_cents: None,
                session_title: None,

                parent_uuid: None,
                user_name: None,
                machine_name: None,
                cost_confidence: "exact".to_string(),
                request_id: None,
                speed: None,
                cache_creation_1h_tokens: 0,
                web_search_requests: 0,
            },
        ]
    }

    #[test]
    fn message_list_returns_messages() {
        let mut conn = test_db();
        ingest_messages(&mut conn, &sample_messages(), None).unwrap();

        let result = message_list(
            &conn,
            &MessageListParams {
                since: None,
                until: None,
                search: None,
                sort_by: None,
                sort_asc: false,
                limit: 50,
                offset: 0,
            },
        )
        .unwrap();
        // Only assistant messages are returned
        assert_eq!(result.messages.len(), 1);
        assert_eq!(result.total_count, 1);
        assert_eq!(result.messages[0].input_tokens, 100);
    }

    #[test]
    fn repo_usage_groups_by_repo_id() {
        let mut conn = test_db();
        let mut msgs = sample_messages();
        // Assign repo_ids — only assistant messages count for cost aggregation
        msgs[0].repo_id = Some("project-a".to_string());
        msgs[1].repo_id = Some("project-a".to_string());
        msgs[2].repo_id = Some("project-b".to_string());
        // Make project-b's message an assistant with tokens so it appears in results
        msgs[2].role = "assistant".to_string();
        msgs[2].model = Some("claude-opus-4-6".to_string());
        msgs[2].input_tokens = 50;
        msgs[2].cost_cents = Some(0.5);
        ingest_messages(&mut conn, &msgs, None).unwrap();

        let repos = repo_usage(&conn, None, None, 10).unwrap();
        assert_eq!(repos.len(), 2);
        // project-a has more cost, project-b has some.
        assert_eq!(repos[0].repo_id, "project-a");
        assert_eq!(repos[0].message_count, 1); // only assistant msg
        assert_eq!(repos[1].repo_id, "project-b");
        assert_eq!(repos[1].message_count, 1);
    }

    fn messages_with_cache_patterns() -> Vec<ParsedMessage> {
        vec![
            ParsedMessage {
                uuid: "t1".to_string(),
                session_id: Some("s1".to_string()),
                timestamp: "2026-03-14T10:00:00Z".parse().unwrap(),
                cwd: Some("/tmp/proj".to_string()),
                role: "assistant".to_string(),
                model: Some("claude-opus-4-6".to_string()),
                input_tokens: 500,
                output_tokens: 100,
                cache_creation_tokens: 0,
                cache_read_tokens: 200,
                git_branch: None,
                repo_id: None,
                provider: "claude_code".to_string(),
                cost_cents: None,
                session_title: None,

                parent_uuid: None,
                user_name: None,
                machine_name: None,
                cost_confidence: "exact".to_string(),
                request_id: None,
                speed: None,
                cache_creation_1h_tokens: 0,
                web_search_requests: 0,
            },
            ParsedMessage {
                uuid: "t2".to_string(),
                session_id: Some("s1".to_string()),
                timestamp: "2026-03-14T10:01:00Z".parse().unwrap(),
                cwd: Some("/tmp/proj".to_string()),
                role: "assistant".to_string(),
                model: Some("claude-opus-4-6".to_string()),
                input_tokens: 300,
                output_tokens: 200,
                cache_creation_tokens: 100,
                cache_read_tokens: 150,
                git_branch: None,
                repo_id: None,
                provider: "claude_code".to_string(),
                cost_cents: None,
                session_title: None,

                parent_uuid: None,
                user_name: None,
                machine_name: None,
                cost_confidence: "exact".to_string(),
                request_id: None,
                speed: None,
                cache_creation_1h_tokens: 0,
                web_search_requests: 0,
            },
            // Token-heavy session: input >> output
            ParsedMessage {
                uuid: "t3".to_string(),
                session_id: Some("s2".to_string()),
                timestamp: "2026-03-14T11:00:00Z".parse().unwrap(),
                cwd: Some("/tmp/big".to_string()),
                role: "assistant".to_string(),
                model: Some("claude-opus-4-6".to_string()),
                input_tokens: 50000,
                output_tokens: 500,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                git_branch: None,
                repo_id: None,
                provider: "claude_code".to_string(),
                cost_cents: None,
                session_title: None,

                parent_uuid: None,
                user_name: None,
                machine_name: None,
                cost_confidence: "exact".to_string(),
                request_id: None,
                speed: None,
                cache_creation_1h_tokens: 0,
                web_search_requests: 0,
            },
        ]
    }

    #[test]
    fn cache_stats_computes_hit_rate() {
        let mut conn = test_db();
        ingest_messages(&mut conn, &messages_with_cache_patterns(), None).unwrap();

        let cs = cache_stats(&conn, None, None).unwrap();
        // total_input = (500+200) + (300+150) + (50000+0) = 51150
        // cache_creation_tokens excluded from denominator — they are new cache writes, not hits/misses
        assert_eq!(cs.total_input_tokens, 51150);
        // cache_read = 200 + 150 + 0 = 350
        assert_eq!(cs.total_cache_read_tokens, 350);
        assert!((cs.cache_hit_rate - 350.0 / 51150.0).abs() < 0.001);
    }

    #[test]
    fn statusline_stats_empty_db() {
        let conn = test_db();
        let params = StatuslineParams::default();
        let stats =
            statusline_stats(&conn, "2026-03-21", "2026-03-17", "2026-03-01", &params).unwrap();
        assert_eq!(stats.today_cost, 0.0);
        assert_eq!(stats.week_cost, 0.0);
        assert_eq!(stats.month_cost, 0.0);
        assert!(stats.session_cost.is_none());
        assert!(stats.branch_cost.is_none());
        assert!(stats.project_cost.is_none());
    }

    #[test]
    fn statusline_stats_with_data() {
        let mut conn = test_db();
        ingest_messages(&mut conn, &sample_messages(), None).unwrap();
        let params = StatuslineParams::default();
        // sample_messages have timestamps on 2026-03-14
        let stats =
            statusline_stats(&conn, "2026-03-14", "2026-03-10", "2026-03-01", &params).unwrap();
        assert!(stats.month_cost > 0.0);
    }

    #[test]
    fn statusline_stats_with_session_filter() {
        let mut conn = test_db();
        ingest_messages(&mut conn, &sample_messages(), None).unwrap();
        let params = StatuslineParams {
            session_id: Some("sess-1".to_string()),
            ..Default::default()
        };
        let stats =
            statusline_stats(&conn, "2026-03-14", "2026-03-10", "2026-03-01", &params).unwrap();
        assert!(stats.session_cost.is_some());
        assert!(stats.session_cost.unwrap() >= 0.0);
    }

    #[test]
    fn statusline_stats_with_branch_filter() {
        let mut conn = test_db();
        ingest_messages(&mut conn, &sample_messages(), None).unwrap();
        let params = StatuslineParams {
            branch: Some("main".to_string()),
            ..Default::default()
        };
        let stats =
            statusline_stats(&conn, "2026-03-14", "2026-03-10", "2026-03-01", &params).unwrap();
        assert!(stats.branch_cost.is_some());
    }

    #[test]
    fn multi_provider_ingest_and_query() {
        let mut conn = test_db();

        // Claude Code messages
        let claude_msgs = vec![
            ParsedMessage {
                uuid: "cc-u1".to_string(),
                session_id: Some("cc-sess-1".to_string()),
                timestamp: "2026-03-20T10:00:00Z".parse().unwrap(),
                cwd: Some("/proj/a".to_string()),
                role: "user".to_string(),
                model: None,
                input_tokens: 0,
                output_tokens: 0,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                git_branch: None,
                repo_id: None,
                provider: "claude_code".to_string(),
                cost_cents: None,
                session_title: None,

                parent_uuid: None,
                user_name: None,
                machine_name: None,
                cost_confidence: "exact".to_string(),
                request_id: None,
                speed: None,
                cache_creation_1h_tokens: 0,
                web_search_requests: 0,
            },
            ParsedMessage {
                uuid: "cc-a1".to_string(),
                session_id: Some("cc-sess-1".to_string()),
                timestamp: "2026-03-20T10:01:00Z".parse().unwrap(),
                cwd: Some("/proj/a".to_string()),
                role: "assistant".to_string(),
                model: Some("claude-opus-4-6".to_string()),
                input_tokens: 1000,
                output_tokens: 500,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                git_branch: None,
                repo_id: None,
                provider: "claude_code".to_string(),
                cost_cents: Some(1.75), // Pre-calculated by CostEnricher in production
                session_title: None,

                parent_uuid: None,
                user_name: None,
                machine_name: None,
                cost_confidence: "exact".to_string(),
                request_id: None,
                speed: None,
                cache_creation_1h_tokens: 0,
                web_search_requests: 0,
            },
        ];

        // Cursor messages
        let cursor_msgs = vec![
            ParsedMessage {
                uuid: "cu-u1".to_string(),
                session_id: Some("cu-sess-1".to_string()),
                timestamp: "2026-03-20T11:00:00Z".parse().unwrap(),
                cwd: Some("/proj/b".to_string()),
                role: "user".to_string(),
                model: None,
                input_tokens: 0,
                output_tokens: 0,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                git_branch: None,
                repo_id: None,
                provider: "cursor".to_string(),
                cost_cents: None,
                session_title: None,

                parent_uuid: None,
                user_name: None,
                machine_name: None,
                cost_confidence: "exact".to_string(),
                request_id: None,
                speed: None,
                cache_creation_1h_tokens: 0,
                web_search_requests: 0,
            },
            ParsedMessage {
                uuid: "cu-a1".to_string(),
                session_id: Some("cu-sess-1".to_string()),
                timestamp: "2026-03-20T11:01:00Z".parse().unwrap(),
                cwd: Some("/proj/b".to_string()),
                role: "assistant".to_string(),
                model: Some("gpt-4o".to_string()),
                input_tokens: 2000,
                output_tokens: 800,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                git_branch: None,
                repo_id: None,
                provider: "cursor".to_string(),
                cost_cents: Some(0.62), // Pre-calculated by CostEnricher in production
                session_title: None,

                parent_uuid: None,
                user_name: None,
                machine_name: None,
                cost_confidence: "exact".to_string(),
                request_id: None,
                speed: None,
                cache_creation_1h_tokens: 0,
                web_search_requests: 0,
            },
        ];

        ingest_messages(&mut conn, &claude_msgs, None).unwrap();
        ingest_messages(&mut conn, &cursor_msgs, None).unwrap();

        // All providers: should see 4 messages
        let all = usage_summary(&conn, None, None).unwrap();
        assert_eq!(all.total_messages, 4);
        assert_eq!(all.total_input_tokens, 3000); // 1000 + 2000
        assert_eq!(all.total_output_tokens, 1300); // 500 + 800

        // Filter by claude_code: 2 messages
        let cc = usage_summary_filtered(&conn, None, None, Some("claude_code")).unwrap();
        assert_eq!(cc.total_messages, 2);
        assert_eq!(cc.total_input_tokens, 1000);
        assert_eq!(cc.total_output_tokens, 500);

        // Filter by cursor: 2 messages
        let cu = usage_summary_filtered(&conn, None, None, Some("cursor")).unwrap();
        assert_eq!(cu.total_messages, 2);
        assert_eq!(cu.total_input_tokens, 2000);
        assert_eq!(cu.total_output_tokens, 800);

        // Provider stats (only assistant messages counted after role pre-filter)
        let pstats = provider_stats(&conn, None, None).unwrap();
        assert_eq!(pstats.len(), 2);
        let cc_stats = pstats.iter().find(|p| p.provider == "claude_code").unwrap();
        let cu_stats = pstats.iter().find(|p| p.provider == "cursor").unwrap();
        assert_eq!(cc_stats.message_count, 1);
        assert_eq!(cu_stats.message_count, 1);

        // Claude Code is registered, so it gets proper display name and cost.
        assert_eq!(cc_stats.display_name, "Claude Code");
        assert!(cc_stats.estimated_cost > 0.0);
    }

    /// Simulate the cross-parse dedup bug: a multi-content-block API response where
    /// intermediate entries are ingested in one parse call and the final entry in the
    /// next. Without request_id dedup, both get inserted — double-counting cache tokens.
    #[test]
    fn cross_parse_dedup_by_request_id() {
        let mut conn = test_db();

        // First parse: intermediate entry with partial output but full cache tokens
        let intermediate = ParsedMessage {
            uuid: "a1".to_string(),
            session_id: Some("s1".to_string()),
            timestamp: "2026-03-25T00:00:01.000Z".parse().unwrap(),
            cwd: Some("/tmp/proj".to_string()),
            role: "assistant".to_string(),
            model: Some("claude-sonnet-4-6".to_string()),
            input_tokens: 3,
            output_tokens: 10, // intermediate: partial output
            cache_creation_tokens: 21559,
            cache_read_tokens: 50000,
            git_branch: None,
            repo_id: None,
            provider: "claude_code".to_string(),
            cost_cents: Some(1.5),
            session_title: None,
            parent_uuid: None,
            user_name: None,
            machine_name: None,
            cost_confidence: "estimated".to_string(),
            request_id: Some("msg_01ABC".to_string()), // same request_id
            speed: None,
            cache_creation_1h_tokens: 0,
            web_search_requests: 0,
        };
        ingest_messages(&mut conn, &[intermediate], None).unwrap();

        // Verify first message is inserted
        let count: i64 = conn
            .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);

        // Second parse: final entry with same request_id but higher output_tokens
        let final_entry = ParsedMessage {
            uuid: "a3".to_string(),
            session_id: Some("s1".to_string()),
            timestamp: "2026-03-25T00:00:01.500Z".parse().unwrap(),
            cwd: Some("/tmp/proj".to_string()),
            role: "assistant".to_string(),
            model: Some("claude-sonnet-4-6".to_string()),
            input_tokens: 3,
            output_tokens: 425, // final: full output
            cache_creation_tokens: 21559,
            cache_read_tokens: 50000,
            git_branch: None,
            repo_id: None,
            provider: "claude_code".to_string(),
            cost_cents: Some(5.0),
            session_title: None,
            parent_uuid: None,
            user_name: None,
            machine_name: None,
            cost_confidence: "estimated".to_string(),
            request_id: Some("msg_01ABC".to_string()), // same request_id
            speed: None,
            cache_creation_1h_tokens: 0,
            web_search_requests: 0,
        };
        ingest_messages(&mut conn, &[final_entry], None).unwrap();

        // Should still have only 1 message (deduped by request_id)
        let count: i64 = conn
            .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "should dedup by request_id, not insert both");

        // The surviving row should have the higher output_tokens
        let (output, cache_read): (i64, i64) = conn
            .query_row(
                "SELECT output_tokens, cache_read_tokens FROM messages",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(output, 425, "should keep higher output_tokens");
        assert_eq!(cache_read, 50000, "cache_read should not be doubled");
    }

    /// When an intermediate entry arrives AFTER the final entry (re-ordered parse),
    /// the existing higher-output row should be kept.
    #[test]
    fn cross_parse_dedup_keeps_higher_output() {
        let mut conn = test_db();

        // Insert final entry first
        let final_entry = ParsedMessage {
            uuid: "a3".to_string(),
            session_id: Some("s1".to_string()),
            timestamp: "2026-03-25T00:00:01.000Z".parse().unwrap(),
            cwd: None,
            role: "assistant".to_string(),
            model: Some("claude-sonnet-4-6".to_string()),
            input_tokens: 3,
            output_tokens: 425,
            cache_creation_tokens: 21559,
            cache_read_tokens: 50000,
            git_branch: None,
            repo_id: None,
            provider: "claude_code".to_string(),
            cost_cents: Some(5.0),
            session_title: None,
            parent_uuid: None,
            user_name: None,
            machine_name: None,
            cost_confidence: "estimated".to_string(),
            request_id: Some("msg_01XYZ".to_string()),
            speed: None,
            cache_creation_1h_tokens: 0,
            web_search_requests: 0,
        };
        ingest_messages(&mut conn, &[final_entry], None).unwrap();

        // Then insert intermediate (lower output)
        let intermediate = ParsedMessage {
            uuid: "a1".to_string(),
            session_id: Some("s1".to_string()),
            timestamp: "2026-03-25T00:00:01.000Z".parse().unwrap(),
            cwd: None,
            role: "assistant".to_string(),
            model: Some("claude-sonnet-4-6".to_string()),
            input_tokens: 3,
            output_tokens: 10,
            cache_creation_tokens: 21559,
            cache_read_tokens: 50000,
            git_branch: None,
            repo_id: None,
            provider: "claude_code".to_string(),
            cost_cents: Some(1.5),
            session_title: None,
            parent_uuid: None,
            user_name: None,
            machine_name: None,
            cost_confidence: "estimated".to_string(),
            request_id: Some("msg_01XYZ".to_string()),
            speed: None,
            cache_creation_1h_tokens: 0,
            web_search_requests: 0,
        };
        ingest_messages(&mut conn, &[intermediate], None).unwrap();

        // Should still have only 1 message
        let count: i64 = conn
            .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);

        // The surviving row should have the higher output_tokens (425)
        let output: i64 = conn
            .query_row("SELECT output_tokens FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            output, 425,
            "should keep the final entry with higher output"
        );
    }

    /// Messages without request_id should not be affected by cross-parse dedup.
    #[test]
    fn no_request_id_no_dedup() {
        let mut conn = test_db();

        let msg1 = ParsedMessage {
            uuid: "m1".to_string(),
            session_id: Some("s1".to_string()),
            timestamp: "2026-03-25T00:00:01.000Z".parse().unwrap(),
            cwd: None,
            role: "assistant".to_string(),
            model: Some("claude-sonnet-4-6".to_string()),
            input_tokens: 100,
            output_tokens: 50,
            cache_creation_tokens: 0,
            cache_read_tokens: 1000,
            git_branch: None,
            repo_id: None,
            provider: "claude_code".to_string(),
            cost_cents: Some(1.0),
            session_title: None,
            parent_uuid: None,
            user_name: None,
            machine_name: None,
            cost_confidence: "estimated".to_string(),
            request_id: None, // no request_id
            speed: None,
            cache_creation_1h_tokens: 0,
            web_search_requests: 0,
        };
        ingest_messages(&mut conn, &[msg1], None).unwrap();

        let msg2 = ParsedMessage {
            uuid: "m2".to_string(),
            session_id: Some("s1".to_string()),
            timestamp: "2026-03-25T00:00:02.000Z".parse().unwrap(),
            cwd: None,
            role: "assistant".to_string(),
            model: Some("claude-sonnet-4-6".to_string()),
            input_tokens: 200,
            output_tokens: 100,
            cache_creation_tokens: 0,
            cache_read_tokens: 2000,
            git_branch: None,
            repo_id: None,
            provider: "claude_code".to_string(),
            cost_cents: Some(2.0),
            session_title: None,
            parent_uuid: None,
            user_name: None,
            machine_name: None,
            cost_confidence: "estimated".to_string(),
            request_id: None,
            speed: None,
            cache_creation_1h_tokens: 0,
            web_search_requests: 0,
        };
        ingest_messages(&mut conn, &[msg2], None).unwrap();

        // Both should be inserted (different UUIDs, no request_id dedup)
        let count: i64 = conn
            .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            count, 2,
            "messages without request_id should both be inserted"
        );
    }

    #[test]
    fn cache_efficiency_computes_savings() {
        let mut conn = test_db();
        ingest_messages(&mut conn, &messages_with_cache_patterns(), None).unwrap();

        let ce = cache_efficiency(&conn, None, None).unwrap();
        assert_eq!(ce.total_cache_read_tokens, 350);
        assert!(ce.cache_hit_rate > 0.0);
        assert!(ce.cache_savings_cents > 0.0);
    }

    #[test]
    fn session_cost_curve_buckets() {
        let mut conn = test_db();
        // Create messages in a session
        let mut msgs = Vec::new();
        for i in 0..10 {
            msgs.push(ParsedMessage {
                uuid: format!("curve-{}", i),
                session_id: Some("curve-sess".to_string()),
                timestamp: format!("2026-03-14T10:{:02}:00Z", i).parse().unwrap(),
                cwd: None,
                role: "assistant".to_string(),
                model: Some("claude-opus-4-6".to_string()),
                input_tokens: 100,
                output_tokens: 50,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                git_branch: None,
                repo_id: None,
                provider: "claude_code".to_string(),
                cost_cents: Some(1.0),
                session_title: None,
                parent_uuid: None,
                user_name: None,
                machine_name: None,
                cost_confidence: "exact".to_string(),
                request_id: None,
                speed: None,
                cache_creation_1h_tokens: 0,
                web_search_requests: 0,
            });
        }
        ingest_messages(&mut conn, &msgs, None).unwrap();

        let curve = session_cost_curve(&conn, None, None).unwrap();
        assert!(!curve.is_empty());
        // 10 messages -> bucket "6-15"
        let bucket = curve.iter().find(|b| b.bucket == "6-15").unwrap();
        assert_eq!(bucket.session_count, 1);
    }

    #[test]
    fn cost_confidence_stats_groups_correctly() {
        let mut conn = test_db();
        let msgs = vec![
            ParsedMessage {
                uuid: "conf-1".to_string(),
                session_id: Some("s1".to_string()),
                timestamp: "2026-03-14T10:00:00Z".parse().unwrap(),
                cwd: None,
                role: "assistant".to_string(),
                model: Some("claude-opus-4-6".to_string()),
                input_tokens: 100,
                output_tokens: 50,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                git_branch: None,
                repo_id: None,
                provider: "claude_code".to_string(),
                cost_cents: Some(1.0),
                session_title: None,
                parent_uuid: None,
                user_name: None,
                machine_name: None,
                cost_confidence: "otel_exact".to_string(),
                request_id: None,
                speed: None,
                cache_creation_1h_tokens: 0,
                web_search_requests: 0,
            },
            ParsedMessage {
                uuid: "conf-2".to_string(),
                session_id: Some("s1".to_string()),
                timestamp: "2026-03-14T10:01:00Z".parse().unwrap(),
                cwd: None,
                role: "assistant".to_string(),
                model: Some("claude-opus-4-6".to_string()),
                input_tokens: 100,
                output_tokens: 50,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                git_branch: None,
                repo_id: None,
                provider: "claude_code".to_string(),
                cost_cents: Some(2.0),
                session_title: None,
                parent_uuid: None,
                user_name: None,
                machine_name: None,
                cost_confidence: "estimated".to_string(),
                request_id: None,
                speed: None,
                cache_creation_1h_tokens: 0,
                web_search_requests: 0,
            },
        ];
        ingest_messages(&mut conn, &msgs, None).unwrap();

        let stats = cost_confidence_stats(&conn, None, None).unwrap();
        assert_eq!(stats.len(), 2);
        let otel = stats.iter().find(|s| s.confidence == "otel_exact").unwrap();
        assert_eq!(otel.message_count, 1);
        let est = stats.iter().find(|s| s.confidence == "estimated").unwrap();
        assert_eq!(est.message_count, 1);
    }

    #[test]
    fn subagent_cost_stats_splits_correctly() {
        let mut conn = test_db();
        let msgs = vec![
            ParsedMessage {
                uuid: "main-1".to_string(),
                session_id: Some("s1".to_string()),
                timestamp: "2026-03-14T10:00:00Z".parse().unwrap(),
                cwd: None,
                role: "assistant".to_string(),
                model: Some("claude-opus-4-6".to_string()),
                input_tokens: 100,
                output_tokens: 50,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                git_branch: None,
                repo_id: None,
                provider: "claude_code".to_string(),
                cost_cents: Some(3.0),
                session_title: None,
                parent_uuid: None,
                user_name: None,
                machine_name: None,
                cost_confidence: "exact".to_string(),
                request_id: None,
                speed: None,
                cache_creation_1h_tokens: 0,
                web_search_requests: 0,
            },
            ParsedMessage {
                uuid: "sub-1".to_string(),
                session_id: Some("s1".to_string()),
                timestamp: "2026-03-14T10:01:00Z".parse().unwrap(),
                cwd: None,
                role: "assistant".to_string(),
                model: Some("claude-opus-4-6".to_string()),
                input_tokens: 200,
                output_tokens: 100,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                git_branch: None,
                repo_id: None,
                provider: "claude_code".to_string(),
                cost_cents: Some(5.0),
                session_title: None,
                parent_uuid: Some("main-1".to_string()),
                user_name: None,
                machine_name: None,
                cost_confidence: "exact".to_string(),
                request_id: None,
                speed: None,
                cache_creation_1h_tokens: 0,
                web_search_requests: 0,
            },
        ];
        ingest_messages(&mut conn, &msgs, None).unwrap();

        let stats = subagent_cost_stats(&conn, None, None).unwrap();
        assert_eq!(stats.len(), 2);
        let main = stats.iter().find(|s| s.category == "main").unwrap();
        assert_eq!(main.message_count, 1);
        assert!((main.cost_cents - 3.0).abs() < 0.01);
        let sub = stats.iter().find(|s| s.category == "subagent").unwrap();
        assert_eq!(sub.message_count, 1);
        assert!((sub.cost_cents - 5.0).abs() < 0.01);
    }

    #[test]
    fn session_list_returns_sessions() {
        let mut conn = test_db();
        ingest_messages(&mut conn, &sample_messages(), None).unwrap();

        let result = session_list(
            &conn,
            &SessionListParams {
                since: None,
                until: None,
                search: None,
                sort_by: None,
                sort_asc: false,
                limit: 50,
                offset: 0,
            },
        )
        .unwrap();
        // sample_messages has 1 assistant message in sess-abc
        assert!(!result.sessions.is_empty());
        assert!(result.total_count >= 1);
    }

    /// Helper: create a minimal assistant ParsedMessage, overriding only what matters.
    fn assistant_msg(uuid: &str, session_id: &str, cost_cents: f64) -> ParsedMessage {
        ParsedMessage {
            uuid: uuid.to_string(),
            session_id: Some(session_id.to_string()),
            timestamp: "2026-03-14T10:00:00Z".parse().unwrap(),
            cwd: None,
            role: "assistant".to_string(),
            model: Some("claude-opus-4-6".to_string()),
            input_tokens: 100,
            output_tokens: 50,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            git_branch: None,
            repo_id: None,
            provider: "claude_code".to_string(),
            cost_cents: Some(cost_cents),
            session_title: None,
            parent_uuid: None,
            user_name: None,
            machine_name: None,
            cost_confidence: "exact".to_string(),
            request_id: None,
            speed: None,
            cache_creation_1h_tokens: 0,
            web_search_requests: 0,
        }
    }

    #[test]
    fn activity_chart_groups_by_day() {
        let mut conn = test_db();
        let mut msg1 = assistant_msg("act-1", "s1", 2.0);
        msg1.timestamp = "2026-03-14T10:00:00Z".parse().unwrap();
        let mut msg2 = assistant_msg("act-2", "s1", 3.0);
        msg2.timestamp = "2026-03-15T14:00:00Z".parse().unwrap();
        ingest_messages(&mut conn, &[msg1, msg2], None).unwrap();

        let chart = activity_chart(&conn, None, None, "day", 0).unwrap();
        assert_eq!(chart.len(), 2);
        assert_eq!(chart[0].label, "2026-03-14");
        assert_eq!(chart[0].message_count, 1);
        assert_eq!(chart[1].label, "2026-03-15");
        assert_eq!(chart[1].message_count, 1);
    }

    #[test]
    fn activity_chart_hour_granularity() {
        let mut conn = test_db();
        let msg = assistant_msg("act-h1", "s1", 1.0);
        ingest_messages(&mut conn, &[msg], None).unwrap();

        let chart = activity_chart(&conn, None, None, "hour", 0).unwrap();
        assert_eq!(chart.len(), 1);
        assert_eq!(chart[0].label, "10:00");
    }

    #[test]
    fn branch_cost_groups_by_branch() {
        let mut conn = test_db();
        let mut msg1 = assistant_msg("br-1", "s1", 5.0);
        msg1.git_branch = Some("main".to_string());
        msg1.repo_id = Some("my-repo".to_string());
        let mut msg2 = assistant_msg("br-2", "s2", 3.0);
        msg2.git_branch = Some("feature".to_string());
        msg2.repo_id = Some("my-repo".to_string());
        let mut msg3 = assistant_msg("br-3", "s1", 2.0);
        msg3.git_branch = Some("main".to_string());
        msg3.repo_id = Some("my-repo".to_string());
        ingest_messages(&mut conn, &[msg1, msg2, msg3], None).unwrap();

        let branches = branch_cost(&conn, None, None, 10).unwrap();
        assert_eq!(branches.len(), 2);
        // Ordered by cost DESC: main (7.0) > feature (3.0)
        assert_eq!(branches[0].git_branch, "main");
        assert!((branches[0].cost_cents - 7.0).abs() < 0.01);
        assert_eq!(branches[0].message_count, 2);
        assert_eq!(branches[1].git_branch, "feature");
        assert!((branches[1].cost_cents - 3.0).abs() < 0.01);
    }

    #[test]
    fn branch_cost_single_finds_branch() {
        let mut conn = test_db();
        let mut msg = assistant_msg("brs-1", "s1", 4.0);
        msg.git_branch = Some("fix/bug-123".to_string());
        msg.repo_id = Some("repo".to_string());
        ingest_messages(&mut conn, &[msg], None).unwrap();

        let result = branch_cost_single(&conn, "fix/bug-123", None, None).unwrap();
        assert!(result.is_some());
        let bc = result.unwrap();
        assert_eq!(bc.git_branch, "fix/bug-123");
        assert!((bc.cost_cents - 4.0).abs() < 0.01);

        // Non-existent branch returns None
        let none = branch_cost_single(&conn, "nonexistent", None, None).unwrap();
        assert!(none.is_none());
    }

    #[test]
    fn branch_cost_untagged() {
        let mut conn = test_db();
        // Message with no git_branch
        let msg = assistant_msg("br-untagged", "s1", 6.0);
        ingest_messages(&mut conn, &[msg], None).unwrap();

        let branches = branch_cost(&conn, None, None, 10).unwrap();
        assert_eq!(branches.len(), 1);
        assert_eq!(branches[0].git_branch, "(untagged)");
    }

    #[test]
    fn model_usage_groups_by_model() {
        let mut conn = test_db();
        let msg1 = assistant_msg("mu-1", "s1", 5.0);
        let mut msg2 = assistant_msg("mu-2", "s1", 3.0);
        msg2.model = Some("claude-sonnet-4-6".to_string());
        ingest_messages(&mut conn, &[msg1, msg2], None).unwrap();

        let models = model_usage(&conn, None, None, 10).unwrap();
        assert_eq!(models.len(), 2);
        // Ordered by cost DESC
        assert_eq!(models[0].model, "claude-opus-4-6");
        assert!((models[0].cost_cents - 5.0).abs() < 0.01);
        assert_eq!(models[1].model, "claude-sonnet-4-6");
        assert!((models[1].cost_cents - 3.0).abs() < 0.01);
    }

    #[test]
    fn tag_stats_groups_by_tag() {
        let mut conn = test_db();
        let msg1 = assistant_msg("ts-1", "s1", 10.0);
        let msg2 = assistant_msg("ts-2", "s2", 6.0);
        let tags = vec![
            vec![Tag {
                key: "repo".to_string(),
                value: "proj-a".to_string(),
            }],
            vec![Tag {
                key: "repo".to_string(),
                value: "proj-b".to_string(),
            }],
        ];
        ingest_messages(&mut conn, &[msg1, msg2], Some(&tags)).unwrap();

        let stats = tag_stats(&conn, Some("repo"), None, None, 10).unwrap();
        // Should have proj-a, proj-b, and (untagged) entries
        let proj_a = stats.iter().find(|s| s.value == "proj-a").unwrap();
        assert!((proj_a.cost_cents - 10.0).abs() < 0.01);
        let proj_b = stats.iter().find(|s| s.value == "proj-b").unwrap();
        assert!((proj_b.cost_cents - 6.0).abs() < 0.01);
    }

    #[test]
    fn tag_stats_even_split_across_values() {
        let mut conn = test_db();
        // One session with two tag values — cost should be split evenly
        let msg = assistant_msg("ts-split", "s-split", 10.0);
        let tags = vec![vec![
            Tag {
                key: "ticket".to_string(),
                value: "ABC-1".to_string(),
            },
            Tag {
                key: "ticket".to_string(),
                value: "DEF-2".to_string(),
            },
        ]];
        ingest_messages(&mut conn, &[msg], Some(&tags)).unwrap();

        let stats = tag_stats(&conn, Some("ticket"), None, None, 10).unwrap();
        let abc = stats.iter().find(|s| s.value == "ABC-1").unwrap();
        let def = stats.iter().find(|s| s.value == "DEF-2").unwrap();
        // 10 cents split evenly = 5 each
        assert!((abc.cost_cents - 5.0).abs() < 0.01);
        assert!((def.cost_cents - 5.0).abs() < 0.01);
    }

    #[test]
    fn session_messages_returns_assistant_only() {
        let mut conn = test_db();
        let mut msgs = sample_messages();
        // sample_messages: u1 (user, sess-abc), a1 (assistant, sess-abc), u2 (user, sess-def)
        ingest_messages(&mut conn, &msgs, None).unwrap();

        let result = session_messages(&conn, "sess-abc").unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].uuid, "a1");
        assert_eq!(result[0].role, "assistant");
    }

    #[test]
    fn session_tags_returns_distinct_tags() {
        let mut conn = test_db();
        let msg = assistant_msg("st-1", "sess-tags", 1.0);
        let tags = vec![vec![
            Tag {
                key: "repo".to_string(),
                value: "my-repo".to_string(),
            },
            Tag {
                key: "activity".to_string(),
                value: "feature".to_string(),
            },
        ]];
        ingest_messages(&mut conn, &[msg], Some(&tags)).unwrap();

        let result = session_tags(&conn, "sess-tags").unwrap();
        assert_eq!(result.len(), 2);
        assert!(result.contains(&("activity".to_string(), "feature".to_string())));
        assert!(result.contains(&("repo".to_string(), "my-repo".to_string())));
    }

    #[test]
    fn session_tags_empty_for_unknown_session() {
        let conn = test_db();
        let result = session_tags(&conn, "nonexistent").unwrap();
        assert!(result.is_empty());
    }
}
