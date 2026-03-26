//! Database schema migration for the analytics SQLite database.
//!
//! Two migration paths:
//!   1. Fresh install (user_version = 0) → create current schema from scratch.
//!   2. Upgrade from v6.0.0 release (user_version ≤ 6) → rebuild to current schema.
//!
//! Migrations run explicitly via `budi sync` or daemon auto-sync, not on every `open_db()`.

use anyhow::Result;
use rusqlite::Connection;

/// Expected schema version for the current binary.
pub const SCHEMA_VERSION: u32 = 10;

/// Check the current schema version without migrating.
pub fn current_version(conn: &Connection) -> u32 {
    conn.pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap_or(0)
}

/// Returns true if the database needs migration.
pub fn needs_migration(conn: &Connection) -> bool {
    current_version(conn) < SCHEMA_VERSION
}

/// Check if a database file needs migration without keeping the connection open.
pub fn needs_migration_at(db_path: &std::path::Path) -> bool {
    Connection::open(db_path)
        .map(|conn| needs_migration(&conn))
        .unwrap_or(false)
}

/// Run all pending migrations up to SCHEMA_VERSION.
pub fn migrate(conn: &Connection) -> Result<()> {
    let version = current_version(conn);

    if version >= SCHEMA_VERSION {
        return Ok(());
    }

    if version == 0 {
        // ── Fresh install ──────────────────────────────────────────────
        conn.execute_batch("PRAGMA foreign_keys=OFF;")?;
        create_current_schema(conn)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
    } else if version <= 6 {
        // ── Upgrade from v6.0.0 release (user_version 1–6) ────────────
        // The JSONL transcript files are the source of truth — just drop and
        // recreate the schema. A fresh sync rebuilds everything in ~60s, which
        // is faster and simpler than migrating 400k+ rows in-place.
        tracing::info!(
            from_version = version,
            to_version = SCHEMA_VERSION,
            "Destructive migration: dropping all tables and recreating schema"
        );
        conn.execute_batch("BEGIN EXCLUSIVE;")?;
        let result = (|| -> Result<()> {
            drop_all_tables(conn)?;
            conn.execute_batch("PRAGMA foreign_keys=OFF;")?;
            create_current_schema(conn)?;
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            conn.execute_batch("PRAGMA foreign_keys=ON;")?;
            Ok(())
        })();
        if result.is_err() {
            let _ = conn.execute_batch("ROLLBACK;");
            return result;
        }
        conn.execute_batch("COMMIT;")?;
    }

    // ── v7 → v8: add missing indexes ─────────────────────────────────
    if version == 7 {
        conn.execute_batch(
            "
            CREATE INDEX IF NOT EXISTS idx_messages_role_cwd ON messages(role, cwd);
            CREATE INDEX IF NOT EXISTS idx_hook_events_event_tool_provider ON hook_events(event, tool_name, provider);
            CREATE INDEX IF NOT EXISTS idx_hook_events_event_mcp ON hook_events(event, mcp_server);
            ",
        )?;
        conn.pragma_update(None, "user_version", 8u32)?;
    }

    // ── v8 → v9: drop unused hook_events columns, add sessions index ──
    if version <= 8 && current_version(conn) == 8 {
        // SQLite doesn't support DROP COLUMN before 3.35.0, and even then
        // it has restrictions.  Safest approach: recreate the table.
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS hook_events_new (
                id                  INTEGER PRIMARY KEY AUTOINCREMENT,
                provider            TEXT NOT NULL,
                event               TEXT NOT NULL,
                conversation_id     TEXT,
                timestamp           TEXT NOT NULL,
                model               TEXT,
                tool_name           TEXT,
                tool_duration_ms    INTEGER,
                tool_call_count     INTEGER,
                raw_json            TEXT,
                mcp_server          TEXT
            );
            INSERT INTO hook_events_new (id, provider, event, conversation_id, timestamp, model,
                tool_name, tool_duration_ms, tool_call_count, raw_json, mcp_server)
                SELECT id, provider, event, conversation_id, timestamp, model,
                       tool_name, tool_duration_ms, tool_call_count, raw_json, mcp_server
                FROM hook_events;
            DROP TABLE hook_events;
            ALTER TABLE hook_events_new RENAME TO hook_events;

            -- Recreate indexes on hook_events
            CREATE INDEX IF NOT EXISTS idx_hook_events_conversation ON hook_events(conversation_id);
            CREATE INDEX IF NOT EXISTS idx_hook_events_timestamp ON hook_events(timestamp);
            CREATE INDEX IF NOT EXISTS idx_hook_events_event ON hook_events(event);
            CREATE INDEX IF NOT EXISTS idx_hook_events_provider ON hook_events(provider);
            CREATE INDEX IF NOT EXISTS idx_hook_events_event_timestamp ON hook_events(event, timestamp);
            CREATE INDEX IF NOT EXISTS idx_hook_events_event_tool ON hook_events(event, tool_name);
            CREATE INDEX IF NOT EXISTS idx_hook_events_event_conversation ON hook_events(event, conversation_id);
            CREATE INDEX IF NOT EXISTS idx_hook_events_mcp_server ON hook_events(mcp_server);
            CREATE INDEX IF NOT EXISTS idx_hook_events_event_tool_provider ON hook_events(event, tool_name, provider);
            CREATE INDEX IF NOT EXISTS idx_hook_events_event_mcp ON hook_events(event, mcp_server);
            CREATE INDEX IF NOT EXISTS idx_hook_events_event_conversation_ts ON hook_events(event, conversation_id, timestamp);

            -- Add missing sessions index
            CREATE INDEX IF NOT EXISTS idx_sessions_conversation_id ON sessions(conversation_id);
            ",
        )?;
        conn.pragma_update(None, "user_version", 9u32)?;
    }

    // ── v9 → v10: drop tool_usage table, add missing indexes ────────────
    if current_version(conn) == 9 {
        conn.execute_batch(
            "
            DROP TABLE IF EXISTS tool_usage;

            -- New message indexes
            CREATE INDEX IF NOT EXISTS idx_messages_session_role ON messages(session_id, role);
            CREATE INDEX IF NOT EXISTS idx_messages_cwd_role ON messages(cwd, role);
            CREATE INDEX IF NOT EXISTS idx_messages_session_role_cost ON messages(session_id, role, cost_cents);

            -- New tag index
            CREATE INDEX IF NOT EXISTS idx_tags_msg_key_val ON tags(message_uuid, key, value);
            ",
        )?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }

    Ok(())
}

/// Drop all user tables so the schema can be recreated from scratch.
fn drop_all_tables(conn: &Connection) -> Result<()> {
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")?
        .query_map([], |r| r.get(0))?
        .filter_map(|r| r.ok())
        .collect();
    for table in tables {
        conn.execute_batch(&format!("DROP TABLE IF EXISTS \"{table}\";"))?;
    }
    Ok(())
}

// ── Fresh install schema ───────────────────────────────────────────────

fn create_current_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
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
            cwd                    TEXT,
            repo_id                TEXT,
            provider               TEXT DEFAULT 'claude_code',
            cost_cents             REAL,
            parent_uuid            TEXT,
            git_branch             TEXT,
            cost_confidence        TEXT DEFAULT 'estimated'
        );

        CREATE TABLE IF NOT EXISTS tags (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            message_uuid TEXT NOT NULL,
            key          TEXT NOT NULL,
            value        TEXT NOT NULL,
            UNIQUE(message_uuid, key, value),
            FOREIGN KEY (message_uuid) REFERENCES messages(uuid) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS sync_state (
            file_path    TEXT PRIMARY KEY,
            byte_offset  INTEGER NOT NULL DEFAULT 0,
            last_synced  TEXT NOT NULL
        );
        ",
    )?;
    create_sessions_and_hook_events(conn)?;
    create_indexes(conn)?;
    Ok(())
}

// ── Shared helpers ─────────────────────────────────────────────────────

/// Create sessions and hook_events tables.
///
/// Note: `repo_id` and `git_branch` are denormalized on both `messages` (canonical
/// for cost queries) and `sessions` (derived from hooks for metadata context).
/// Messages are the source of truth for cost queries.
fn create_sessions_and_hook_events(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS sessions (
            conversation_id    TEXT PRIMARY KEY,
            provider           TEXT NOT NULL DEFAULT 'claude_code',
            started_at         TEXT,
            ended_at           TEXT,
            duration_ms        INTEGER,
            composer_mode      TEXT,
            permission_mode    TEXT,
            user_email         TEXT,
            workspace_root     TEXT,
            end_reason         TEXT,
            prompt_category    TEXT,
            model              TEXT,
            raw_json           TEXT,
            repo_id            TEXT,
            git_branch         TEXT
        );

        CREATE TABLE IF NOT EXISTS hook_events (
            id                  INTEGER PRIMARY KEY AUTOINCREMENT,
            provider            TEXT NOT NULL,
            event               TEXT NOT NULL,
            conversation_id     TEXT,
            timestamp           TEXT NOT NULL,
            model               TEXT,
            tool_name           TEXT,
            tool_duration_ms    INTEGER,
            tool_call_count     INTEGER,
            raw_json            TEXT,
            mcp_server          TEXT
        );
        ",
    )?;
    Ok(())
}

fn create_indexes(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        -- messages
        CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id);
        CREATE INDEX IF NOT EXISTS idx_messages_timestamp ON messages(timestamp);
        CREATE INDEX IF NOT EXISTS idx_messages_session_ts ON messages(session_id, timestamp);
        CREATE INDEX IF NOT EXISTS idx_messages_repo ON messages(repo_id);
        CREATE INDEX IF NOT EXISTS idx_messages_provider ON messages(provider);
        CREATE INDEX IF NOT EXISTS idx_messages_parent ON messages(parent_uuid);
        CREATE INDEX IF NOT EXISTS idx_messages_branch ON messages(git_branch);
        CREATE INDEX IF NOT EXISTS idx_messages_role ON messages(role);

        -- tags
        CREATE INDEX IF NOT EXISTS idx_tags_key_value ON tags(key, value);
        CREATE INDEX IF NOT EXISTS idx_tags_message ON tags(message_uuid);
        CREATE INDEX IF NOT EXISTS idx_tags_msg_key_val ON tags(message_uuid, key, value);

        -- messages (covering indexes for cost queries)
        CREATE INDEX IF NOT EXISTS idx_messages_ts_cost ON messages(timestamp, cost_cents);
        CREATE INDEX IF NOT EXISTS idx_messages_role_ts_cost ON messages(role, timestamp, cost_cents);
        CREATE INDEX IF NOT EXISTS idx_messages_role_branch_cost ON messages(role, git_branch, cost_cents);
        CREATE INDEX IF NOT EXISTS idx_messages_role_branch_ts ON messages(role, git_branch, timestamp);
        CREATE INDEX IF NOT EXISTS idx_messages_role_cwd ON messages(role, cwd);
        CREATE INDEX IF NOT EXISTS idx_messages_session_role ON messages(session_id, role);
        CREATE INDEX IF NOT EXISTS idx_messages_cwd_role ON messages(cwd, role);
        CREATE INDEX IF NOT EXISTS idx_messages_session_role_cost ON messages(session_id, role, cost_cents);

        -- sessions
        CREATE INDEX IF NOT EXISTS idx_sessions_conversation_id ON sessions(conversation_id);
        CREATE INDEX IF NOT EXISTS idx_sessions_provider ON sessions(provider);
        CREATE INDEX IF NOT EXISTS idx_sessions_started ON sessions(started_at);

        -- hook_events
        CREATE INDEX IF NOT EXISTS idx_hook_events_conversation ON hook_events(conversation_id);
        CREATE INDEX IF NOT EXISTS idx_hook_events_timestamp ON hook_events(timestamp);
        CREATE INDEX IF NOT EXISTS idx_hook_events_event ON hook_events(event);
        CREATE INDEX IF NOT EXISTS idx_hook_events_provider ON hook_events(provider);
        CREATE INDEX IF NOT EXISTS idx_hook_events_event_timestamp ON hook_events(event, timestamp);
        CREATE INDEX IF NOT EXISTS idx_hook_events_event_tool ON hook_events(event, tool_name);
        CREATE INDEX IF NOT EXISTS idx_hook_events_event_conversation ON hook_events(event, conversation_id);
        CREATE INDEX IF NOT EXISTS idx_hook_events_mcp_server ON hook_events(mcp_server);
        CREATE INDEX IF NOT EXISTS idx_hook_events_event_tool_provider ON hook_events(event, tool_name, provider);
        CREATE INDEX IF NOT EXISTS idx_hook_events_event_mcp ON hook_events(event, mcp_server);
        CREATE INDEX IF NOT EXISTS idx_hook_events_event_conversation_ts ON hook_events(event, conversation_id, timestamp);
        ",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    /// Simulate a v5 database with a different schema, then migrate and verify
    /// it reaches the current SCHEMA_VERSION with all expected tables.
    #[test]
    fn migrate_from_old_schema_to_current() {
        let conn = Connection::open_in_memory().unwrap();

        // Create a fake old schema (v5) with a subset of tables
        conn.execute_batch(
            "
            CREATE TABLE messages (id INTEGER PRIMARY KEY, text TEXT);
            CREATE TABLE old_table (id INTEGER PRIMARY KEY);
            ",
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 5u32).unwrap();

        assert_eq!(current_version(&conn), 5);
        assert!(needs_migration(&conn));

        migrate(&conn).unwrap();

        assert_eq!(current_version(&conn), SCHEMA_VERSION);
        assert!(!needs_migration(&conn));

        // Verify core tables exist by querying them
        conn.execute_batch("SELECT count(*) FROM messages").unwrap();
        conn.execute_batch("SELECT count(*) FROM sessions").unwrap();
        conn.execute_batch("SELECT count(*) FROM hook_events").unwrap();
        conn.execute_batch("SELECT count(*) FROM tags").unwrap();
        conn.execute_batch("SELECT count(*) FROM sync_state").unwrap();

        // Verify old table was dropped
        let old_exists: bool = conn
            .prepare("SELECT count(*) FROM sqlite_master WHERE type='table' AND name='old_table'")
            .unwrap()
            .query_row([], |r| r.get::<_, i64>(0))
            .map(|c| c > 0)
            .unwrap();
        assert!(!old_exists, "old_table should have been dropped");
    }

    #[test]
    fn migrate_fresh_install() {
        let conn = Connection::open_in_memory().unwrap();
        assert_eq!(current_version(&conn), 0);

        migrate(&conn).unwrap();

        assert_eq!(current_version(&conn), SCHEMA_VERSION);
        conn.execute_batch("SELECT count(*) FROM messages").unwrap();
        conn.execute_batch("SELECT count(*) FROM sessions").unwrap();
    }

    #[test]
    fn migrate_already_current_is_noop() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        // Running again should be a no-op
        migrate(&conn).unwrap();
        assert_eq!(current_version(&conn), SCHEMA_VERSION);
    }
}
