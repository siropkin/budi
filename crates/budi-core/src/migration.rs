//! Database schema migration for the analytics SQLite database.
//!
//! Migrations run explicitly via `budi update` or `budi sync`, not on every `open_db()`.

use anyhow::Result;
use rusqlite::{Connection, params};

/// Expected schema version for the current binary.
pub const SCHEMA_VERSION: u32 = 7;

/// Check the current schema version without migrating.
pub fn current_version(conn: &Connection) -> u32 {
    conn.pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap_or(0)
}

/// Returns true if the database needs migration.
pub fn needs_migration(conn: &Connection) -> bool {
    current_version(conn) < SCHEMA_VERSION
}

/// Run all pending migrations up to SCHEMA_VERSION.
/// Returns true if tags need backfilling (when tag-related schema changed).
pub fn migrate(conn: &Connection) -> Result<bool> {
    let version = current_version(conn);
    let mut needs_tag_backfill = false;

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
        backfill_cost_cents(conn)?;
    }

    if version < 6 {
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_sessions_title ON sessions(session_title);
             CREATE INDEX IF NOT EXISTS idx_messages_session_ts ON messages(session_id, timestamp);",
        )?;
    }

    if version < 7 {
        conn.execute_batch(
            "ALTER TABLE sessions ADD COLUMN user_name TEXT;
             ALTER TABLE sessions ADD COLUMN machine_name TEXT;
             ALTER TABLE messages ADD COLUMN parent_uuid TEXT;

             CREATE TABLE IF NOT EXISTS tags (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 message_uuid TEXT NOT NULL,
                 key TEXT NOT NULL,
                 value TEXT NOT NULL,
                 FOREIGN KEY (message_uuid) REFERENCES messages(uuid)
             );
             CREATE INDEX IF NOT EXISTS idx_tags_key_value ON tags(key, value);
             CREATE INDEX IF NOT EXISTS idx_tags_message ON tags(message_uuid);
             CREATE INDEX IF NOT EXISTS idx_messages_parent ON messages(parent_uuid);",
        )?;
        needs_tag_backfill = true;
    }

    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(needs_tag_backfill)
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
