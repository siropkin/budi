//! SQLite-backed analytics storage for AI coding agent usage data.
//!
//! Stores sessions, messages, and tool usage extracted from JSONL transcript
//! files across all providers. Supports incremental ingestion via sync state
//! tracking (byte offset per file).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};

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

/// Insert or update a session record.
#[allow(clippy::too_many_arguments)]
fn upsert_session(
    conn: &Connection,
    session_id: &str,
    timestamp: &DateTime<Utc>,
    cwd: Option<&str>,
    version: Option<&str>,
    git_branch: Option<&str>,
    repo_id: Option<&str>,
    provider: &str,
    session_title: Option<&str>,
    interaction_mode: Option<&str>,
    lines_added: Option<u64>,
    lines_removed: Option<u64>,
    user_name: Option<&str>,
    machine_name: Option<&str>,
) -> Result<()> {
    let ts = timestamp.to_rfc3339();
    let la = lines_added.map(|v| v as i64);
    let lr = lines_removed.map(|v| v as i64);
    conn.execute(
        "INSERT INTO sessions (session_id, project_dir, first_seen, last_seen, version, git_branch, repo_id, provider, session_title, interaction_mode, lines_added, lines_removed, user_name, machine_name)
         VALUES (?1, ?2, ?3, ?3, ?4, ?5, ?6, ?7, ?8, ?9, COALESCE(?10, 0), COALESCE(?11, 0), ?12, ?13)
         ON CONFLICT(session_id) DO UPDATE SET
           last_seen = MAX(sessions.last_seen, ?3),
           project_dir = COALESCE(?2, sessions.project_dir),
           version = COALESCE(?4, sessions.version),
           git_branch = COALESCE(?5, sessions.git_branch),
           repo_id = COALESCE(?6, sessions.repo_id),
           provider = COALESCE(?7, sessions.provider),
           session_title = COALESCE(?8, sessions.session_title),
           interaction_mode = COALESCE(?9, sessions.interaction_mode),
           lines_added = MAX(sessions.lines_added, COALESCE(?10, 0)),
           lines_removed = MAX(sessions.lines_removed, COALESCE(?11, 0)),
           user_name = COALESCE(?12, sessions.user_name),
           machine_name = COALESCE(?13, sessions.machine_name)",
        params![session_id, cwd, ts, version, git_branch, repo_id, provider, session_title, interaction_mode, la, lr, user_name, machine_name],
    )?;
    Ok(())
}

/// Estimate cost in dollars for a message using the appropriate provider's pricing.
fn estimate_cost_for_provider(
    provider: &str,
    model: &str,
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_tokens: u64,
    cache_read_tokens: u64,
) -> f64 {
    let pricing = match provider {
        "cursor" => crate::providers::cursor::cursor_pricing_for_model(model),
        _ => crate::providers::claude_code::claude_pricing_for_model(model),
    };
    input_tokens as f64 * pricing.input / 1_000_000.0
        + output_tokens as f64 * pricing.output / 1_000_000.0
        + cache_creation_tokens as f64 * pricing.cache_write / 1_000_000.0
        + cache_read_tokens as f64 * pricing.cache_read / 1_000_000.0
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
        // Upsert session if we have a session_id.
        if let Some(ref sid) = msg.session_id {
            upsert_session(
                &tx,
                sid,
                &msg.timestamp,
                msg.cwd.as_deref(),
                msg.version.as_deref(),
                msg.git_branch.as_deref(),
                msg.repo_id.as_deref(),
                &msg.provider,
                msg.session_title.as_deref(),
                msg.interaction_mode.as_deref(),
                msg.lines_added,
                msg.lines_removed,
                msg.user_name.as_deref(),
                msg.machine_name.as_deref(),
            )?;
        }

        // Insert message (skip duplicates).
        let ts = msg.timestamp.to_rfc3339();
        // Calculate cost_cents at ingest time if not already provided
        let cost_cents = msg.cost_cents.or_else(|| {
            if msg.role == "assistant" {
                let pricing = estimate_cost_for_provider(
                    &msg.provider,
                    msg.model.as_deref().unwrap_or("unknown"),
                    msg.input_tokens,
                    msg.output_tokens,
                    msg.cache_creation_tokens,
                    msg.cache_read_tokens,
                );
                if pricing > 0.0 {
                    Some((pricing * 100.0 * 100.0).round() / 100.0) // cents, 2 decimal places
                } else {
                    None
                }
            } else {
                None
            }
        });
        let ctx_used = msg.context_tokens_used.map(|v| v as i64);
        let ctx_limit = msg.context_token_limit.map(|v| v as i64);
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO messages
             (uuid, session_id, role, timestamp, model,
              input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
              has_thinking, stop_reason, text_length, cwd, repo_id, provider,
              cost_cents, context_tokens_used, context_token_limit, interaction_mode,
              parent_uuid)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
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
                msg.has_thinking as i32,
                msg.stop_reason,
                msg.text_length as i64,
                msg.cwd,
                msg.repo_id,
                msg.provider,
                cost_cents,
                ctx_used,
                ctx_limit,
                msg.interaction_mode,
                msg.parent_uuid,
            ],
        )?;

        if inserted > 0 {
            count += 1;
            // Insert tool usage rows.
            for tool_name in &msg.tool_names {
                tx.execute(
                    "INSERT INTO tool_usage (message_uuid, tool_name) VALUES (?1, ?2)",
                    params![msg.uuid, tool_name],
                )?;
            }
            // Insert tags.
            if let Some(msg_tags) = tags.and_then(|t| t.get(i)) {
                for tag in msg_tags {
                    tx.execute(
                        "INSERT INTO tags (message_uuid, key, value) VALUES (?1, ?2, ?3)",
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
/// Clears the tags table and re-populates it.
pub fn backfill_tags(conn: &mut Connection) -> Result<usize> {
    let tags_config = crate::config::load_tags_config();
    let mut pipeline = crate::pipeline::Pipeline::default_pipeline(tags_config);

    // Read all messages with session fields needed for enrichment
    let messages: Vec<(String, crate::jsonl::ParsedMessage)> = {
        let mut stmt = conn.prepare(
            "SELECT m.uuid, m.session_id, m.timestamp, m.cwd, m.role, m.model,
                    m.input_tokens, m.output_tokens, m.cache_creation_tokens, m.cache_read_tokens,
                    m.provider, m.cost_cents, m.repo_id, m.parent_uuid,
                    s.git_branch, s.user_name, s.machine_name
             FROM messages m
             LEFT JOIN sessions s ON m.session_id = s.session_id",
        )?;
        stmt.query_map([], |row| {
            let uuid: String = row.get(0)?;
            Ok((
                uuid.clone(),
                crate::jsonl::ParsedMessage {
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
                    tool_names: vec![],
                    has_thinking: false,
                    stop_reason: None,
                    text_length: 0,
                    version: None,
                    git_branch: row.get(14)?,
                    repo_id: row.get(12)?,
                    provider: row
                        .get::<_, Option<String>>(10)?
                        .unwrap_or_else(|| "claude_code".to_string()),
                    cost_cents: row.get(11)?,
                    context_tokens_used: None,
                    context_token_limit: None,
                    interaction_mode: None,
                    session_title: None,
                    lines_added: None,
                    lines_removed: None,
                    parent_uuid: row.get(13)?,
                    user_name: row.get(15)?,
                    machine_name: row.get(16)?,
                },
            ))
        })?
        .filter_map(|r| r.ok())
        .collect()
    }; // stmt dropped here

    // Extract ParsedMessages for pipeline processing
    let mut parsed: Vec<crate::jsonl::ParsedMessage> =
        messages.into_iter().map(|(_, m)| m).collect();
    let all_tags = pipeline.process(&mut parsed);

    // Clear and re-insert tags
    let tx = conn.transaction()?;
    tx.execute_batch("DELETE FROM tags")?;
    let mut count = 0usize;
    for (i, msg) in parsed.iter().enumerate() {
        if let Some(msg_tags) = all_tags.get(i) {
            for tag in msg_tags {
                tx.execute(
                    "INSERT INTO tags (message_uuid, key, value) VALUES (?1, ?2, ?3)",
                    params![msg.uuid, tag.key, tag.value],
                )?;
                count += 1;
            }
        }
    }
    tx.commit()?;
    Ok(count)
}

/// Run a full incremental sync: discover JSONL files, parse new content, ingest.
/// Iterates all available providers, discovering and parsing their files.
/// Returns (files_synced, messages_ingested).
pub fn sync_all(conn: &mut Connection) -> Result<(usize, usize)> {
    let providers = crate::provider::available_providers();
    let tags_config = crate::config::load_tags_config();
    let mut pipeline = crate::pipeline::Pipeline::default_pipeline(tags_config);
    let mut total_files = 0;
    let mut total_messages = 0;

    for provider in &providers {
        // Try direct sync first (e.g. Cursor state.vscdb).
        if let Some(result) = provider.sync_direct(conn, &mut pipeline) {
            let (files, messages) = result?;
            total_files += files;
            total_messages += messages;
            continue;
        }

        let files = provider.discover_files()?;

        for discovered in &files {
            let file_path = &discovered.path;
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

    // Post-sync: enrich sessions with git commit data
    if total_messages > 0 {
        match crate::git::enrich_git_commits(conn) {
            Ok(n) if n > 0 => tracing::info!("Git enrichment: {} commits found", n),
            Err(e) => tracing::warn!("Git enrichment failed: {e}"),
            _ => {}
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
    let mut pipeline = crate::pipeline::Pipeline::default_pipeline(tags_config);
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

    // Post-sync: enrich sessions with git commit data
    if count > 0 {
        if let Err(e) = crate::git::enrich_git_commits(conn) {
            tracing::warn!("Git enrichment failed: {e}");
        }
    }

    Ok(count)
}

/// Summary statistics for display.
#[derive(Debug, Clone, serde::Serialize)]
pub struct UsageSummary {
    pub total_sessions: u64,
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
        "SELECT COUNT(DISTINCT session_id),
                COUNT(*),
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
        total_sessions,
        total_messages,
        total_user_messages,
        total_assistant_messages,
        total_input,
        total_output,
        total_cache_create,
        total_cache_read,
    ): (u64, u64, u64, u64, u64, u64, u64, u64) =
        conn.query_row(&sql, param_refs.as_slice(), |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
            ))
        })?;

    Ok(UsageSummary {
        total_sessions,
        total_messages,
        total_user_messages,
        total_assistant_messages,
        total_input_tokens: total_input,
        total_output_tokens: total_output,
        total_cache_creation_tokens: total_cache_create,
        total_cache_read_tokens: total_cache_read,
    })
}

/// Top tools by usage count, optionally filtered by date range.
pub fn top_tools(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Vec<(String, u64)>> {
    let (where_clause, date_params) = date_filter(since, until, "WHERE", 0);
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = date_params
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    let sql = if where_clause.is_empty() {
        "SELECT tool_name, COUNT(*) as cnt FROM tool_usage GROUP BY tool_name ORDER BY cnt DESC LIMIT 50".to_string()
    } else {
        format!(
            "SELECT tu.tool_name, COUNT(*) as cnt FROM tool_usage tu
             JOIN messages m ON tu.message_uuid = m.uuid
             {}
             GROUP BY tu.tool_name ORDER BY cnt DESC LIMIT 50",
            where_clause
        )
    };
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(param_refs.as_slice(), |row| Ok((row.get(0)?, row.get(1)?)))?
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

/// A session row with aggregated stats.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub project_dir: Option<String>,
    pub repo_id: Option<String>,
    pub first_seen: String,
    pub last_seen: String,
    pub message_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub provider: String,
    pub session_title: Option<String>,
    pub cost_cents: f64,
    pub git_branch: Option<String>,
    pub user_name: Option<String>,
    pub machine_name: Option<String>,
    pub commit_count: u64,
    pub git_author_name: Option<String>,
}

/// Paginated session list result.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PaginatedSessions {
    pub sessions: Vec<SessionSummary>,
    pub total_count: u64,
}

/// Parameters for paginated session queries.
pub struct SessionListParams<'a> {
    pub since: Option<&'a str>,
    pub until: Option<&'a str>,
    pub search: Option<&'a str>,
    pub sort_by: Option<&'a str>,
    pub sort_asc: bool,
    pub limit: usize,
    pub offset: usize,
}

/// List sessions with aggregated stats, with server-side search, sort, and pagination.
pub fn session_list(conn: &Connection, p: &SessionListParams) -> Result<PaginatedSessions> {
    // Build parameterized date filter for m.timestamp columns.
    let mut conditions = Vec::new();
    let mut param_values: Vec<String> = Vec::new();
    if let Some(s) = p.since {
        param_values.push(s.to_string());
        conditions.push(format!("m.timestamp >= ?{}", param_values.len()));
    }
    if let Some(u) = p.until {
        param_values.push(u.to_string());
        conditions.push(format!("m.timestamp < ?{}", param_values.len()));
    }
    // Search filter on session fields
    if let Some(q) = p.search
        && !q.is_empty()
    {
        param_values.push(format!("%{q}%"));
        let idx = param_values.len();
        conditions.push(format!(
            "(s.session_title LIKE ?{idx} OR s.session_id LIKE ?{idx} OR s.repo_id LIKE ?{idx} OR s.project_dir LIKE ?{idx} OR s.git_branch LIKE ?{idx} OR s.provider LIKE ?{idx} OR EXISTS (SELECT 1 FROM tags t JOIN messages tm ON t.message_uuid = tm.uuid WHERE tm.session_id = s.session_id AND t.value LIKE ?{idx}))"
        ));
    }
    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    // Whitelist sort columns to prevent SQL injection
    let order_col = match p.sort_by.unwrap_or("last_seen") {
        "session_id" => "s.session_id",
        "repo_id" => "s.repo_id",
        "git_branch" => "s.git_branch",
        "last_seen" => "s.last_seen",
        "duration" => "(julianday(s.last_seen) - julianday(s.first_seen))",
        "message_count" => "msg_count",
        "tokens" => "(inp + outp)",
        "cost" => "cost_sum",
        _ => "s.last_seen",
    };
    let order_dir = if p.sort_asc { "ASC" } else { "DESC" };

    // Count query — use same JOIN as data query for consistent results
    let count_sql = format!(
        "SELECT COUNT(DISTINCT s.session_id)
         FROM sessions s
         LEFT JOIN messages m ON m.session_id = s.session_id
         {}",
        where_clause
    );
    let total_count: u64 = conn.query_row(&count_sql, param_refs.as_slice(), |r| r.get(0))?;

    // Data query with pagination
    let sql = format!(
        "SELECT s.session_id, s.project_dir, s.first_seen, s.last_seen,
                COUNT(m.uuid) as msg_count,
                COALESCE(SUM(m.input_tokens), 0) as inp,
                COALESCE(SUM(m.output_tokens), 0) as outp,
                COALESCE(SUM(m.cache_creation_tokens), 0) as cache_c,
                COALESCE(SUM(m.cache_read_tokens), 0) as cache_r,
                s.repo_id,
                COALESCE(s.provider, 'claude_code'),
                s.session_title,
                COALESCE(SUM(m.cost_cents), 0.0) as cost_sum,
                s.git_branch,
                s.user_name,
                s.machine_name,
                (SELECT COUNT(*) FROM commits c WHERE c.session_id = s.session_id) as commit_count,
                s.git_author_name
         FROM sessions s
         LEFT JOIN messages m ON m.session_id = s.session_id
         {}
         GROUP BY s.session_id
         ORDER BY {} {}
         LIMIT {} OFFSET {}",
        where_clause, order_col, order_dir, p.limit, p.offset
    );

    let mut stmt = conn.prepare(&sql)?;
    let sessions = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(SessionSummary {
                session_id: row.get(0)?,
                project_dir: row.get(1)?,
                first_seen: row.get(2)?,
                last_seen: row.get(3)?,
                message_count: row.get(4)?,
                input_tokens: row.get(5)?,
                output_tokens: row.get(6)?,
                cache_creation_tokens: row.get(7)?,
                cache_read_tokens: row.get(8)?,
                repo_id: row.get(9)?,
                provider: row.get(10)?,
                session_title: row.get(11)?,
                cost_cents: row.get(12)?,
                git_branch: row.get(13)?,
                user_name: row.get(14)?,
                machine_name: row.get(15)?,
                commit_count: row.get(16)?,
                git_author_name: row.get(17)?,
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
    Ok(PaginatedSessions {
        sessions,
        total_count,
    })
}

/// Detailed stats for a single session.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionDetail {
    pub session_id: String,
    pub project_dir: Option<String>,
    pub repo_id: Option<String>,
    pub first_seen: String,
    pub last_seen: String,
    pub version: Option<String>,
    pub git_branch: Option<String>,
    pub user_messages: u64,
    pub assistant_messages: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub top_tools: Vec<(String, u64)>,
    pub provider: String,
    pub session_title: Option<String>,
    pub interaction_mode: Option<String>,
    pub lines_added: u64,
    pub lines_removed: u64,
    pub cost_cents: f64,
    pub git_author_name: Option<String>,
    pub git_author_email: Option<String>,
    pub commits: Vec<crate::git::GitCommit>,
}

/// Get detailed stats for a single session by ID (prefix match supported).
pub fn session_detail(conn: &Connection, session_id_prefix: &str) -> Result<Option<SessionDetail>> {
    // Find session by exact or prefix match.
    let session_row = conn
        .query_row(
            "SELECT session_id, project_dir, first_seen, last_seen, version, git_branch, repo_id,
                    COALESCE(provider, 'claude_code'), session_title, interaction_mode,
                    COALESCE(lines_added, 0), COALESCE(lines_removed, 0),
                    git_author_name, git_author_email
             FROM sessions WHERE session_id = ?1 OR session_id LIKE ?2
             ORDER BY last_seen DESC LIMIT 1",
            params![session_id_prefix, format!("{}%", session_id_prefix)],
            |row| {
                Ok(SessionDetail {
                    session_id: row.get(0)?,
                    project_dir: row.get(1)?,
                    first_seen: row.get(2)?,
                    last_seen: row.get(3)?,
                    version: row.get(4)?,
                    git_branch: row.get(5)?,
                    repo_id: row.get(6)?,
                    provider: row.get(7)?,
                    session_title: row.get(8)?,
                    interaction_mode: row.get(9)?,
                    lines_added: row.get(10)?,
                    lines_removed: row.get(11)?,
                    git_author_name: row.get(12)?,
                    git_author_email: row.get(13)?,
                    user_messages: 0,
                    assistant_messages: 0,
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_creation_tokens: 0,
                    cache_read_tokens: 0,
                    top_tools: vec![],
                    cost_cents: 0.0,
                    commits: vec![],
                })
            },
        )
        .optional()?;

    let Some(mut detail) = session_row else {
        return Ok(None);
    };
    let sid = &detail.session_id;

    conn.query_row(
        "SELECT
            COUNT(CASE WHEN role='user' THEN 1 END),
            COUNT(CASE WHEN role='assistant' THEN 1 END),
            COALESCE(SUM(input_tokens), 0),
            COALESCE(SUM(output_tokens), 0),
            COALESCE(SUM(cache_creation_tokens), 0),
            COALESCE(SUM(cache_read_tokens), 0),
            COALESCE(SUM(cost_cents), 0.0)
         FROM messages WHERE session_id = ?1",
        params![sid],
        |r| {
            detail.user_messages = r.get(0)?;
            detail.assistant_messages = r.get(1)?;
            detail.input_tokens = r.get(2)?;
            detail.output_tokens = r.get(3)?;
            detail.cache_creation_tokens = r.get(4)?;
            detail.cache_read_tokens = r.get(5)?;
            detail.cost_cents = r.get(6)?;
            Ok(())
        },
    )?;

    let mut stmt = conn.prepare(
        "SELECT tu.tool_name, COUNT(*) as cnt
         FROM tool_usage tu
         JOIN messages m ON tu.message_uuid = m.uuid
         WHERE m.session_id = ?1
         GROUP BY tu.tool_name ORDER BY cnt DESC LIMIT 10",
    )?;
    detail.top_tools = stmt
        .query_map(params![sid], |row| Ok((row.get(0)?, row.get(1)?)))
        .ok()
        .map(|rows| {
            rows.filter_map(|r| match r {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!("skipping row: {e}");
                    None
                }
            })
            .collect()
        })
        .unwrap_or_default();

    // Load git commits for this session
    detail.commits = crate::git::commits_for_session(conn, sid).unwrap_or_default();

    Ok(Some(detail))
}

/// Repository usage stats, grouped by repo_id.
#[derive(Debug, Clone, serde::Serialize)]
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
         HAVING (inp + outp) > 0
         ORDER BY (inp + outp) DESC
         LIMIT ?{}",
        conditions.join(" AND "),
        limit_idx
    );

    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        param_values.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
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

/// MCP tool usage breakdown: tools with `mcp__` prefix, grouped by server.
#[derive(Debug, Clone, serde::Serialize)]
pub struct McpToolStat {
    pub server: String,
    pub tool: String,
    pub call_count: u64,
}

pub fn mcp_tool_stats(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Vec<McpToolStat>> {
    let (where_clause, date_params) = date_filter(since, until, "WHERE", 0);

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = date_params
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    let sql = if where_clause.is_empty() {
        "SELECT tool_name, COUNT(*) as cnt FROM tool_usage WHERE tool_name LIKE 'mcp__%' GROUP BY tool_name ORDER BY cnt DESC".to_string()
    } else {
        format!(
            "SELECT tu.tool_name, COUNT(*) as cnt FROM tool_usage tu
             JOIN messages m ON tu.message_uuid = m.uuid
             {} AND tu.tool_name LIKE 'mcp__%%'
             GROUP BY tu.tool_name ORDER BY cnt DESC",
            where_clause
        )
    };
    let mut stmt = conn.prepare(&sql)?;

    let rows: Vec<(String, u64)> = stmt
        .query_map(param_refs.as_slice(), |row| Ok((row.get(0)?, row.get(1)?)))?
        .filter_map(|r| match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("skipping row: {e}");
                None
            }
        })
        .collect();

    let results = rows
        .into_iter()
        .map(|(tool_name, count)| {
            // Parse server from tool name: mcp__server__tool → server
            let server = tool_name
                .strip_prefix("mcp__")
                .and_then(|rest| rest.split("__").next())
                .unwrap_or("unknown")
                .to_string();
            McpToolStat {
                server,
                tool: tool_name,
                call_count: count,
            }
        })
        .collect();

    Ok(results)
}

/// Cache efficiency stats for a date range.
#[derive(Debug, Clone, serde::Serialize)]
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
#[derive(Debug, Clone, serde::Serialize)]
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

/// Feature cost breakdown: tokens and cost grouped by git branch.
#[derive(Debug, Clone, serde::Serialize)]
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

/// Query feature cost grouped by git branch, optionally filtered by date range.
pub fn branch_cost(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Vec<BranchCost>> {
    let (where_clause, date_params) = date_filter(since, until, "WHERE", 0);
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = date_params
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    let sql = format!(
        "SELECT s.git_branch,
                COALESCE(s.repo_id, '') as repo,
                COUNT(DISTINCT m.session_id) as sess,
                COUNT(*) as cnt,
                COALESCE(SUM(m.input_tokens), 0),
                COALESCE(SUM(m.output_tokens), 0),
                COALESCE(SUM(m.cache_read_tokens), 0),
                COALESCE(SUM(m.cache_creation_tokens), 0),
                COALESCE(SUM(m.cost_cents), 0.0)
         FROM messages m
         JOIN sessions s ON m.session_id = s.session_id
         {} {} s.git_branch IS NOT NULL AND s.git_branch != ''
         GROUP BY s.git_branch, s.repo_id
         ORDER BY COALESCE(SUM(m.cost_cents), 0.0) DESC
         LIMIT 20",
        where_clause,
        if where_clause.is_empty() {
            "WHERE"
        } else {
            "AND"
        }
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
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

/// Query cost for a single branch (matched case-insensitively, with or without refs/heads/ prefix).
pub fn branch_cost_single(
    conn: &Connection,
    branch: &str,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Option<BranchCost>> {
    let (where_clause, date_params) = date_filter(since, until, "WHERE", 0);
    let branch_stripped = branch.strip_prefix("refs/heads/").unwrap_or(branch);

    let mut param_values: Vec<String> = date_params;
    let branch_idx = param_values.len() + 1;
    param_values.push(format!("%{}", branch_stripped));

    let sql = format!(
        "SELECT s.git_branch,
                COALESCE(s.repo_id, '') as repo,
                COUNT(DISTINCT m.session_id) as sess,
                COUNT(*) as cnt,
                COALESCE(SUM(m.input_tokens), 0),
                COALESCE(SUM(m.output_tokens), 0),
                COALESCE(SUM(m.cache_read_tokens), 0),
                COALESCE(SUM(m.cache_creation_tokens), 0),
                COALESCE(SUM(m.cost_cents), 0.0)
         FROM messages m
         JOIN sessions s ON m.session_id = s.session_id
         {} {} (s.git_branch LIKE ?{} OR REPLACE(s.git_branch, 'refs/heads/', '') LIKE ?{})
         GROUP BY s.git_branch, s.repo_id
         ORDER BY COALESCE(SUM(m.cost_cents), 0.0) DESC
         LIMIT 1",
        where_clause,
        if where_clause.is_empty() {
            "WHERE"
        } else {
            "AND"
        },
        branch_idx,
        branch_idx,
    );

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    let mut stmt = conn.prepare(&sql)?;
    let result = stmt
        .query_row(param_refs.as_slice(), |row| {
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
        })
        .optional()?;
    Ok(result)
}

/// Tag-based cost breakdown: cost grouped by tag key+value.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TagCost {
    pub key: String,
    pub value: String,
    pub message_count: u64,
    pub cost_cents: f64,
}

/// Query cost breakdown by tag, optionally filtered by tag key and date range.
pub fn tag_stats(
    conn: &Connection,
    tag_key: Option<&str>,
    since: Option<&str>,
    until: Option<&str>,
    limit: usize,
) -> Result<Vec<TagCost>> {
    let mut conditions = Vec::new();
    let mut param_values: Vec<String> = Vec::new();

    if let Some(k) = tag_key {
        param_values.push(k.to_string());
        conditions.push(format!("t.key = ?{}", param_values.len()));
    }
    // Date filter uses session last_seen so that tickets with recent activity show up
    // even if the tagged (user) message was created earlier
    if let Some(s) = since {
        param_values.push(s.to_string());
        conditions.push(format!("s.last_seen >= ?{}", param_values.len()));
    }
    if let Some(u) = until {
        param_values.push(u.to_string());
        conditions.push(format!("s.last_seen < ?{}", param_values.len()));
    }

    param_values.push(limit.to_string());
    let limit_idx = param_values.len();

    // Tags like ticket_id/branch are attached to user messages (which have no cost).
    // Join through session to get the full session cost for each tagged session.
    // Date filter is on session last_seen so tickets active today appear even if
    // the tagged message was from days ago.
    let extra_conditions = if conditions.is_empty() {
        String::new()
    } else {
        format!("AND {}", conditions.join(" AND "))
    };

    let sql = format!(
        "SELECT ts.key, ts.value,
                COUNT(*) as session_count,
                COALESCE(SUM(ts.session_cost), 0.0) as total_cost_cents
         FROM (
             SELECT DISTINCT t.key, t.value, tm.session_id,
                    (SELECT COALESCE(SUM(sm.cost_cents), 0.0)
                     FROM messages sm WHERE sm.session_id = tm.session_id) as session_cost
             FROM tags t
             JOIN messages tm ON t.message_uuid = tm.uuid
             JOIN sessions s ON s.session_id = tm.session_id
             WHERE tm.session_id IS NOT NULL
             {}
         ) ts
         GROUP BY ts.key, ts.value
         ORDER BY total_cost_cents DESC
         LIMIT ?{}",
        extra_conditions, limit_idx
    );

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
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
    Ok(rows)
}

/// Model usage breakdown: tokens grouped by model name.
#[derive(Debug, Clone, serde::Serialize)]
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
                COALESCE(SUM(input_tokens), 0),
                COALESCE(SUM(output_tokens), 0),
                COALESCE(SUM(cache_read_tokens), 0),
                COALESCE(SUM(cache_creation_tokens), 0),
                COALESCE(SUM(cost_cents), 0.0)
         FROM messages
         {} {} role = 'assistant' AND model IS NOT NULL AND model != '' AND model NOT LIKE '<%'
         GROUP BY m, p
         ORDER BY (COALESCE(SUM(input_tokens), 0) + COALESCE(SUM(output_tokens), 0)) DESC",
        where_clause,
        if where_clause.is_empty() {
            "WHERE"
        } else {
            "AND"
        }
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
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
#[derive(Debug, Clone, serde::Serialize)]
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

    // Branch cost: total cost for sessions on a specific git branch
    let branch_cost = params.branch.as_ref().map(|branch| {
        conn.query_row(
            "SELECT COALESCE(SUM(m.cost_cents), 0.0) \
             FROM messages m JOIN sessions s ON m.session_id = s.session_id \
             WHERE s.git_branch = ?1",
            [branch],
            |r| r.get::<_, f64>(0),
        )
        .unwrap_or(0.0)
            / 100.0
    });

    // Project cost: total cost for sessions in a specific project directory
    let project_cost = params.project_dir.as_ref().map(|dir| {
        conn.query_row(
            "SELECT COALESCE(SUM(m.cost_cents), 0.0) \
             FROM messages m JOIN sessions s ON m.session_id = s.session_id \
             WHERE s.project_dir = ?1",
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
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProviderStats {
    pub provider: String,
    pub display_name: String,
    pub session_count: u64,
    pub message_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub estimated_cost: f64,
    pub total_cost_cents: f64,
    pub total_lines_added: u64,
    pub total_lines_removed: u64,
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
                COUNT(DISTINCT session_id) as sess,
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
                row.get::<_, u64>(6)?,
                row.get::<_, f64>(7)?,
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
    // Build a date filter with param_start=1 for sub-queries where ?1 is the provider
    let (sub_where, sub_date_params) = date_filter(since, until, "AND", 1);
    let sub_where_sessions = sub_where.replace("timestamp", "last_seen");
    let mut result = Vec::new();

    for (prov, sessions, messages, input, output, cache_create, cache_read, sum_cost_cents) in rows
    {
        let display_name = providers
            .iter()
            .find(|p| p.name() == prov)
            .map(|p| p.display_name().to_string())
            .unwrap_or_else(|| prov.clone());

        // Cost is baked into cost_cents at ingest time — just use the sum.
        // sum_cost_cents is in cents; estimated_cost is in dollars.
        let estimated_cost = (sum_cost_cents / 100.0 * 100.0).round() / 100.0;

        // Query lines from sessions for this provider
        let lines_sql = format!(
            "SELECT COALESCE(SUM(lines_added), 0), COALESCE(SUM(lines_removed), 0)
             FROM sessions WHERE COALESCE(provider, 'claude_code') = ?1 {}",
            sub_where_sessions
        );
        let mut lines_params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(prov.clone())];
        for p in &sub_date_params {
            lines_params.push(Box::new(p.clone()));
        }
        let lines_refs: Vec<&dyn rusqlite::types::ToSql> =
            lines_params.iter().map(|b| b.as_ref()).collect();

        let (lines_added, lines_removed) = conn
            .query_row(&lines_sql, lines_refs.as_slice(), |row| {
                Ok((row.get::<_, u64>(0)?, row.get::<_, u64>(1)?))
            })
            .unwrap_or((0, 0));

        result.push(ProviderStats {
            provider: prov,
            display_name,
            session_count: sessions,
            message_count: messages,
            input_tokens: input,
            output_tokens: output,
            cache_creation_tokens: cache_create,
            cache_read_tokens: cache_read,
            estimated_cost,
            total_cost_cents: (sum_cost_cents * 100.0).round() / 100.0,
            total_lines_added: lines_added,
            total_lines_removed: lines_removed,
        });
    }

    Ok(result)
}

/// Interaction mode breakdown: count of sessions by mode.
pub fn interaction_mode_breakdown(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Vec<(String, u64)>> {
    let (where_clause, date_params) = date_filter(since, until, "WHERE", 0);
    let extra = if where_clause.is_empty() {
        "WHERE interaction_mode IS NOT NULL".to_string()
    } else {
        format!("{} AND interaction_mode IS NOT NULL", where_clause)
    };
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = date_params
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    let sql = format!(
        "SELECT interaction_mode, COUNT(DISTINCT session_id) FROM sessions {} GROUP BY interaction_mode ORDER BY COUNT(DISTINCT session_id) DESC",
        extra.replace("timestamp", "last_seen")
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
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
    Ok(rows)
}

/// Context window utilization stats.
#[derive(Debug, Clone, serde::Serialize)]
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
        "SELECT COUNT(DISTINCT session_id),
                COUNT(*),
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
        total_sessions,
        total_messages,
        total_user_messages,
        total_assistant_messages,
        total_input,
        total_output,
        total_cache_create,
        total_cache_read,
    ): (u64, u64, u64, u64, u64, u64, u64, u64) =
        conn.query_row(&sql, param_refs.as_slice(), |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
            ))
        })?;

    Ok(UsageSummary {
        total_sessions,
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
                tool_names: vec![],
                has_thinking: false,
                stop_reason: None,
                text_length: 20,
                version: Some("2.1.76".to_string()),
                git_branch: Some("main".to_string()),
                repo_id: None,
                provider: "claude_code".to_string(),
                cost_cents: None,
                context_tokens_used: None,
                context_token_limit: None,
                interaction_mode: None,
                session_title: None,
                lines_added: None,
                lines_removed: None,
                parent_uuid: None,
                user_name: None,
                machine_name: None,
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
                tool_names: vec!["Read".to_string(), "Edit".to_string()],
                has_thinking: true,
                stop_reason: Some("end_turn".to_string()),
                text_length: 150,
                version: None,
                git_branch: None,
                repo_id: None,
                provider: "claude_code".to_string(),
                cost_cents: None,
                context_tokens_used: None,
                context_token_limit: None,
                interaction_mode: None,
                session_title: None,
                lines_added: None,
                lines_removed: None,
                parent_uuid: None,
                user_name: None,
                machine_name: None,
            },
        ];

        let count = ingest_messages(&mut conn, &msgs, None).unwrap();
        assert_eq!(count, 2);

        // Duplicate insert should be skipped.
        let count2 = ingest_messages(&mut conn, &msgs, None).unwrap();
        assert_eq!(count2, 0);

        let summary = usage_summary(&conn, None, None).unwrap();
        assert_eq!(summary.total_sessions, 1);
        assert_eq!(summary.total_messages, 2);
        assert_eq!(summary.total_user_messages, 1);
        assert_eq!(summary.total_assistant_messages, 1);
        assert_eq!(summary.total_input_tokens, 100);
        assert_eq!(summary.total_output_tokens, 50);
        // top_tools is now a separate function
        let tools = top_tools(&conn, None, None).unwrap();
        assert_eq!(tools.len(), 2);
    }

    #[test]
    fn cost_cents_baked_at_ingest() {
        let mut conn = test_db();
        let msg = ParsedMessage {
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
            tool_names: vec![],
            has_thinking: false,
            stop_reason: None,
            text_length: 0,
            version: None,
            git_branch: None,
            repo_id: None,
            provider: "claude_code".to_string(),
            cost_cents: None, // Should be calculated at ingest
            context_tokens_used: None,
            context_token_limit: None,
            interaction_mode: None,
            session_title: None,
            lines_added: None,
            lines_removed: None,
            parent_uuid: None,
            user_name: None,
            machine_name: None,
        };
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
    fn session_upsert_updates_last_seen() {
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
                tool_names: vec![],
                has_thinking: false,
                stop_reason: None,
                text_length: 5,
                version: Some("2.1.0".to_string()),
                git_branch: Some("main".to_string()),
                repo_id: None,
                provider: "claude_code".to_string(),
                cost_cents: None,
                context_tokens_used: None,
                context_token_limit: None,
                interaction_mode: None,
                session_title: None,
                lines_added: None,
                lines_removed: None,
                parent_uuid: None,
                user_name: None,
                machine_name: None,
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
                tool_names: vec![],
                has_thinking: false,
                stop_reason: None,
                text_length: 5,
                version: None,
                git_branch: None,
                repo_id: None,
                provider: "claude_code".to_string(),
                cost_cents: None,
                context_tokens_used: None,
                context_token_limit: None,
                interaction_mode: None,
                session_title: None,
                lines_added: None,
                lines_removed: None,
                parent_uuid: None,
                user_name: None,
                machine_name: None,
            },
        ];
        ingest_messages(&mut conn, &msgs, None).unwrap();

        let last_seen: String = conn
            .query_row(
                "SELECT last_seen FROM sessions WHERE session_id = 's1'",
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
                tool_names: vec![],
                has_thinking: false,
                stop_reason: None,
                text_length: 20,
                version: Some("2.1.76".to_string()),
                git_branch: Some("main".to_string()),
                repo_id: None,
                provider: "claude_code".to_string(),
                cost_cents: None,
                context_tokens_used: None,
                context_token_limit: None,
                interaction_mode: None,
                session_title: None,
                lines_added: None,
                lines_removed: None,
                parent_uuid: None,
                user_name: None,
                machine_name: None,
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
                tool_names: vec!["Read".to_string(), "Edit".to_string()],
                has_thinking: true,
                stop_reason: Some("end_turn".to_string()),
                text_length: 150,
                version: None,
                git_branch: None,
                repo_id: None,
                provider: "claude_code".to_string(),
                cost_cents: None,
                context_tokens_used: None,
                context_token_limit: None,
                interaction_mode: None,
                session_title: None,
                lines_added: None,
                lines_removed: None,
                parent_uuid: None,
                user_name: None,
                machine_name: None,
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
                tool_names: vec![],
                has_thinking: false,
                stop_reason: None,
                text_length: 10,
                version: None,
                git_branch: None,
                repo_id: None,
                provider: "claude_code".to_string(),
                cost_cents: None,
                context_tokens_used: None,
                context_token_limit: None,
                interaction_mode: None,
                session_title: None,
                lines_added: None,
                lines_removed: None,
                parent_uuid: None,
                user_name: None,
                machine_name: None,
            },
        ]
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
        assert_eq!(result.sessions.len(), 2);
        assert_eq!(result.total_count, 2);
        // Most recent first.
        assert_eq!(result.sessions[0].session_id, "sess-def");
        assert_eq!(result.sessions[1].session_id, "sess-abc");
        assert_eq!(result.sessions[1].input_tokens, 100);
    }

    #[test]
    fn session_detail_exact_and_prefix() {
        let mut conn = test_db();
        ingest_messages(&mut conn, &sample_messages(), None).unwrap();

        // Exact match.
        let d = session_detail(&conn, "sess-abc").unwrap().unwrap();
        assert_eq!(d.session_id, "sess-abc");
        assert_eq!(d.user_messages, 1);
        assert_eq!(d.assistant_messages, 1);
        assert_eq!(d.input_tokens, 100);
        assert_eq!(d.top_tools.len(), 2);
        assert_eq!(d.version.as_deref(), Some("2.1.76"));

        // Prefix match.
        let d2 = session_detail(&conn, "sess-a").unwrap().unwrap();
        assert_eq!(d2.session_id, "sess-abc");

        // No match.
        assert!(session_detail(&conn, "nonexistent").unwrap().is_none());
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
                tool_names: vec!["Grep".to_string(), "Glob".to_string(), "Read".to_string()],
                has_thinking: false,
                stop_reason: Some("end_turn".to_string()),
                text_length: 50,
                version: None,
                git_branch: None,
                repo_id: None,
                provider: "claude_code".to_string(),
                cost_cents: None,
                context_tokens_used: None,
                context_token_limit: None,
                interaction_mode: None,
                session_title: None,
                lines_added: None,
                lines_removed: None,
                parent_uuid: None,
                user_name: None,
                machine_name: None,
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
                tool_names: vec![
                    "Edit".to_string(),
                    "mcp__context7__query-docs".to_string(),
                    "mcp__linear__get-issue".to_string(),
                ],
                has_thinking: false,
                stop_reason: Some("end_turn".to_string()),
                text_length: 80,
                version: None,
                git_branch: None,
                repo_id: None,
                provider: "claude_code".to_string(),
                cost_cents: None,
                context_tokens_used: None,
                context_token_limit: None,
                interaction_mode: None,
                session_title: None,
                lines_added: None,
                lines_removed: None,
                parent_uuid: None,
                user_name: None,
                machine_name: None,
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
                tool_names: vec!["Read".to_string()],
                has_thinking: false,
                stop_reason: Some("end_turn".to_string()),
                text_length: 20,
                version: None,
                git_branch: None,
                repo_id: None,
                provider: "claude_code".to_string(),
                cost_cents: None,
                context_tokens_used: None,
                context_token_limit: None,
                interaction_mode: None,
                session_title: None,
                lines_added: None,
                lines_removed: None,
                parent_uuid: None,
                user_name: None,
                machine_name: None,
            },
        ]
    }

    #[test]
    fn mcp_tool_stats_groups_by_server() {
        let mut conn = test_db();
        ingest_messages(&mut conn, &messages_with_tools(), None).unwrap();

        let mcp = mcp_tool_stats(&conn, None, None).unwrap();
        assert_eq!(mcp.len(), 2);
        // Both have count 1.
        let servers: Vec<&str> = mcp.iter().map(|m| m.server.as_str()).collect();
        assert!(servers.contains(&"context7"));
        assert!(servers.contains(&"linear"));
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
                tool_names: vec![],
                has_thinking: false,
                stop_reason: None,
                text_length: 10,
                version: None,
                git_branch: None,
                repo_id: None,
                provider: "claude_code".to_string(),
                cost_cents: None,
                context_tokens_used: None,
                context_token_limit: None,
                interaction_mode: None,
                session_title: None,
                lines_added: None,
                lines_removed: None,
                parent_uuid: None,
                user_name: None,
                machine_name: None,
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
                tool_names: vec!["Read".to_string()],
                has_thinking: false,
                stop_reason: Some("end_turn".to_string()),
                text_length: 50,
                version: None,
                git_branch: None,
                repo_id: None,
                provider: "claude_code".to_string(),
                cost_cents: None,
                context_tokens_used: None,
                context_token_limit: None,
                interaction_mode: None,
                session_title: None,
                lines_added: None,
                lines_removed: None,
                parent_uuid: None,
                user_name: None,
                machine_name: None,
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
                tool_names: vec![],
                has_thinking: false,
                stop_reason: None,
                text_length: 15,
                version: None,
                git_branch: None,
                repo_id: None,
                provider: "cursor".to_string(),
                cost_cents: None,
                context_tokens_used: None,
                context_token_limit: None,
                interaction_mode: None,
                session_title: None,
                lines_added: None,
                lines_removed: None,
                parent_uuid: None,
                user_name: None,
                machine_name: None,
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
                tool_names: vec!["edit_file".to_string()],
                has_thinking: false,
                stop_reason: Some("end_turn".to_string()),
                text_length: 80,
                version: None,
                git_branch: None,
                repo_id: None,
                provider: "cursor".to_string(),
                cost_cents: None,
                context_tokens_used: None,
                context_token_limit: None,
                interaction_mode: None,
                session_title: None,
                lines_added: None,
                lines_removed: None,
                parent_uuid: None,
                user_name: None,
                machine_name: None,
            },
        ];

        ingest_messages(&mut conn, &claude_msgs, None).unwrap();
        ingest_messages(&mut conn, &cursor_msgs, None).unwrap();

        // All providers: should see 4 messages, 2 sessions
        let all = usage_summary(&conn, None, None).unwrap();
        assert_eq!(all.total_messages, 4);
        assert_eq!(all.total_sessions, 2);
        assert_eq!(all.total_input_tokens, 3000); // 1000 + 2000
        assert_eq!(all.total_output_tokens, 1300); // 500 + 800

        // Filter by claude_code: 2 messages, 1 session
        let cc = usage_summary_filtered(&conn, None, None, Some("claude_code")).unwrap();
        assert_eq!(cc.total_messages, 2);
        assert_eq!(cc.total_sessions, 1);
        assert_eq!(cc.total_input_tokens, 1000);
        assert_eq!(cc.total_output_tokens, 500);

        // Filter by cursor: 2 messages, 1 session
        let cu = usage_summary_filtered(&conn, None, None, Some("cursor")).unwrap();
        assert_eq!(cu.total_messages, 2);
        assert_eq!(cu.total_sessions, 1);
        assert_eq!(cu.total_input_tokens, 2000);
        assert_eq!(cu.total_output_tokens, 800);

        // Provider stats
        let pstats = provider_stats(&conn, None, None).unwrap();
        assert_eq!(pstats.len(), 2);
        let cc_stats = pstats.iter().find(|p| p.provider == "claude_code").unwrap();
        let cu_stats = pstats.iter().find(|p| p.provider == "cursor").unwrap();
        assert_eq!(cc_stats.session_count, 1);
        assert_eq!(cc_stats.message_count, 2);
        assert_eq!(cu_stats.session_count, 1);
        assert_eq!(cu_stats.message_count, 2);

        // Claude Code is registered, so it gets proper display name and cost.
        assert_eq!(cc_stats.display_name, "Claude Code");
        assert!(cc_stats.estimated_cost > 0.0);
    }
}
