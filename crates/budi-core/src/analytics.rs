//! SQLite-backed analytics storage for Claude Code usage data.
//!
//! Stores sessions, messages, and tool usage extracted from Claude Code
//! JSONL transcript files. Supports incremental ingestion via sync state
//! tracking (byte offset per file).

use std::collections::HashMap;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::jsonl::ParsedMessage;

const SCHEMA_VERSION: u32 = 2;

/// Open (or create) the analytics database at the given path.
pub fn open_db(db_path: &Path) -> Result<Connection> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create dir {}", parent.display()))?;
    }
    let conn = Connection::open(db_path)?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
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

    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;

    Ok(())
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
fn upsert_session(
    conn: &Connection,
    session_id: &str,
    timestamp: &DateTime<Utc>,
    cwd: Option<&str>,
    version: Option<&str>,
    git_branch: Option<&str>,
    repo_id: Option<&str>,
) -> Result<()> {
    let ts = timestamp.to_rfc3339();
    conn.execute(
        "INSERT INTO sessions (session_id, project_dir, first_seen, last_seen, version, git_branch, repo_id)
         VALUES (?1, ?2, ?3, ?3, ?4, ?5, ?6)
         ON CONFLICT(session_id) DO UPDATE SET
           last_seen = MAX(sessions.last_seen, ?3),
           project_dir = COALESCE(?2, sessions.project_dir),
           version = COALESCE(?4, sessions.version),
           git_branch = COALESCE(?5, sessions.git_branch),
           repo_id = COALESCE(?6, sessions.repo_id)",
        params![session_id, cwd, ts, version, git_branch, repo_id],
    )?;
    Ok(())
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
            )?;
        }

        // Insert message (skip duplicates).
        let ts = msg.timestamp.to_rfc3339();
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO messages
             (uuid, session_id, role, timestamp, model,
              input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
              has_thinking, stop_reason, text_length, cwd, repo_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
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

/// Discover all Claude Code JSONL transcript files under `~/.claude/projects/`.
pub fn discover_jsonl_files() -> Result<Vec<PathBuf>> {
    let claude_dir = dirs_claude_projects()?;
    let mut files = Vec::new();
    collect_jsonl_recursive(&claude_dir, &mut files, 0);
    files.sort();
    Ok(files)
}

fn collect_jsonl_recursive(dir: &Path, files: &mut Vec<PathBuf>, depth: u32) {
    // Limit recursion depth to avoid runaway traversal.
    if depth > 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Skip subagents directory — those are sub-conversations
            if path.file_name().map(|n| n == "subagents").unwrap_or(false) {
                continue;
            }
            collect_jsonl_recursive(&path, files, depth + 1);
        } else if path.extension().is_some_and(|e| e == "jsonl") {
            files.push(path);
        }
    }
}

fn dirs_claude_projects() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".claude").join("projects"))
}

/// Path to the analytics database file.
pub fn db_path() -> Result<PathBuf> {
    let home_dir = crate::config::budi_home_dir()?;
    Ok(home_dir.join("analytics.db"))
}

/// Run a full incremental sync: discover JSONL files, parse new content, ingest.
/// Returns (files_synced, messages_ingested).
pub fn sync_all(conn: &mut Connection) -> Result<(usize, usize)> {
    let files = discover_jsonl_files()?;
    let mut total_files = 0;
    let mut total_messages = 0;
    let mut repo_cache = crate::repo_id::RepoIdCache::new();

    for file_path in &files {
        let path_str = file_path.display().to_string();
        let offset = get_sync_offset(conn, &path_str)?;

        let content = std::fs::read_to_string(file_path)
            .with_context(|| format!("Failed to read {}", file_path.display()))?;

        if offset >= content.len() {
            continue; // Already fully synced.
        }

        let (mut messages, new_offset) = crate::jsonl::parse_transcript(&content, offset);
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
    pub top_tools: Vec<(String, u64)>,
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

    let total_sessions: u64 = conn.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))?;

    let total_messages: u64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM messages {}", where_clause),
        param_refs.as_slice(),
        |r| r.get(0),
    )?;

    let and_clause = if where_clause.is_empty() {
        "WHERE"
    } else {
        "AND"
    };
    let total_user_messages: u64 = conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM messages {} {} role = 'user'",
            where_clause, and_clause
        ),
        param_refs.as_slice(),
        |r| r.get(0),
    )?;

    let total_assistant_messages: u64 = conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM messages {} {} role = 'assistant'",
            where_clause, and_clause
        ),
        param_refs.as_slice(),
        |r| r.get(0),
    )?;

    let (total_input, total_output, total_cache_create, total_cache_read): (u64, u64, u64, u64) =
        conn.query_row(
            &format!(
                "SELECT COALESCE(SUM(input_tokens),0), COALESCE(SUM(output_tokens),0),
                        COALESCE(SUM(cache_creation_tokens),0), COALESCE(SUM(cache_read_tokens),0)
                 FROM messages {}",
                where_clause
            ),
            param_refs.as_slice(),
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;

    // Top tools by usage count.
    let tool_clause = if where_clause.is_empty() {
        String::new()
    } else {
        format!(
            "WHERE message_uuid IN (SELECT uuid FROM messages {})",
            where_clause
        )
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT tool_name, COUNT(*) as cnt FROM tool_usage {} GROUP BY tool_name ORDER BY cnt DESC LIMIT 50",
        tool_clause
    ))?;
    let top_tools: Vec<(String, u64)> = stmt
        .query_map(param_refs.as_slice(), |row| Ok((row.get(0)?, row.get(1)?)))?
        .filter_map(|r| r.ok())
        .collect();

    Ok(UsageSummary {
        total_sessions,
        total_messages,
        total_user_messages,
        total_assistant_messages,
        total_input_tokens: total_input,
        total_output_tokens: total_output,
        total_cache_creation_tokens: total_cache_create,
        total_cache_read_tokens: total_cache_read,
        top_tools,
    })
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
    pub tool_calls: u64,
}

/// List sessions with aggregated stats, ordered by most recent first.
/// Optionally filtered by date range (same format as usage_summary).
pub fn session_list(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Vec<SessionSummary>> {
    // Build parameterized date filter for m.timestamp columns.
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
        "SELECT s.session_id, s.project_dir, s.first_seen, s.last_seen,
                COUNT(m.uuid) as msg_count,
                COALESCE(SUM(m.input_tokens), 0) as inp,
                COALESCE(SUM(m.output_tokens), 0) as outp,
                COALESCE(SUM(m.cache_creation_tokens), 0) as cache_c,
                COALESCE(SUM(m.cache_read_tokens), 0) as cache_r,
                (SELECT COUNT(*) FROM tool_usage tu
                 JOIN messages mm ON tu.message_uuid = mm.uuid
                 WHERE mm.session_id = s.session_id) as tool_calls,
                s.repo_id
         FROM sessions s
         LEFT JOIN messages m ON m.session_id = s.session_id
         {}
         GROUP BY s.session_id
         ORDER BY s.last_seen DESC",
        where_clause
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
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
                tool_calls: row.get(9)?,
                repo_id: row.get(10)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
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
}

/// Get detailed stats for a single session by ID (prefix match supported).
pub fn session_detail(conn: &Connection, session_id_prefix: &str) -> Result<Option<SessionDetail>> {
    // Find session by exact or prefix match.
    let session_row = conn
        .query_row(
            "SELECT session_id, project_dir, first_seen, last_seen, version, git_branch, repo_id
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
                    user_messages: 0,
                    assistant_messages: 0,
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_creation_tokens: 0,
                    cache_read_tokens: 0,
                    top_tools: vec![],
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
            COALESCE(SUM(cache_read_tokens), 0)
         FROM messages WHERE session_id = ?1",
        params![sid],
        |r| {
            detail.user_messages = r.get(0)?;
            detail.assistant_messages = r.get(1)?;
            detail.input_tokens = r.get(2)?;
            detail.output_tokens = r.get(3)?;
            detail.cache_creation_tokens = r.get(4)?;
            detail.cache_read_tokens = r.get(5)?;
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
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
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
        .filter_map(|r| r.ok())
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

    let tool_where = if where_clause.is_empty() {
        "WHERE tool_name LIKE 'mcp__%'".to_string()
    } else {
        format!(
            "WHERE message_uuid IN (SELECT uuid FROM messages {}) AND tool_name LIKE 'mcp__%%'",
            where_clause
        )
    };
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = date_params
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    let mut stmt = conn.prepare(&format!(
        "SELECT tool_name, COUNT(*) as cnt FROM tool_usage {} GROUP BY tool_name ORDER BY cnt DESC",
        tool_where
    ))?;

    let rows: Vec<(String, u64)> = stmt
        .query_map(param_refs.as_slice(), |row| Ok((row.get(0)?, row.get(1)?)))?
        .filter_map(|r| r.ok())
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
        .filter_map(|r| r.ok())
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
        .filter_map(|r| r.ok())
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
        .filter_map(|r| r.ok())
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
        "SELECT {} as bucket, COUNT(*) as cnt
         FROM messages {}
         GROUP BY bucket ORDER BY bucket",
        group_expr, where_clause
    );

    let mut stmt = conn.prepare(&sql)?;
    let msg_rows: Vec<(String, u64)> = stmt
        .query_map(param_refs.as_slice(), |row| Ok((row.get(0)?, row.get(1)?)))?
        .filter_map(|r| r.ok())
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
        .filter_map(|r| r.ok())
        .collect();

    let results = msg_rows
        .into_iter()
        .map(|(label, count)| ActivityBucket {
            tool_call_count: tool_rows.get(&label).copied().unwrap_or(0),
            label,
            message_count: count,
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
        "SELECT COALESCE(model, 'unknown') as m,
                COUNT(*) as cnt,
                COALESCE(SUM(input_tokens), 0),
                COALESCE(SUM(output_tokens), 0),
                COALESCE(SUM(cache_read_tokens), 0),
                COALESCE(SUM(cache_creation_tokens), 0)
         FROM messages
         {} {} role = 'assistant'
         GROUP BY m
         ORDER BY cnt DESC",
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
        .filter_map(|r| r.ok())
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
                COALESCE(SUM(input_tokens), 0),
                COALESCE(SUM(output_tokens), 0),
                COALESCE(SUM(cache_creation_tokens), 0),
                COALESCE(SUM(cache_read_tokens), 0),
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
                row.get::<_, u64>(3)?,
                row.get::<_, u64>(4)?,
                row.get::<_, u64>(5)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect::<Vec<_>>();

    let mut result = Vec::new();
    for (date, inp, outp, cw, cr, sessions) in rows {
        // Use rough sonnet pricing for the trend (per-model breakdown isn't needed here)
        let cost = inp as f64 * 3.0 / 1_000_000.0
            + outp as f64 * 15.0 / 1_000_000.0
            + cw as f64 * 3.75 / 1_000_000.0
            + cr as f64 * 0.30 / 1_000_000.0;
        result.push(DailyCost {
            date,
            cost: (cost * 100.0).round() / 100.0,
            tokens: inp + outp + cw + cr,
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
                COALESCE(SUM(m.input_tokens), 0) + COALESCE(SUM(m.output_tokens), 0) +
                COALESCE(SUM(m.cache_creation_tokens), 0) + COALESCE(SUM(m.cache_read_tokens), 0) as total_tok
         FROM sessions s
         LEFT JOIN messages m ON m.session_id = s.session_id
         {where_clause}
         GROUP BY s.session_id",
        where_clause =
            where_clause.replace("timestamp", "m.timestamp"),
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<(String, String, String, u64, u64)> = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, u64>(3)?,
                row.get::<_, u64>(4)?,
            ))
        })?
        .filter_map(|r| r.ok())
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
    let mut total_tokens = 0u64;
    for (_, first, last, msgs, toks) in &rows {
        if let (Ok(f), Ok(l)) = (
            chrono::DateTime::parse_from_rfc3339(first),
            chrono::DateTime::parse_from_rfc3339(last),
        ) {
            total_duration_secs += (l - f).num_seconds().max(0) as f64;
        }
        total_messages += msgs;
        total_tokens += toks;
    }

    // Rough cost estimate (sonnet default)
    let avg_cost = (total_tokens as f64 * 5.0 / 1_000_000.0) / n; // blended ~$5/M

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
            .filter_map(|r| r.ok())
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
        .filter_map(|r| r.ok())
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
    pub total_tokens: u64,
    pub estimated_cost: f64,
    pub cache_hit_rate: f64,
    pub session_count: u64,
    pub active_minutes: u64,
}

/// Compute compact stats for today, suitable for the CLI status line.
pub fn statusline_stats(conn: &Connection, today: &str) -> Result<StatuslineStats> {
    // Token totals for today
    let (input, output, cache_create, cache_read): (u64, u64, u64, u64) = conn.query_row(
        "SELECT COALESCE(SUM(input_tokens),0), COALESCE(SUM(output_tokens),0),
                    COALESCE(SUM(cache_creation_tokens),0), COALESCE(SUM(cache_read_tokens),0)
             FROM messages WHERE timestamp >= ?1",
        [today],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;

    let total_tokens = input + output + cache_create + cache_read;

    // Cost estimate for today
    let cost = crate::cost::estimate_cost(conn, Some(today), None)?;

    // Cache hit rate: cache_read / (input + cache_read)
    let cache_hit_rate = if input + cache_read > 0 {
        cache_read as f64 / (input + cache_read) as f64
    } else {
        0.0
    };

    // Session count and active time for today
    let session_count: u64 = conn.query_row(
        "SELECT COUNT(DISTINCT session_id) FROM messages WHERE timestamp >= ?1",
        [today],
        |r| r.get(0),
    )?;

    // Active minutes: sum of (last_message - first_message) per session today
    let active_minutes: u64 = conn.query_row(
        "SELECT COALESCE(SUM(span_mins), 0) FROM (
             SELECT (JULIANDAY(MAX(timestamp)) - JULIANDAY(MIN(timestamp))) * 24 * 60 AS span_mins
             FROM messages WHERE timestamp >= ?1
             GROUP BY session_id
         )",
        [today],
        |r| {
            let mins: f64 = r.get(0)?;
            Ok(mins.round() as u64)
        },
    )?;

    Ok(StatuslineStats {
        total_tokens,
        estimated_cost: cost.total_cost,
        cache_hit_rate: (cache_hit_rate * 1000.0).round() / 10.0, // percentage with 1 decimal
        session_count,
        active_minutes,
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
            .filter_map(|r| r.ok())
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
        assert_eq!(summary.top_tools.len(), 2);
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
            },
        ]
    }

    #[test]
    fn session_list_returns_sessions() {
        let mut conn = test_db();
        ingest_messages(&mut conn, &sample_messages()).unwrap();

        let sessions = session_list(&conn, None, None).unwrap();
        assert_eq!(sessions.len(), 2);
        // Most recent first.
        assert_eq!(sessions[0].session_id, "sess-def");
        assert_eq!(sessions[1].session_id, "sess-abc");
        assert_eq!(sessions[1].input_tokens, 100);
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
        ingest_messages(&mut conn, &msgs).unwrap();

        let repos = repo_usage(&conn, None, None, 10).unwrap();
        assert_eq!(repos.len(), 2);
        // project-a has 2 messages (more tokens), project-b has 1.
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
        let stats = statusline_stats(&conn, "2026-03-21").unwrap();
        assert_eq!(stats.total_tokens, 0);
        assert_eq!(stats.estimated_cost, 0.0);
        assert_eq!(stats.cache_hit_rate, 0.0);
        assert_eq!(stats.session_count, 0);
        assert_eq!(stats.active_minutes, 0);
    }

    #[test]
    fn statusline_stats_with_data() {
        let mut conn = test_db();
        ingest_messages(&mut conn, &sample_messages()).unwrap();
        // sample_messages have timestamps on 2026-03-14
        let stats = statusline_stats(&conn, "2026-03-14").unwrap();
        assert!(stats.total_tokens > 0);
        assert!(stats.session_count > 0);
    }
}
