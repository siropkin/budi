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

const SCHEMA_VERSION: u32 = 6;

/// Open (or create) the analytics database at the given path.
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
    migrate(&conn)?;
    Ok(conn)
}

/// Run schema migration. Exposed for cross-module test helpers.
#[doc(hidden)]
pub fn migrate_for_test(conn: &Connection) {
    migrate(conn).expect("migration failed");
}

fn migrate(conn: &Connection) -> Result<()> {
    let version: u32 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap_or(0);

    if version < 1 {
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS sessions (
                session_id   TEXT PRIMARY KEY,
                project_dir  TEXT,
                first_seen   TEXT NOT NULL,
                last_seen    TEXT NOT NULL,
                version      TEXT,
                git_branch   TEXT
            );

            CREATE TABLE IF NOT EXISTS messages (
                uuid                   TEXT PRIMARY KEY,
                session_id             TEXT,
                role                   TEXT NOT NULL,
                timestamp              TEXT NOT NULL,
                model                  TEXT,
                input_tokens           INTEGER NOT NULL DEFAULT 0,
                output_tokens          INTEGER NOT NULL DEFAULT 0,
                cache_creation_tokens  INTEGER NOT NULL DEFAULT 0,
                cache_read_tokens      INTEGER NOT NULL DEFAULT 0,
                has_thinking           INTEGER NOT NULL DEFAULT 0,
                stop_reason            TEXT,
                text_length            INTEGER NOT NULL DEFAULT 0,
                cwd                    TEXT,
                FOREIGN KEY (session_id) REFERENCES sessions(session_id)
            );

            CREATE TABLE IF NOT EXISTS tool_usage (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                message_uuid TEXT NOT NULL,
                tool_name    TEXT NOT NULL,
                FOREIGN KEY (message_uuid) REFERENCES messages(uuid)
            );

            CREATE TABLE IF NOT EXISTS sync_state (
                file_path    TEXT PRIMARY KEY,
                byte_offset  INTEGER NOT NULL DEFAULT 0,
                last_synced  TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id);
            CREATE INDEX IF NOT EXISTS idx_messages_timestamp ON messages(timestamp);
            CREATE INDEX IF NOT EXISTS idx_tool_usage_message ON tool_usage(message_uuid);
            CREATE INDEX IF NOT EXISTS idx_tool_usage_name ON tool_usage(tool_name);
            ",
        )?;
    }

    if version < 2 {
        conn.execute_batch(
            "
            ALTER TABLE sessions ADD COLUMN repo_id TEXT;
            ALTER TABLE messages ADD COLUMN repo_id TEXT;
            CREATE INDEX IF NOT EXISTS idx_sessions_repo ON sessions(repo_id);
            CREATE INDEX IF NOT EXISTS idx_messages_repo ON messages(repo_id);
            ",
        )?;
    }

    if version < 3 {
        conn.execute_batch(
            "
            ALTER TABLE sessions ADD COLUMN provider TEXT DEFAULT 'claude_code';
            ALTER TABLE messages ADD COLUMN provider TEXT DEFAULT 'claude_code';
            CREATE INDEX IF NOT EXISTS idx_sessions_provider ON sessions(provider);
            CREATE INDEX IF NOT EXISTS idx_messages_provider ON messages(provider);
            ",
        )?;
    }

    if version < 4 {
        conn.execute_batch(
            "
            ALTER TABLE sessions ADD COLUMN session_title TEXT;
            ALTER TABLE sessions ADD COLUMN interaction_mode TEXT;
            ALTER TABLE sessions ADD COLUMN lines_added INTEGER DEFAULT 0;
            ALTER TABLE sessions ADD COLUMN lines_removed INTEGER DEFAULT 0;

            ALTER TABLE messages ADD COLUMN cost_cents REAL;
            ALTER TABLE messages ADD COLUMN context_tokens_used INTEGER;
            ALTER TABLE messages ADD COLUMN context_token_limit INTEGER;
            ALTER TABLE messages ADD COLUMN interaction_mode TEXT;
            ",
        )?;
    }

    if version < 5 {
        // Backfill cost_cents for existing messages that don't have it.
        // This bakes in the estimated cost at current pricing rates.
        backfill_cost_cents(conn)?;
    }

    if version < 6 {
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_sessions_title ON sessions(session_title);
             CREATE INDEX IF NOT EXISTS idx_messages_session_ts ON messages(session_id, timestamp);",
        )?;
    }

    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;

    Ok(())
}

/// Backfill cost_cents for messages where it's NULL, using provider pricing.
fn backfill_cost_cents(conn: &Connection) -> Result<()> {
    conn.execute_batch("BEGIN")?;
    let result = (|| -> Result<()> {
        let mut stmt = conn.prepare(
            "SELECT uuid, COALESCE(provider, 'claude_code'), COALESCE(model, 'unknown'),
                    input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens
             FROM messages WHERE cost_cents IS NULL AND role = 'assistant'",
        )?;
        let rows: Vec<(String, String, String, u64, u64, u64, u64)> = stmt
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
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

        let mut update_stmt =
            conn.prepare("UPDATE messages SET cost_cents = ?1 WHERE uuid = ?2")?;
        for (uuid, provider, model, inp, outp, cw, cr) in &rows {
            let cost = estimate_cost_for_provider(provider, model, *inp, *outp, *cw, *cr);
            if cost > 0.0 {
                let cents = (cost * 100.0 * 100.0).round() / 100.0;
                update_stmt.execute(params![cents, uuid])?;
            }
        }
        Ok(())
    })();
    match result {
        Ok(()) => conn.execute_batch("COMMIT")?,
        Err(ref _e) => {
            let _ = conn.execute_batch("ROLLBACK");
        }
    }
    result
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
) -> Result<()> {
    let ts = timestamp.to_rfc3339();
    let la = lines_added.map(|v| v as i64);
    let lr = lines_removed.map(|v| v as i64);
    conn.execute(
        "INSERT INTO sessions (session_id, project_dir, first_seen, last_seen, version, git_branch, repo_id, provider, session_title, interaction_mode, lines_added, lines_removed)
         VALUES (?1, ?2, ?3, ?3, ?4, ?5, ?6, ?7, ?8, ?9, COALESCE(?10, 0), COALESCE(?11, 0))
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
           lines_removed = MAX(sessions.lines_removed, COALESCE(?11, 0))",
        params![session_id, cwd, ts, version, git_branch, repo_id, provider, session_title, interaction_mode, la, lr],
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

/// Ingest a batch of parsed messages into the database.
pub fn ingest_messages(conn: &mut Connection, messages: &[ParsedMessage]) -> Result<usize> {
    let tx = conn.transaction()?;
    let mut count = 0;

    for msg in messages {
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
              cost_cents, context_tokens_used, context_token_limit, interaction_mode)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
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

/// Run a full incremental sync: discover JSONL files, parse new content, ingest.
/// Iterates all available providers, discovering and parsing their files.
/// Returns (files_synced, messages_ingested).
pub fn sync_all(conn: &mut Connection) -> Result<(usize, usize)> {
    let providers = crate::provider::available_providers();
    let mut total_files = 0;
    let mut total_messages = 0;
    let mut repo_cache = crate::repo_id::RepoIdCache::new();

    for provider in &providers {
        // Try direct sync first (e.g. Cursor state.vscdb).
        if let Some(result) = provider.sync_direct(conn) {
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

            // Resolve repo_id for each message from its cwd.
            for msg in &mut messages {
                if let Some(ref cwd) = msg.cwd {
                    msg.repo_id = Some(repo_cache.resolve(Path::new(cwd)));
                }
            }

            let count = ingest_messages(conn, &messages)?;
            set_sync_offset(conn, &path_str, new_offset)?;

            if count > 0 {
                total_files += 1;
                total_messages += count;
            }
        }
    }

    Ok((total_files, total_messages))
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
            "(s.session_title LIKE ?{idx} OR s.session_id LIKE ?{idx} OR s.repo_id LIKE ?{idx} OR s.project_dir LIKE ?{idx})"
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
                COALESCE(SUM(m.cost_cents), 0.0) as cost_sum
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
}

/// Get detailed stats for a single session by ID (prefix match supported).
pub fn session_detail(conn: &Connection, session_id_prefix: &str) -> Result<Option<SessionDetail>> {
    // Find session by exact or prefix match.
    let session_row = conn
        .query_row(
            "SELECT session_id, project_dir, first_seen, last_seen, version, git_branch, repo_id,
                    COALESCE(provider, 'claude_code'), session_title, interaction_mode,
                    COALESCE(lines_added, 0), COALESCE(lines_removed, 0)
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
                    user_messages: 0,
                    assistant_messages: 0,
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_creation_tokens: 0,
                    cache_read_tokens: 0,
                    top_tools: vec![],
                    cost_cents: 0.0,
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
                COALESCE(SUM(output_tokens), 0) as outp
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

/// Search tool usage stats: count of search tools vs total tool calls.
/// Search tools are: Grep, Glob (the primary search tools in Claude Code).
#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchToolStats {
    pub search_calls: u64,
    pub total_calls: u64,
    pub ratio: f64,
}

pub fn search_tool_stats(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<SearchToolStats> {
    let (where_clause, date_params) = date_filter(since, until, "WHERE", 0);

    let tool_where = if where_clause.is_empty() {
        String::new()
    } else {
        format!(
            "WHERE message_uuid IN (SELECT uuid FROM messages {})",
            where_clause
        )
    };
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = date_params
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    let total_calls: u64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM tool_usage {}", tool_where),
        param_refs.as_slice(),
        |r| r.get(0),
    )?;

    let search_where = if tool_where.is_empty() {
        "WHERE tool_name IN ('Grep', 'Glob')".to_string()
    } else {
        format!("{} AND tool_name IN ('Grep', 'Glob')", tool_where)
    };
    let search_calls: u64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM tool_usage {}", search_where),
        param_refs.as_slice(),
        |r| r.get(0),
    )?;

    let ratio = if total_calls > 0 {
        search_calls as f64 / total_calls as f64
    } else {
        0.0
    };

    Ok(SearchToolStats {
        search_calls,
        total_calls,
        ratio,
    })
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

/// Sessions with disproportionately high input tokens relative to output.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TokenHeavySession {
    pub session_id: String,
    pub project_dir: Option<String>,
    pub repo_id: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub ratio: f64,
}

pub fn token_heavy_sessions(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    threshold: f64,
) -> Result<Vec<TokenHeavySession>> {
    let mut conditions = Vec::new();
    let mut param_values: Vec<String> = Vec::new();
    if let Some(s) = since {
        param_values.push(s.to_string());
        conditions.push(format!("m.timestamp >= ?{}", param_values.len()));
    }
    if let Some(u) = until {
        param_values.push(u.to_string());
        conditions.push(format!("m.timestamp < ?{}", param_values.len()));
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

    let sql = format!(
        "SELECT s.session_id, s.project_dir, s.repo_id,
                COALESCE(SUM(m.input_tokens), 0) as inp,
                COALESCE(SUM(m.output_tokens), 0) as outp
         FROM sessions s
         LEFT JOIN messages m ON m.session_id = s.session_id
         {}
         GROUP BY s.session_id
         HAVING outp > 0 AND CAST(inp AS REAL) / CAST(outp AS REAL) > {}
         ORDER BY inp DESC
         LIMIT 10",
        where_clause, threshold
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            let inp: u64 = row.get(3)?;
            let outp: u64 = row.get(4)?;
            Ok(TokenHeavySession {
                session_id: row.get(0)?,
                project_dir: row.get(1)?,
                repo_id: row.get(2)?,
                input_tokens: inp,
                output_tokens: outp,
                ratio: inp as f64 / outp as f64,
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

/// Get distinct repo IDs from sessions table.
pub fn repo_ids(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT repo_id FROM sessions WHERE repo_id IS NOT NULL ORDER BY repo_id",
    )?;
    let rows = stmt
        .query_map([], |row| row.get(0))?
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

/// Get distinct project directories from sessions table (for filesystem scanning).
pub fn project_dirs(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT project_dir FROM sessions WHERE project_dir IS NOT NULL ORDER BY project_dir",
    )?;
    let rows = stmt
        .query_map([], |row| row.get(0))?
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
                COALESCE(SUM(output_tokens), 0)
         FROM messages {}
         GROUP BY bucket ORDER BY bucket",
        group_expr, where_clause
    );

    let mut stmt = conn.prepare(&sql)?;
    let msg_rows: Vec<(String, u64, u64, u64)> = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
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
        .map(|(label, count, inp, outp)| ActivityBucket {
            tool_call_count: tool_rows.get(&label).copied().unwrap_or(0),
            label,
            message_count: count,
            input_tokens: inp,
            output_tokens: outp,
        })
        .collect();

    Ok(results)
}

/// Model usage breakdown: tokens grouped by model name.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ModelUsage {
    pub model: String,
    pub message_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
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
                COUNT(*) as cnt,
                COALESCE(SUM(input_tokens), 0),
                COALESCE(SUM(output_tokens), 0),
                COALESCE(SUM(cache_read_tokens), 0),
                COALESCE(SUM(cache_creation_tokens), 0)
         FROM messages
         {} {} role = 'assistant' AND model IS NOT NULL AND model != '' AND model NOT LIKE '<%'
         GROUP BY m
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
                message_count: row.get(1)?,
                input_tokens: row.get(2)?,
                output_tokens: row.get(3)?,
                cache_read_tokens: row.get(4)?,
                cache_creation_tokens: row.get(5)?,
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

/// Scan project directories for configuration files (CLAUDE.md, .claude/settings.json, etc.)
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConfigFileInfo {
    pub path: String,
    pub project: String,
    pub file_type: String,
    pub size_bytes: u64,
    pub est_tokens: u64,
}

/// Discover configuration files across all known project directories.
pub fn config_files(conn: &Connection) -> Result<Vec<ConfigFileInfo>> {
    let dirs = project_dirs(conn)?;
    let mut results = Vec::new();

    // Scan global ~/.claude/ directory first
    if let Ok(home_str) = std::env::var("HOME") {
        let home = PathBuf::from(home_str);
        let global_claude = home.join(".claude");
        let global_targets: &[(&str, &str)] = &[
            ("settings.json", "settings"),
            ("settings.local.json", "settings-local"),
            ("CLAUDE.md", "claude-md"),
        ];
        for &(file, file_type) in global_targets {
            let path = global_claude.join(file);
            if let Ok(metadata) = std::fs::metadata(&path) {
                let size = metadata.len();
                results.push(ConfigFileInfo {
                    path: path.display().to_string(),
                    project: "(global)".to_string(),
                    file_type: file_type.to_string(),
                    size_bytes: size,
                    est_tokens: size / 4,
                });
            }
        }
        // Global rules
        let global_rules = global_claude.join("rules");
        if let Ok(entries) = std::fs::read_dir(&global_rules) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "mdc" || e == "md")
                    && let Ok(metadata) = std::fs::metadata(&path)
                {
                    let size = metadata.len();
                    results.push(ConfigFileInfo {
                        path: path.display().to_string(),
                        project: "(global)".to_string(),
                        file_type: "rule".to_string(),
                        size_bytes: size,
                        est_tokens: size / 4,
                    });
                }
            }
        }
        // Global skills
        let global_skills = global_claude.join("skills");
        if let Ok(entries) = std::fs::read_dir(&global_skills) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "md")
                    && let Ok(metadata) = std::fs::metadata(&path)
                {
                    let size = metadata.len();
                    results.push(ConfigFileInfo {
                        path: path.display().to_string(),
                        project: "(global)".to_string(),
                        file_type: "skill".to_string(),
                        size_bytes: size,
                        est_tokens: size / 4,
                    });
                }
            }
        }
    }

    let scan_targets: &[(&str, &str)] = &[
        ("CLAUDE.md", "claude-md"),
        (".claude/settings.json", "settings"),
        (".claude/settings.local.json", "settings-local"),
    ];

    for dir in &dirs {
        let base = std::path::Path::new(dir);
        let proj = dir.clone();
        for &(file, file_type) in scan_targets {
            let path = base.join(file);
            if let Ok(metadata) = std::fs::metadata(&path) {
                let size = metadata.len();
                results.push(ConfigFileInfo {
                    path: path.display().to_string(),
                    project: proj.clone(),
                    file_type: file_type.to_string(),
                    size_bytes: size,
                    est_tokens: size / 4,
                });
            }
        }

        // Scan for .mdc/.md files in .claude/rules/ directory
        let rules_dir = base.join(".claude").join("rules");
        if let Ok(entries) = std::fs::read_dir(&rules_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "mdc" || e == "md")
                    && let Ok(metadata) = std::fs::metadata(&path)
                {
                    let size = metadata.len();
                    results.push(ConfigFileInfo {
                        path: path.display().to_string(),
                        project: proj.clone(),
                        file_type: "rule".to_string(),
                        size_bytes: size,
                        est_tokens: size / 4,
                    });
                }
            }
        }

        // Scan for skills in .claude/skills/ directory
        let skills_dir = base.join(".claude").join("skills");
        if let Ok(entries) = std::fs::read_dir(&skills_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "md")
                    && let Ok(metadata) = std::fs::metadata(&path)
                {
                    let size = metadata.len();
                    results.push(ConfigFileInfo {
                        path: path.display().to_string(),
                        project: proj.clone(),
                        file_type: "skill".to_string(),
                        size_bytes: size,
                        est_tokens: size / 4,
                    });
                }
            }
        }
    }

    results.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));
    Ok(results)
}

/// Per-day cost breakdown for cost trend insights.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DailyCost {
    pub date: String,
    pub cost: f64,
    pub tokens: u64,
    pub sessions: u64,
}

/// Query daily cost totals (uses sonnet pricing as default for simplicity).
pub fn daily_cost_trend(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    tz_offset: i32,
) -> Result<Vec<DailyCost>> {
    let offset_str = if tz_offset >= 0 {
        format!(
            "+{:02}:{:02}",
            tz_offset / 60,
            tz_offset.unsigned_abs() % 60
        )
    } else {
        format!(
            "-{:02}:{:02}",
            tz_offset.unsigned_abs() / 60,
            tz_offset.unsigned_abs() % 60
        )
    };

    let (where_clause, date_params) = date_filter(since, until, "WHERE", 0);
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = date_params
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    let sql = format!(
        "SELECT DATE(timestamp, '{offset}') as d,
                COALESCE(SUM(input_tokens + cache_creation_tokens + cache_read_tokens), 0),
                COALESCE(SUM(output_tokens), 0),
                COALESCE(SUM(cost_cents), 0.0),
                COUNT(DISTINCT session_id)
         FROM messages {where_clause}
         GROUP BY d ORDER BY d",
        offset = offset_str,
        where_clause = where_clause,
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, u64>(2)?,
                row.get::<_, f64>(3)?,
                row.get::<_, u64>(4)?,
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

    let mut result = Vec::new();
    for (date, inp, outp, cost_cents, sessions) in rows {
        result.push(DailyCost {
            date,
            cost: (cost_cents / 100.0 * 100.0).round() / 100.0,
            tokens: inp + outp,
            sessions,
        });
    }

    Ok(result)
}

/// Session duration and cost stats for pattern analysis.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionPatternStats {
    pub total_sessions: u64,
    pub avg_duration_mins: f64,
    pub avg_messages_per_session: f64,
    pub avg_cost_per_session: f64,
    pub busiest_hour: Option<u32>,
    pub busiest_hour_sessions: u64,
}

pub fn session_patterns(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<SessionPatternStats> {
    let (where_clause, date_params) = date_filter(since, until, "WHERE", 0);
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = date_params
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    // Session durations and message counts
    let sql = format!(
        "SELECT s.session_id,
                s.first_seen, s.last_seen,
                COUNT(m.uuid) as msg_count,
                COALESCE(SUM(m.cost_cents), 0.0)
         FROM sessions s
         LEFT JOIN messages m ON m.session_id = s.session_id
         {where_clause}
         GROUP BY s.session_id",
        where_clause = where_clause.replace("timestamp", "m.timestamp"),
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<(String, String, String, u64, f64)> = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, u64>(3)?,
                row.get::<_, f64>(4)?,
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

    let n = rows.len() as f64;
    if n == 0.0 {
        return Ok(SessionPatternStats {
            total_sessions: 0,
            avg_duration_mins: 0.0,
            avg_messages_per_session: 0.0,
            avg_cost_per_session: 0.0,
            busiest_hour: None,
            busiest_hour_sessions: 0,
        });
    }

    let mut total_duration_secs = 0f64;
    let mut total_messages = 0u64;
    let mut total_cost_cents = 0f64;
    for (_, first, last, msgs, cost_cents) in &rows {
        if let (Ok(f), Ok(l)) = (
            chrono::DateTime::parse_from_rfc3339(first),
            chrono::DateTime::parse_from_rfc3339(last),
        ) {
            total_duration_secs += (l - f).num_seconds().max(0) as f64;
        }
        total_messages += msgs;
        total_cost_cents += cost_cents;
    }

    let avg_cost = total_cost_cents / 100.0 / n;

    // Busiest hour
    let hour_sql = format!(
        "SELECT CAST(strftime('%H', timestamp) AS INTEGER) as h, COUNT(DISTINCT session_id)
         FROM messages {where_clause}
         GROUP BY h ORDER BY COUNT(DISTINCT session_id) DESC LIMIT 1",
        where_clause = where_clause,
    );
    let hour_result: Option<(u32, u64)> = conn.prepare(&hour_sql).ok().and_then(|mut s| {
        let rows: Vec<(u32, u64)> = s
            .query_map(param_refs.as_slice(), |row| {
                Ok((row.get::<_, u32>(0)?, row.get::<_, u64>(1)?))
            })
            .ok()?
            .filter_map(|r| match r {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!("skipping row: {e}");
                    None
                }
            })
            .collect();
        rows.into_iter().next()
    });

    Ok(SessionPatternStats {
        total_sessions: rows.len() as u64,
        avg_duration_mins: (total_duration_secs / n / 60.0 * 10.0).round() / 10.0,
        avg_messages_per_session: (total_messages as f64 / n * 10.0).round() / 10.0,
        avg_cost_per_session: (avg_cost * 100.0).round() / 100.0,
        busiest_hour: hour_result.map(|(h, _)| h),
        busiest_hour_sessions: hour_result.map(|(_, c)| c).unwrap_or(0),
    })
}

/// Tool diversity stats.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolDiversity {
    pub unique_tools: u64,
    pub total_calls: u64,
    pub top_tool: Option<String>,
    pub top_tool_pct: f64,
}

pub fn tool_diversity(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<ToolDiversity> {
    let (having_clause, date_params) = date_filter(since, until, "AND", 0);
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = date_params
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    let sql = format!(
        "SELECT tu.tool_name, COUNT(*) as cnt
         FROM tool_usage tu
         JOIN messages m ON tu.message_uuid = m.uuid
         WHERE 1=1 {having_clause}
         GROUP BY tu.tool_name
         ORDER BY cnt DESC",
        having_clause = having_clause,
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<(String, u64)> = stmt
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

    let total: u64 = rows.iter().map(|(_, c)| c).sum();
    let unique = rows.len() as u64;
    let (top_tool, top_pct) = rows
        .first()
        .map(|(name, cnt)| {
            (
                Some(name.clone()),
                if total > 0 {
                    *cnt as f64 / total as f64 * 100.0
                } else {
                    0.0
                },
            )
        })
        .unwrap_or((None, 0.0));

    Ok(ToolDiversity {
        unique_tools: unique,
        total_calls: total,
        top_tool,
        top_tool_pct: (top_pct * 10.0).round() / 10.0,
    })
}

/// Compact stats for the status line display.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StatuslineStats {
    pub today_cost: f64,
    pub week_cost: f64,
    pub month_cost: f64,
}

/// Compute cost stats for today/week/month, suitable for the CLI status line.
pub fn statusline_stats(
    conn: &Connection,
    today: &str,
    week_start: &str,
    month_start: &str,
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

    Ok(StatuslineStats {
        today_cost,
        week_cost,
        month_cost,
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
        migrate(&conn).unwrap();
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
            },
        ];

        let count = ingest_messages(&mut conn, &msgs).unwrap();
        assert_eq!(count, 2);

        // Duplicate insert should be skipped.
        let count2 = ingest_messages(&mut conn, &msgs).unwrap();
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
        };
        ingest_messages(&mut conn, &[msg]).unwrap();

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
            },
        ];
        ingest_messages(&mut conn, &msgs).unwrap();

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
            },
        ]
    }

    #[test]
    fn session_list_returns_sessions() {
        let mut conn = test_db();
        ingest_messages(&mut conn, &sample_messages()).unwrap();

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
        ingest_messages(&mut conn, &sample_messages()).unwrap();

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
        ingest_messages(&mut conn, &msgs).unwrap();

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
            },
        ]
    }

    #[test]
    fn search_tool_stats_counts() {
        let mut conn = test_db();
        ingest_messages(&mut conn, &messages_with_tools()).unwrap();

        let stats = search_tool_stats(&conn, None, None).unwrap();
        // Grep + Glob = 2 search calls out of 7 total (Grep, Glob, Read, Edit, mcp__*, mcp__*, Read)
        assert_eq!(stats.search_calls, 2);
        assert_eq!(stats.total_calls, 7);
        assert!((stats.ratio - 2.0 / 7.0).abs() < 0.01);
    }

    #[test]
    fn mcp_tool_stats_groups_by_server() {
        let mut conn = test_db();
        ingest_messages(&mut conn, &messages_with_tools()).unwrap();

        let mcp = mcp_tool_stats(&conn, None, None).unwrap();
        assert_eq!(mcp.len(), 2);
        // Both have count 1.
        let servers: Vec<&str> = mcp.iter().map(|m| m.server.as_str()).collect();
        assert!(servers.contains(&"context7"));
        assert!(servers.contains(&"linear"));
    }

    #[test]
    fn token_heavy_sessions_filters() {
        let mut conn = test_db();
        ingest_messages(&mut conn, &messages_with_tools()).unwrap();

        // Threshold 5.0: s2 has ratio 50000/500 = 100, s1 has 800/300 = 2.67
        let heavy = token_heavy_sessions(&conn, None, None, 5.0).unwrap();
        assert_eq!(heavy.len(), 1);
        assert_eq!(heavy[0].session_id, "s2");
        assert_eq!(heavy[0].input_tokens, 50000);
        assert!(heavy[0].ratio > 90.0);
    }

    #[test]
    fn cache_stats_computes_hit_rate() {
        let mut conn = test_db();
        ingest_messages(&mut conn, &messages_with_tools()).unwrap();

        let cs = cache_stats(&conn, None, None).unwrap();
        // total_input = (500+0+200) + (300+100+150) + (50000+0+0) = 51250
        assert_eq!(cs.total_input_tokens, 51250);
        // cache_read = 200 + 150 + 0 = 350
        assert_eq!(cs.total_cache_read_tokens, 350);
        assert!((cs.hit_rate - 350.0 / 51250.0).abs() < 0.001);
    }

    #[test]
    fn project_dirs_returns_distinct() {
        let mut conn = test_db();
        ingest_messages(&mut conn, &messages_with_tools()).unwrap();

        let dirs = project_dirs(&conn).unwrap();
        assert_eq!(dirs.len(), 2);
        assert!(dirs.contains(&"/tmp/big".to_string()));
        assert!(dirs.contains(&"/tmp/proj".to_string()));
    }

    #[test]
    fn statusline_stats_empty_db() {
        let conn = test_db();
        let stats = statusline_stats(&conn, "2026-03-21", "2026-03-17", "2026-03-01").unwrap();
        assert_eq!(stats.today_cost, 0.0);
        assert_eq!(stats.week_cost, 0.0);
        assert_eq!(stats.month_cost, 0.0);
    }

    #[test]
    fn statusline_stats_with_data() {
        let mut conn = test_db();
        ingest_messages(&mut conn, &sample_messages()).unwrap();
        // sample_messages have timestamps on 2026-03-14
        let stats = statusline_stats(&conn, "2026-03-14", "2026-03-10", "2026-03-01").unwrap();
        assert!(stats.month_cost > 0.0);
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
            },
        ];

        ingest_messages(&mut conn, &claude_msgs).unwrap();
        ingest_messages(&mut conn, &cursor_msgs).unwrap();

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
