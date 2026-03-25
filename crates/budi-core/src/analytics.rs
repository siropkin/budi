//! SQLite-backed analytics storage for AI coding agent usage data.
//!
//! Stores sessions, messages, and tool usage extracted from JSONL transcript
//! files across all providers. Supports incremental ingestion via sync state
//! tracking (byte offset per file).

use std::collections::HashMap;
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
         PRAGMA synchronous=NORMAL;",
    )?;
    Ok(conn)
}

/// Open the analytics database and run pending migrations.
/// Used by `budi sync`, `budi update`, and `budi migrate`.
/// Automatically backfills tags if the migration created the tags table.
pub fn open_db_with_migration(db_path: &Path) -> Result<Connection> {
    let mut conn = open_db(db_path)?;
    let needs_tag_backfill = crate::migration::migrate(&conn)?;
    if needs_tag_backfill {
        tracing::info!("Backfilling tags after migration...");
        let count = backfill_tags(&mut conn)?;
        tracing::info!("Backfilled {} tags.", count);
    }
    Ok(conn)
}

/// Run schema migration. Exposed for cross-module test helpers.
#[doc(hidden)]
pub fn migrate_for_test(conn: &Connection) {
    let _ = crate::migration::migrate(conn).expect("migration failed");
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

/// A tag to be stored alongside a message.
#[derive(Debug, Clone)]
pub struct Tag {
    pub key: String,
    pub value: String,
}

/// Ingest a batch of parsed messages into the database.
/// `tags` is parallel to `messages` — each entry is the list of tags for that message.
pub fn ingest_messages(
    conn: &mut Connection,
    messages: &[ParsedMessage],
    tags: Option<&[Vec<Tag>]>,
) -> Result<usize> {
    let tx = conn.transaction()?;
    let mut count = 0;

    for (i, msg) in messages.iter().enumerate() {
        // Insert message (skip duplicates).
        let ts = msg.timestamp.to_rfc3339();
        // cost_cents is set by CostEnricher in the pipeline before ingest
        let cost_cents = msg.cost_cents;
        let ctx_used = msg.context_tokens_used.map(|v| v as i64);
        let ctx_limit = msg.context_token_limit.map(|v| v as i64);
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO messages
             (uuid, session_id, role, timestamp, model,
              input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
              cwd, repo_id, provider,
              cost_cents, context_tokens_used, context_token_limit,
              parent_uuid, git_branch, cost_confidence)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
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
                ctx_used,
                ctx_limit,
                msg.parent_uuid,
                msg.git_branch,
                msg.cost_confidence,
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

    tx.commit()?;
    Ok(count)
}

/// Path to the analytics database file.
pub fn db_path() -> Result<PathBuf> {
    let home_dir = crate::config::budi_home_dir()?;
    Ok(home_dir.join("analytics.db"))
}

/// Regenerate all tags from existing messages using the current pipeline enrichers.
/// Reads existing tags to populate ParsedMessage fields (git_branch, session_title,
/// user_name, machine_name) so enrichers can reproduce them.
/// Clears the tags table and re-populates it.
pub fn backfill_tags(conn: &mut Connection) -> Result<usize> {
    let tags_config = crate::config::load_tags_config();
    let session_cache = crate::hooks::load_session_meta(conn).unwrap_or_default();
    let mut pipeline = crate::pipeline::Pipeline::default_pipeline(tags_config, session_cache);

    // Read all messages
    let mut parsed: Vec<crate::jsonl::ParsedMessage> = {
        let mut stmt = conn.prepare(
            "SELECT m.uuid, m.session_id, m.timestamp, m.cwd, m.role, m.model,
                    m.input_tokens, m.output_tokens, m.cache_creation_tokens, m.cache_read_tokens,
                    m.provider, m.cost_cents, m.repo_id, m.parent_uuid,
                    COALESCE(m.cost_confidence, 'exact')
             FROM messages m",
        )?;
        stmt.query_map([], |row| {
            let uuid: String = row.get(0)?;
            Ok(crate::jsonl::ParsedMessage {
                uuid,
                session_id: row.get(1)?,
                timestamp: row
                    .get::<_, String>(2)?
                    .parse()
                    .unwrap_or_else(|_| chrono::Utc::now()),
                cwd: row.get(3)?,
                role: row.get(4)?,
                model: row.get(5)?,
                input_tokens: row.get::<_, i64>(6)? as u64,
                output_tokens: row.get::<_, i64>(7)? as u64,
                cache_creation_tokens: row.get::<_, i64>(8)? as u64,
                cache_read_tokens: row.get::<_, i64>(9)? as u64,
                git_branch: None,
                repo_id: row.get(12)?,
                provider: row
                    .get::<_, Option<String>>(10)?
                    .unwrap_or_else(|| "claude_code".to_string()),
                cost_cents: row.get(11)?,
                context_tokens_used: None,
                context_token_limit: None,
                session_title: None,
                parent_uuid: row.get(13)?,
                user_name: None,
                machine_name: None,
                cost_confidence: row.get::<_, String>(14)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect()
    }; // stmt dropped here

    // Populate fields from existing tags so enrichers can reproduce session-level tags
    {
        let mut tag_stmt = conn.prepare(
            "SELECT message_uuid, key, value FROM tags WHERE key IN ('branch', 'session_title', 'user', 'machine')",
        )?;
        let tag_rows: Vec<(String, String, String)> = tag_stmt
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .filter_map(|r| r.ok())
            .collect();

        let mut tag_map: HashMap<String, Vec<(String, String)>> = HashMap::new();
        for (uuid, key, value) in tag_rows {
            tag_map.entry(uuid).or_default().push((key, value));
        }

        for msg in &mut parsed {
            if let Some(tags) = tag_map.get(&msg.uuid) {
                for (key, value) in tags {
                    match key.as_str() {
                        "branch" => msg.git_branch = Some(value.clone()),
                        "session_title" => msg.session_title = Some(value.clone()),
                        "user" => msg.user_name = Some(value.clone()),
                        "machine" => msg.machine_name = Some(value.clone()),
                        _ => {}
                    }
                }
            }
        }
    }

    let all_tags = pipeline.process(&mut parsed);

    // Clear and re-insert tags
    let tx = conn.transaction()?;
    tx.execute_batch("DELETE FROM tags")?;
    let mut count = 0usize;
    for (i, msg) in parsed.iter().enumerate() {
        if let Some(msg_tags) = all_tags.get(i) {
            for tag in msg_tags {
                tx.execute(
                    "INSERT OR IGNORE INTO tags (message_uuid, key, value) VALUES (?1, ?2, ?3)",
                    params![msg.uuid, tag.key, tag.value],
                )?;
                count += 1;
            }
        }
        // Also update the denormalized git_branch column
        if let Some(ref branch) = msg.git_branch {
            tx.execute(
                "UPDATE messages SET git_branch = ?2 WHERE uuid = ?1",
                params![msg.uuid, branch],
            )?;
        }
    }
    tx.commit()?;
    Ok(count)
}

/// Quick sync: only files modified in the last 7 days.
/// Used by `budi sync` and the daemon's 30s auto-sync.
pub fn sync_all(conn: &mut Connection) -> Result<(usize, usize)> {
    sync_with_max_age(conn, Some(7))
}

/// Full history sync: process ALL transcript files regardless of age.
/// Used by `budi history` — may take minutes on large histories.
pub fn sync_history(conn: &mut Connection) -> Result<(usize, usize)> {
    sync_with_max_age(conn, None)
}

/// Internal sync implementation with optional max_age filter.
/// When `max_age_days` is Some(N), only files modified in the last N days are processed.
/// When None, all files are processed.
fn sync_with_max_age(conn: &mut Connection, max_age_days: Option<u64>) -> Result<(usize, usize)> {
    let providers = crate::provider::available_providers();
    let tags_config = crate::config::load_tags_config();
    let session_cache = crate::hooks::load_session_meta(conn).unwrap_or_default();
    let mut pipeline = crate::pipeline::Pipeline::default_pipeline(tags_config, session_cache);
    let mut total_files = 0;
    let mut total_messages = 0;

    let cutoff = max_age_days.map(|days| {
        std::time::SystemTime::now()
            - std::time::Duration::from_secs(days * 86400)
    });

    for provider in &providers {
        // Try direct sync first (e.g. Cursor Usage API).
        if let Some(result) = provider.sync_direct(conn, &mut pipeline) {
            let (files, messages) = result?;
            total_files += files;
            total_messages += messages;
            continue;
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

            let content = std::fs::read_to_string(file_path)
                .with_context(|| format!("Failed to read {}", file_path.display()))?;

            if offset >= content.len() {
                continue; // Already fully synced.
            }

            let (mut messages, new_offset) = provider.parse_file(file_path, &content, offset)?;
            if messages.is_empty() {
                set_sync_offset(conn, &path_str, new_offset)?;
                continue;
            }

            let tags = pipeline.process(&mut messages);
            let count = ingest_messages(conn, &messages, Some(&tags))?;
            set_sync_offset(conn, &path_str, new_offset)?;

            if count > 0 {
                total_files += 1;
                total_messages += count;
            }
        }
    }

    Ok((total_files, total_messages))
}

/// Sync a single JSONL transcript file (used for hook-triggered incremental sync).
/// Returns the number of messages ingested.
pub fn sync_one_file(conn: &mut Connection, file_path: &Path) -> Result<usize> {
    use crate::provider::Provider;
    let provider = crate::providers::claude_code::ClaudeCodeProvider;
    let tags_config = crate::config::load_tags_config();
    let session_cache = crate::hooks::load_session_meta(conn).unwrap_or_default();
    let mut pipeline = crate::pipeline::Pipeline::default_pipeline(tags_config, session_cache);
    let path_str = file_path.display().to_string();
    let offset = get_sync_offset(conn, &path_str)?;

    let content = std::fs::read_to_string(file_path)
        .with_context(|| format!("Failed to read {}", file_path.display()))?;

    if offset >= content.len() {
        return Ok(0);
    }

    let (mut messages, new_offset) = provider.parse_file(file_path, &content, offset)?;
    if messages.is_empty() {
        set_sync_offset(conn, &path_str, new_offset)?;
        return Ok(0);
    }

    let tags = pipeline.process(&mut messages);
    let count = ingest_messages(conn, &messages, Some(&tags))?;
    set_sync_offset(conn, &path_str, new_offset)?;
    Ok(count)
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

/// Build a parameterized date filter clause and its bind values.
/// Returns (clause_str, params_vec) where clause_str uses ?N placeholders.
fn date_filter(
    since: Option<&str>,
    until: Option<&str>,
    keyword: &str,
    param_start: usize,
) -> (String, Vec<String>) {
    let mut conditions = Vec::new();
    let mut param_values = Vec::new();
    if let Some(s) = since {
        param_values.push(s.to_string());
        conditions.push(format!(
            "timestamp >= ?{}",
            param_start + param_values.len()
        ));
    }
    if let Some(u) = until {
        param_values.push(u.to_string());
        conditions.push(format!("timestamp < ?{}", param_start + param_values.len()));
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
pub fn usage_summary(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<UsageSummary> {
    let (where_clause, date_params) = date_filter(since, until, "WHERE", 0);
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = date_params
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    // Single scan: all aggregates in one query
    let sql = format!(
        "SELECT COUNT(*),
                SUM(CASE WHEN role = 'user' THEN 1 ELSE 0 END),
                SUM(CASE WHEN role = 'assistant' THEN 1 ELSE 0 END),
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
    ): (u64, u64, u64, u64, u64, u64, u64) =
        conn.query_row(&sql, param_refs.as_slice(), |r| {
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
    let mut conditions = vec!["role = 'assistant'".to_string()];
    let mut param_values: Vec<String> = Vec::new();
    if let Some(s) = p.since {
        param_values.push(s.to_string());
        conditions.push(format!("timestamp >= ?{}", param_values.len()));
    }
    if let Some(u) = p.until {
        param_values.push(u.to_string());
        conditions.push(format!("timestamp < ?{}", param_values.len()));
    }
    if let Some(q) = p.search
        && !q.is_empty()
    {
        param_values.push(format!("%{q}%"));
        let idx = param_values.len();
        conditions.push(format!(
            "(model LIKE ?{idx} OR repo_id LIKE ?{idx} OR cwd LIKE ?{idx} OR provider LIKE ?{idx})"
        ));
    }
    let where_clause = format!("WHERE {}", conditions.join(" AND "));
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    let order_col = match p.sort_by.unwrap_or("timestamp") {
        "model" => "model",
        "provider" => "provider",
        "tokens" => "(input_tokens + output_tokens)",
        "cost" => "COALESCE(cost_cents, 0.0)",
        _ => "timestamp",
    };
    let order_dir = if p.sort_asc { "ASC" } else { "DESC" };

    let count_sql = format!("SELECT COUNT(*) FROM messages {}", where_clause);
    let total_count: u64 = conn.query_row(&count_sql, param_refs.as_slice(), |r| r.get(0))?;

    let sql = format!(
        "SELECT messages.uuid, messages.timestamp, messages.role, messages.model,
                COALESCE(messages.provider, 'claude_code'),
                COALESCE(messages.repo_id, s.repo_id),
                messages.input_tokens, messages.output_tokens,
                messages.cache_creation_tokens, messages.cache_read_tokens,
                COALESCE(messages.cost_cents, 0.0),
                COALESCE(messages.cost_confidence, 'exact'),
                COALESCE(messages.git_branch, s.git_branch)
         FROM messages
         LEFT JOIN sessions s ON s.conversation_id = messages.session_id
         {}
         ORDER BY {} {}
         LIMIT {} OFFSET {}",
        where_clause, order_col, order_dir, p.limit, p.offset
    );

    let mut stmt = conn.prepare(&sql)?;
    let messages = stmt
        .query_map(param_refs.as_slice(), |row| {
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
    let mut conditions = vec!["repo_id IS NOT NULL".to_string()];
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
        "SELECT repo_id, MIN(cwd) as display_path, COUNT(*) as cnt,
                COALESCE(SUM(input_tokens), 0) as inp,
                COALESCE(SUM(output_tokens), 0) as outp,
                COALESCE(SUM(cost_cents), 0.0) as cost
         FROM messages
         WHERE {}
         GROUP BY repo_id
         HAVING cost > 0 OR (inp + outp) > 0
         ORDER BY cost DESC
         LIMIT ?{}",
        conditions.join(" AND "),
        limit_idx
    );

    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        param_values.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let mut rows: Vec<RepoUsage> = stmt
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

    // Add "(untagged)" for messages without repo_id
    let tagged_cost: f64 = rows.iter().map(|r| r.cost_cents).sum();
    let (total_where, total_params) = date_filter(since, until, "WHERE", 0);
    let total_refs: Vec<&dyn rusqlite::types::ToSql> = total_params
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();
    let total_cost: f64 = conn
        .query_row(
            &format!(
                "SELECT COALESCE(SUM(cost_cents), 0.0) FROM messages {}",
                total_where
            ),
            total_refs.as_slice(),
            |r| r.get(0),
        )
        .unwrap_or(0.0);
    let untagged = total_cost - tagged_cost;
    if untagged > 0.01 {
        rows.push(RepoUsage {
            repo_id: "(untagged)".to_string(),
            display_path: "(untagged)".to_string(),
            message_count: 0,
            input_tokens: 0,
            output_tokens: 0,
            cost_cents: untagged,
        });
    }

    Ok(rows)
}

/// Cache efficiency stats for a date range.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CacheStats {
    pub total_input_tokens: u64,
    pub total_cache_read_tokens: u64,
    pub total_cache_creation_tokens: u64,
    pub hit_rate: f64,
}

pub fn cache_stats(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<CacheStats> {
    let (where_clause, date_params) = date_filter(since, until, "WHERE", 0);
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = date_params
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    let (total_input, cache_read, cache_creation): (u64, u64, u64) = conn.query_row(
        &format!(
            "SELECT COALESCE(SUM(input_tokens + cache_creation_tokens + cache_read_tokens), 0),
                    COALESCE(SUM(cache_read_tokens), 0),
                    COALESCE(SUM(cache_creation_tokens), 0)
             FROM messages {}",
            where_clause
        ),
        param_refs.as_slice(),
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;

    let hit_rate = if total_input > 0 {
        cache_read as f64 / total_input as f64
    } else {
        0.0
    };

    Ok(CacheStats {
        total_input_tokens: total_input,
        total_cache_read_tokens: cache_read,
        total_cache_creation_tokens: cache_creation,
        hit_rate,
    })
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
    let (where_clause, date_params) = date_filter(since, until, "WHERE", 0);
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = date_params
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    // Apply timezone offset to get local time grouping
    let tz_adjust = if tz_offset_min != 0 {
        let hours = tz_offset_min / 60;
        let mins = (tz_offset_min % 60).abs();
        let sign = if tz_offset_min >= 0 { "+" } else { "-" };
        format!(
            "datetime(timestamp, '{}{:02}:{:02}')",
            sign,
            hours.abs(),
            mins
        )
    } else {
        "timestamp".to_string()
    };

    let group_expr = match granularity {
        "hour" => format!("strftime('%H:00', {})", tz_adjust),
        "month" => format!("strftime('%Y-%m', {})", tz_adjust),
        _ => format!("date({})", tz_adjust),
    };

    let sql = format!(
        "SELECT {} as bucket, COUNT(*) as cnt,
                COALESCE(SUM(input_tokens + cache_creation_tokens + cache_read_tokens), 0),
                COALESCE(SUM(output_tokens), 0),
                COALESCE(SUM(cost_cents), 0.0)
         FROM messages {}
         GROUP BY bucket ORDER BY bucket",
        group_expr, where_clause
    );

    let mut stmt = conn.prepare(&sql)?;
    let msg_rows: Vec<(String, u64, u64, u64, f64)> = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?
        .filter_map(|r| match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("skipping row: {e}");
                None
            }
        })
        .collect();

    // Tool calls per bucket — use m.timestamp for the join
    let tool_group = group_expr.replace("timestamp", "m.timestamp");
    let tool_where = if where_clause.is_empty() {
        String::new()
    } else {
        where_clause.replace("timestamp", "m.timestamp")
    };
    let tool_sql = format!(
        "SELECT {} as bucket, COUNT(*) as cnt
         FROM tool_usage tu
         JOIN messages m ON tu.message_uuid = m.uuid
         {}
         GROUP BY bucket ORDER BY bucket",
        tool_group, tool_where
    );

    let mut tool_stmt = conn.prepare(&tool_sql)?;
    let tool_rows: HashMap<String, u64> = tool_stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
        })?
        .filter_map(|r| match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("skipping row: {e}");
                None
            }
        })
        .collect();

    let results = msg_rows
        .into_iter()
        .map(|(label, count, inp, outp, cost)| ActivityBucket {
            tool_call_count: tool_rows.get(&label).copied().unwrap_or(0),
            label,
            message_count: count,
            cost_cents: cost,
            input_tokens: inp,
            output_tokens: outp,
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
) -> Result<Vec<BranchCost>> {
    let mut conditions = vec![
        "git_branch IS NOT NULL".to_string(),
        "git_branch != ''".to_string(),
    ];
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
         LIMIT 20",
    );

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();
    let mut stmt = conn.prepare(&sql)?;
    let mut rows: Vec<BranchCost> = stmt
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

    // Add "untagged" entry for messages not in any branch
    let tagged_cost: f64 = rows.iter().map(|r| r.cost_cents).sum();
    let (total_where, total_params) = date_filter(since, until, "WHERE", 0);
    let total_refs: Vec<&dyn rusqlite::types::ToSql> = total_params
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();
    let total_cost: f64 = conn
        .query_row(
            &format!(
                "SELECT COALESCE(SUM(cost_cents), 0.0) FROM messages {}",
                total_where
            ),
            total_refs.as_slice(),
            |r| r.get(0),
        )
        .unwrap_or(0.0);
    let untagged = total_cost - tagged_cost;
    if untagged > 0.01 {
        rows.push(BranchCost {
            git_branch: "(untagged)".to_string(),
            repo_id: String::new(),
            session_count: 0,
            message_count: 0,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            cost_cents: untagged,
        });
    }

    Ok(rows)
}

/// Query cost for a single branch via branch_cost.
pub fn branch_cost_single(
    conn: &Connection,
    branch: &str,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Option<BranchCost>> {
    let branch_stripped = branch.strip_prefix("refs/heads/").unwrap_or(branch);
    let all = branch_cost(conn, since, until)?;
    Ok(all
        .into_iter()
        .find(|b| {
            let v = b.git_branch.strip_prefix("refs/heads/").unwrap_or(&b.git_branch);
            v.eq_ignore_ascii_case(branch_stripped)
        }))
}

/// Tag-based cost breakdown: cost grouped by tag key+value.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TagCost {
    pub key: String,
    pub value: String,
    pub message_count: u64,
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
    let mut tag_where = String::new();
    let mut date_where = String::new();
    {
        let mut idx = 0usize;
        if tag_key.is_some() {
            idx += 1;
            tag_where = format!("WHERE t.key = ?{idx}");
        }
        let mut dconds = Vec::new();
        if since.is_some() {
            idx += 1;
            dconds.push(format!("timestamp >= ?{idx}"));
        }
        if until.is_some() {
            idx += 1;
            dconds.push(format!("timestamp < ?{idx}"));
        }
        if !dconds.is_empty() {
            date_where = format!("WHERE {}", dconds.join(" AND "));
        }
    }

    // Use CTEs with window functions to scan tags+messages only once.
    let sql = format!(
        "WITH tag_sessions AS (
             SELECT DISTINCT t.key, t.value, tm.session_id,
                    COUNT(DISTINCT t.value) OVER (PARTITION BY t.key, tm.session_id) as value_count
             FROM tags t
             JOIN messages tm ON t.message_uuid = tm.uuid
             {tag_where}
         ),
         session_costs AS (
             SELECT session_id, COALESCE(SUM(cost_cents), 0.0) as session_cost
             FROM messages
             {date_where}
             GROUP BY session_id
         )
         SELECT ts.key, ts.value,
                COUNT(DISTINCT ts.session_id) as session_count,
                COALESCE(SUM(sc.session_cost / ts.value_count), 0.0) as total_cost_cents
         FROM tag_sessions ts
         JOIN session_costs sc ON sc.session_id = ts.session_id
         GROUP BY ts.key, ts.value
         ORDER BY total_cost_cents DESC
         LIMIT ?{limit_idx}",
    );

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    let mut stmt = conn.prepare(&sql)?;
    let mut rows: Vec<TagCost> = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(TagCost {
                key: row.get(0)?,
                value: row.get(1)?,
                message_count: row.get(2)?,
                cost_cents: row.get(3)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    // Add "(untagged)" entry for cost not attributed to any tag of this key
    if tag_key.is_some() {
        let tagged_cost: f64 = rows.iter().map(|r| r.cost_cents).sum();
        let (total_where, total_params) = date_filter(since, until, "WHERE", 0);
        let total_refs: Vec<&dyn rusqlite::types::ToSql> = total_params
            .iter()
            .map(|s| s as &dyn rusqlite::types::ToSql)
            .collect();
        let total_cost: f64 = conn
            .query_row(
                &format!(
                    "SELECT COALESCE(SUM(cost_cents), 0.0) FROM messages {}",
                    total_where
                ),
                total_refs.as_slice(),
                |r| r.get(0),
            )
            .unwrap_or(0.0);
        let untagged = total_cost - tagged_cost;
        if untagged > 0.01 {
            rows.push(TagCost {
                key: tag_key.unwrap_or("").to_string(),
                value: "(untagged)".to_string(),
                message_count: 0,
                cost_cents: untagged,
            });
        }
    }

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
) -> Result<Vec<ModelUsage>> {
    let (where_clause, date_params) = date_filter(since, until, "WHERE", 0);
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = date_params
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    let sql = format!(
        "SELECT model as m,
                COALESCE(provider, 'claude_code') as p,
                COUNT(*) as cnt,
                COALESCE(SUM(input_tokens), 0) as total_input,
                COALESCE(SUM(output_tokens), 0) as total_output,
                COALESCE(SUM(cache_read_tokens), 0),
                COALESCE(SUM(cache_creation_tokens), 0),
                COALESCE(SUM(cost_cents), 0.0)
         FROM messages
         {} {} role = 'assistant' AND model IS NOT NULL AND model != '' AND model NOT LIKE '<%'
         GROUP BY m, p
         ORDER BY (total_input + total_output) DESC",
        where_clause,
        if where_clause.is_empty() {
            "WHERE"
        } else {
            "AND"
        }
    );

    let mut stmt = conn.prepare(&sql)?;
    let mut rows: Vec<ModelUsage> = stmt
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

    // Add "(untagged)" for messages without a model (user messages)
    let tagged_cost: f64 = rows.iter().map(|r| r.cost_cents).sum();
    let (total_where, total_params) = date_filter(since, until, "WHERE", 0);
    let total_refs: Vec<&dyn rusqlite::types::ToSql> = total_params
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();
    let total_cost: f64 = conn
        .query_row(
            &format!(
                "SELECT COALESCE(SUM(cost_cents), 0.0) FROM messages {}",
                total_where
            ),
            total_refs.as_slice(),
            |r| r.get(0),
        )
        .unwrap_or(0.0);
    let untagged = total_cost - tagged_cost;
    if untagged > 0.01 {
        rows.push(ModelUsage {
            model: "(untagged)".to_string(),
            provider: String::new(),
            message_count: 0,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            cost_cents: untagged,
        });
    }

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
            "SELECT COALESCE(SUM(cost_cents), 0.0) FROM messages WHERE timestamp >= ?1",
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
            "SELECT COALESCE(SUM(cost_cents), 0.0) FROM messages WHERE session_id = ?1",
            [sid],
            |r| r.get::<_, f64>(0),
        )
        .unwrap_or(0.0)
            / 100.0
    });

    // Branch cost: total cost for sessions with a specific branch tag
    let branch_cost = params.branch.as_ref().map(|branch| {
        conn.query_row(
            "SELECT COALESCE(SUM(m.cost_cents), 0.0) \
             FROM messages m \
             WHERE m.session_id IN ( \
                 SELECT DISTINCT tm.session_id FROM tags t \
                 JOIN messages tm ON t.message_uuid = tm.uuid \
                 WHERE t.key = 'branch' AND t.value = ?1 \
             )",
            [branch],
            |r| r.get::<_, f64>(0),
        )
        .unwrap_or(0.0)
            / 100.0
    });

    // Project cost: total cost for messages in a specific directory
    let project_cost = params.project_dir.as_ref().map(|dir| {
        conn.query_row(
            "SELECT COALESCE(SUM(cost_cents), 0.0) FROM messages WHERE cwd = ?1",
            [dir],
            |r| r.get::<_, f64>(0),
        )
        .unwrap_or(0.0)
            / 100.0
    });

    // Active provider: most recent provider used today
    let active_provider: Option<String> = conn
        .query_row(
            "SELECT COALESCE(provider, 'claude_code') FROM messages \
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

/// Quick check: how many distinct providers have data in the database?
pub fn provider_count(conn: &Connection) -> Result<usize> {
    let count: u64 = conn.query_row(
        "SELECT COUNT(DISTINCT COALESCE(provider, 'claude_code')) FROM messages",
        [],
        |r| r.get(0),
    )?;
    Ok(count as usize)
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
    let (where_clause, date_params) = date_filter(since, until, "WHERE", 0);
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = date_params
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    let sql = format!(
        "SELECT COALESCE(provider, 'claude_code') as p,
                COUNT(*) as msgs,
                COALESCE(SUM(input_tokens), 0),
                COALESCE(SUM(output_tokens), 0),
                COALESCE(SUM(cache_creation_tokens), 0),
                COALESCE(SUM(cache_read_tokens), 0),
                COALESCE(SUM(cost_cents), 0.0)
         FROM messages {}
         GROUP BY p ORDER BY msgs DESC",
        where_clause
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

    for (prov, messages, input, output, cache_create, cache_read, sum_cost_cents) in rows
    {
        let display_name = providers
            .iter()
            .find(|p| p.name() == prov)
            .map(|p| p.display_name().to_string())
            .unwrap_or_else(|| prov.clone());

        // Cost is baked into cost_cents at ingest time — just use the sum.
        // sum_cost_cents is in cents; estimated_cost is in dollars.
        let estimated_cost = (sum_cost_cents / 100.0 * 100.0).round() / 100.0;

        result.push(ProviderStats {
            provider: prov,
            display_name,
            message_count: messages,
            input_tokens: input,
            output_tokens: output,
            cache_creation_tokens: cache_create,
            cache_read_tokens: cache_read,
            estimated_cost,
            total_cost_cents: (sum_cost_cents * 100.0).round() / 100.0,
        });
    }

    Ok(result)
}


/// Context window utilization stats.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ContextUsageStats {
    pub avg_usage_pct: f64,
    pub max_usage_pct: f64,
    pub sessions_over_80_pct: u64,
    pub total_sessions_with_data: u64,
}

pub fn context_usage_stats(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<ContextUsageStats> {
    let (where_clause, date_params) = date_filter(since, until, "WHERE", 0);
    let extra = if where_clause.is_empty() {
        "WHERE context_tokens_used IS NOT NULL AND context_token_limit IS NOT NULL AND context_token_limit > 0".to_string()
    } else {
        format!(
            "{} AND context_tokens_used IS NOT NULL AND context_token_limit IS NOT NULL AND context_token_limit > 0",
            where_clause
        )
    };
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = date_params
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    let sql = format!(
        "SELECT
            AVG(CAST(context_tokens_used AS REAL) * 100.0 / CAST(context_token_limit AS REAL)),
            MAX(CAST(context_tokens_used AS REAL) * 100.0 / CAST(context_token_limit AS REAL)),
            SUM(CASE WHEN CAST(context_tokens_used AS REAL) * 100.0 / CAST(context_token_limit AS REAL) > 80.0 THEN 1 ELSE 0 END),
            COUNT(*)
         FROM messages {}",
        extra
    );

    let result = conn.query_row(&sql, param_refs.as_slice(), |r| {
        Ok(ContextUsageStats {
            avg_usage_pct: r.get::<_, f64>(0).unwrap_or(0.0),
            max_usage_pct: r.get::<_, f64>(1).unwrap_or(0.0),
            sessions_over_80_pct: r.get::<_, u64>(2).unwrap_or(0),
            total_sessions_with_data: r.get::<_, u64>(3).unwrap_or(0),
        })
    })?;

    Ok(result)
}

/// Build a parameterized filter clause that includes optional date range and provider.
fn date_provider_filter(
    since: Option<&str>,
    until: Option<&str>,
    provider: Option<&str>,
    keyword: &str,
    param_start: usize,
) -> (String, Vec<String>) {
    let mut conditions = Vec::new();
    let mut param_values = Vec::new();
    if let Some(s) = since {
        param_values.push(s.to_string());
        conditions.push(format!(
            "timestamp >= ?{}",
            param_start + param_values.len()
        ));
    }
    if let Some(u) = until {
        param_values.push(u.to_string());
        conditions.push(format!("timestamp < ?{}", param_start + param_values.len()));
    }
    if let Some(p) = provider {
        param_values.push(p.to_string());
        conditions.push(format!(
            "COALESCE(provider, 'claude_code') = ?{}",
            param_start + param_values.len()
        ));
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
    let (where_clause, params) = date_provider_filter(since, until, provider, "WHERE", 0);
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    // Single scan: all aggregates in one query
    let sql = format!(
        "SELECT COUNT(*),
                SUM(CASE WHEN role = 'user' THEN 1 ELSE 0 END),
                SUM(CASE WHEN role = 'assistant' THEN 1 ELSE 0 END),
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
    ): (u64, u64, u64, u64, u64, u64, u64) =
        conn.query_row(&sql, param_refs.as_slice(), |r| {
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(tables.contains(&"tool_usage".to_string()));
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
                context_tokens_used: None,
                context_token_limit: None,
                session_title: None,
                parent_uuid: None,
                user_name: None,
                machine_name: None,
                cost_confidence: "exact".to_string(),
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
                context_tokens_used: None,
                context_token_limit: None,
                session_title: None,
                parent_uuid: None,
                user_name: None,
                machine_name: None,
                cost_confidence: "exact".to_string(),
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
        use crate::pipeline::enrichers::CostEnricher;
        use crate::pipeline::Enricher;

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
            context_tokens_used: None,
            context_token_limit: None,
            session_title: None,


            parent_uuid: None,
            user_name: None,
            machine_name: None,
            cost_confidence: "exact".to_string(),
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
                context_tokens_used: None,
                context_token_limit: None,
                session_title: None,
                parent_uuid: None,
                user_name: None,
                machine_name: None,
                cost_confidence: "exact".to_string(),
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
                context_tokens_used: None,
                context_token_limit: None,
                session_title: None,
                parent_uuid: None,
                user_name: None,
                machine_name: None,
                cost_confidence: "exact".to_string(),
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
                context_tokens_used: None,
                context_token_limit: None,
                session_title: None,


                parent_uuid: None,
                user_name: None,
                machine_name: None,
                cost_confidence: "exact".to_string(),
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
                context_tokens_used: None,
                context_token_limit: None,
                session_title: None,


                parent_uuid: None,
                user_name: None,
                machine_name: None,
                cost_confidence: "exact".to_string(),
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
                context_tokens_used: None,
                context_token_limit: None,
                session_title: None,


                parent_uuid: None,
                user_name: None,
                machine_name: None,
                cost_confidence: "exact".to_string(),
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
        // Assign repo_ids
        msgs[0].repo_id = Some("project-a".to_string());
        msgs[1].repo_id = Some("project-a".to_string());
        msgs[2].repo_id = Some("project-b".to_string());
        // Give project-b's user message some tokens so it appears in results
        msgs[2].input_tokens = 50;
        ingest_messages(&mut conn, &msgs, None).unwrap();

        let repos = repo_usage(&conn, None, None, 10).unwrap();
        assert_eq!(repos.len(), 2);
        // project-a has more tokens, project-b has some.
        assert_eq!(repos[0].repo_id, "project-a");
        assert_eq!(repos[0].message_count, 2);
        assert_eq!(repos[1].repo_id, "project-b");
        assert_eq!(repos[1].message_count, 1);
    }

    fn messages_with_tools() -> Vec<ParsedMessage> {
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
                context_tokens_used: None,
                context_token_limit: None,
                session_title: None,


                parent_uuid: None,
                user_name: None,
                machine_name: None,
                cost_confidence: "exact".to_string(),
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
                context_tokens_used: None,
                context_token_limit: None,
                session_title: None,


                parent_uuid: None,
                user_name: None,
                machine_name: None,
                cost_confidence: "exact".to_string(),
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
                context_tokens_used: None,
                context_token_limit: None,
                session_title: None,


                parent_uuid: None,
                user_name: None,
                machine_name: None,
                cost_confidence: "exact".to_string(),
            },
        ]
    }

    #[test]
    fn cache_stats_computes_hit_rate() {
        let mut conn = test_db();
        ingest_messages(&mut conn, &messages_with_tools(), None).unwrap();

        let cs = cache_stats(&conn, None, None).unwrap();
        // total_input = (500+0+200) + (300+100+150) + (50000+0+0) = 51250
        assert_eq!(cs.total_input_tokens, 51250);
        // cache_read = 200 + 150 + 0 = 350
        assert_eq!(cs.total_cache_read_tokens, 350);
        assert!((cs.hit_rate - 350.0 / 51250.0).abs() < 0.001);
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
                context_tokens_used: None,
                context_token_limit: None,
                session_title: None,


                parent_uuid: None,
                user_name: None,
                machine_name: None,
                cost_confidence: "exact".to_string(),
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
                context_tokens_used: None,
                context_token_limit: None,
                session_title: None,


                parent_uuid: None,
                user_name: None,
                machine_name: None,
                cost_confidence: "exact".to_string(),
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
                context_tokens_used: None,
                context_token_limit: None,
                session_title: None,


                parent_uuid: None,
                user_name: None,
                machine_name: None,
                cost_confidence: "exact".to_string(),
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
                context_tokens_used: None,
                context_token_limit: None,
                session_title: None,


                parent_uuid: None,
                user_name: None,
                machine_name: None,
                cost_confidence: "exact".to_string(),
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

        // Provider stats
        let pstats = provider_stats(&conn, None, None).unwrap();
        assert_eq!(pstats.len(), 2);
        let cc_stats = pstats.iter().find(|p| p.provider == "claude_code").unwrap();
        let cu_stats = pstats.iter().find(|p| p.provider == "cursor").unwrap();
        assert_eq!(cc_stats.message_count, 2);
        assert_eq!(cu_stats.message_count, 2);

        // Claude Code is registered, so it gets proper display name and cost.
        assert_eq!(cc_stats.display_name, "Claude Code");
        assert!(cc_stats.estimated_cost > 0.0);
    }

}
