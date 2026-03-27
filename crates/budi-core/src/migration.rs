//! Database schema migration for the analytics SQLite database.
//!
//! Three migration paths:
//!   1. Fresh install (user_version = 0) → create current schema from scratch.
//!   2. Upgrade from pre-stable version (1–9) → drop all tables, recreate from scratch.
//!   3. Upgrade from stable version (≥ 10) → incremental migrations.
//!
//! Migrations run explicitly via `budi sync` or daemon auto-sync, not on every `open_db()`.

use anyhow::Result;
use rusqlite::Connection;

/// Expected schema version for the current binary.
pub const SCHEMA_VERSION: u32 = 12;

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
    } else if version < 10 {
        // ── Upgrade from pre-stable version (dev builds, v7.0.0, etc.) ─
        // These schemas varied across dev builds and can have FK violations.
        // JSONL transcripts are the source of truth — nuke and rebuild.
        tracing::info!(
            from_version = version,
            to_version = SCHEMA_VERSION,
            "Destructive migration: dropping all tables and recreating schema"
        );
        conn.execute_batch("PRAGMA foreign_keys=OFF;")?;
        drop_all_tables(conn)?;
        create_current_schema(conn)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
    }

    // ── Incremental migrations from stable baseline (v10+) ─────────
    if current_version(conn) == 10 {
        migrate_v10_to_v11(conn)?;
    }
    if current_version(conn) == 11 {
        migrate_v11_to_v12(conn)?;
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
    create_otel_events(conn)?;
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

/// Create otel_events table for raw OTEL event storage.
fn create_otel_events(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS otel_events (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            event_name  TEXT NOT NULL,
            session_id  TEXT,
            timestamp   TEXT NOT NULL,
            raw_json    TEXT,
            processed   INTEGER NOT NULL DEFAULT 0
        );
        ",
    )?;
    Ok(())
}

/// Incremental migration from v10 to v11: add otel_events table.
fn migrate_v10_to_v11(conn: &Connection) -> Result<()> {
    tracing::info!("Migrating schema v10 → v11: adding otel_events table");
    create_otel_events(conn)?;
    conn.execute_batch(
        "
        CREATE INDEX IF NOT EXISTS idx_otel_events_session ON otel_events(session_id);
        CREATE INDEX IF NOT EXISTS idx_otel_events_timestamp ON otel_events(timestamp);
        ",
    )?;
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

/// Incremental migration from v11 to v12: add composite dedup index for OTEL/JSONL matching.
fn migrate_v11_to_v12(conn: &Connection) -> Result<()> {
    tracing::info!("Migrating schema v11 → v12: adding dedup index for OTEL/JSONL matching");
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_messages_dedup
            ON messages(session_id, model, role, cost_confidence, timestamp);",
    )?;
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
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
        CREATE INDEX IF NOT EXISTS idx_messages_dedup ON messages(session_id, model, role, cost_confidence, timestamp);

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

        -- otel_events
        CREATE INDEX IF NOT EXISTS idx_otel_events_session ON otel_events(session_id);
        CREATE INDEX IF NOT EXISTS idx_otel_events_timestamp ON otel_events(timestamp);
        ",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    /// Simulate an old database, then migrate and verify it reaches
    /// the current SCHEMA_VERSION with all expected tables.
    #[test]
    fn migrate_from_old_schema_to_current() {
        // Test with several old versions to ensure destructive rebuild works
        for old_version in [3, 5, 7, 9] {
            let conn = Connection::open_in_memory().unwrap();
            conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();

            // Create a fake old schema with some tables
            conn.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
            conn.execute_batch(
                "
                CREATE TABLE messages (id INTEGER PRIMARY KEY, text TEXT);
                CREATE TABLE old_table (id INTEGER PRIMARY KEY);
                ",
            )
            .unwrap();
            conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
            conn.pragma_update(None, "user_version", old_version)
                .unwrap();

            assert!(needs_migration(&conn));

            migrate(&conn).unwrap();

            assert_eq!(current_version(&conn), SCHEMA_VERSION);
            assert!(!needs_migration(&conn));

            // Verify core tables exist
            conn.execute_batch("SELECT count(*) FROM messages").unwrap();
            conn.execute_batch("SELECT count(*) FROM sessions").unwrap();
            conn.execute_batch("SELECT count(*) FROM hook_events")
                .unwrap();
            conn.execute_batch("SELECT count(*) FROM tags").unwrap();
            conn.execute_batch("SELECT count(*) FROM sync_state")
                .unwrap();
            conn.execute_batch("SELECT count(*) FROM otel_events")
                .unwrap();

            // Verify old table was dropped
            let old_exists: bool = conn
                .prepare(
                    "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='old_table'",
                )
                .unwrap()
                .query_row([], |r| r.get::<_, i64>(0))
                .map(|c| c > 0)
                .unwrap();
            assert!(
                !old_exists,
                "old_table should have been dropped (v{old_version})"
            );
        }
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

    #[test]
    fn migrate_v10_to_v11_adds_otel_events() {
        let conn = Connection::open_in_memory().unwrap();
        // Start at v10 with current schema minus otel_events
        conn.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
        // Create the v10 schema (everything except otel_events)
        conn.execute_batch(
            "
            CREATE TABLE messages (
                uuid TEXT PRIMARY KEY, session_id TEXT, role TEXT NOT NULL,
                timestamp TEXT NOT NULL, model TEXT,
                input_tokens INTEGER NOT NULL DEFAULT 0, output_tokens INTEGER NOT NULL DEFAULT 0,
                cache_creation_tokens INTEGER NOT NULL DEFAULT 0, cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                cwd TEXT, repo_id TEXT, provider TEXT DEFAULT 'claude_code', cost_cents REAL,
                parent_uuid TEXT, git_branch TEXT, cost_confidence TEXT DEFAULT 'estimated'
            );
            CREATE TABLE tags (
                id INTEGER PRIMARY KEY AUTOINCREMENT, message_uuid TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL,
                UNIQUE(message_uuid, key, value),
                FOREIGN KEY (message_uuid) REFERENCES messages(uuid) ON DELETE CASCADE
            );
            CREATE TABLE sync_state (file_path TEXT PRIMARY KEY, byte_offset INTEGER NOT NULL DEFAULT 0, last_synced TEXT NOT NULL);
            CREATE TABLE sessions (
                conversation_id TEXT PRIMARY KEY, provider TEXT NOT NULL DEFAULT 'claude_code',
                started_at TEXT, ended_at TEXT, duration_ms INTEGER, composer_mode TEXT,
                permission_mode TEXT, user_email TEXT, workspace_root TEXT, end_reason TEXT,
                prompt_category TEXT, model TEXT, raw_json TEXT, repo_id TEXT, git_branch TEXT
            );
            CREATE TABLE hook_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT, provider TEXT NOT NULL, event TEXT NOT NULL,
                conversation_id TEXT, timestamp TEXT NOT NULL, model TEXT, tool_name TEXT,
                tool_duration_ms INTEGER, tool_call_count INTEGER, raw_json TEXT, mcp_server TEXT
            );
            ",
        )
        .unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        conn.pragma_update(None, "user_version", 10u32).unwrap();

        assert!(needs_migration(&conn));
        migrate(&conn).unwrap();
        assert_eq!(current_version(&conn), SCHEMA_VERSION);

        // otel_events table should exist
        conn.execute_batch("SELECT count(*) FROM otel_events")
            .unwrap();
    }

    /// Simulate a database with orphaned FK references (tags pointing to
    /// non-existent messages). This can happen with dev builds or corrupted DBs.
    #[test]
    fn migrate_with_orphaned_fk_data() {
        let conn = Connection::open_in_memory().unwrap();

        // Simulate open_db pragmas — foreign_keys=ON is the key
        conn.execute_batch(
            "PRAGMA foreign_keys=ON;
             PRAGMA busy_timeout=5000;",
        )
        .unwrap();

        // Create old v7 schema
        conn.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
        conn.execute_batch(
            "
            CREATE TABLE messages (
                uuid TEXT PRIMARY KEY, session_id TEXT, role TEXT NOT NULL,
                timestamp TEXT NOT NULL, model TEXT,
                input_tokens INTEGER NOT NULL DEFAULT 0, output_tokens INTEGER NOT NULL DEFAULT 0,
                cache_creation_tokens INTEGER NOT NULL DEFAULT 0, cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                cwd TEXT, repo_id TEXT, provider TEXT DEFAULT 'claude_code', cost_cents REAL,
                context_tokens_used INTEGER, context_token_limit INTEGER,
                parent_uuid TEXT, git_branch TEXT, cost_confidence TEXT DEFAULT 'estimated'
            );
            CREATE TABLE tool_usage (
                id INTEGER PRIMARY KEY AUTOINCREMENT, message_uuid TEXT NOT NULL, tool_name TEXT NOT NULL,
                FOREIGN KEY (message_uuid) REFERENCES messages(uuid) ON DELETE CASCADE
            );
            CREATE TABLE tags (
                id INTEGER PRIMARY KEY AUTOINCREMENT, message_uuid TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL,
                UNIQUE(message_uuid, key, value),
                FOREIGN KEY (message_uuid) REFERENCES messages(uuid) ON DELETE CASCADE
            );
            CREATE TABLE sync_state (file_path TEXT PRIMARY KEY, byte_offset INTEGER NOT NULL DEFAULT 0, last_synced TEXT NOT NULL);
            CREATE TABLE sessions (
                conversation_id TEXT PRIMARY KEY, provider TEXT NOT NULL DEFAULT 'claude_code',
                started_at TEXT, ended_at TEXT, duration_ms INTEGER, composer_mode TEXT,
                permission_mode TEXT, user_email TEXT, workspace_root TEXT, end_reason TEXT,
                prompt_category TEXT, model TEXT, raw_json TEXT, repo_id TEXT, git_branch TEXT
            );
            CREATE TABLE hook_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT, provider TEXT NOT NULL, event TEXT NOT NULL,
                conversation_id TEXT, timestamp TEXT NOT NULL, model TEXT, tool_name TEXT,
                tool_duration_ms INTEGER, tool_call_count INTEGER, raw_json TEXT, mcp_server TEXT
            );
            -- Insert orphaned data (FK disabled, so this succeeds)
            INSERT INTO tags (message_uuid, key, value) VALUES ('orphan_msg', 'provider', 'claude_code');
            INSERT INTO tool_usage (message_uuid, tool_name) VALUES ('orphan_msg', 'Read');
            ",
        )
        .unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        conn.pragma_update(None, "user_version", 7u32).unwrap();

        // This should succeed even with orphaned FK data
        migrate(&conn).unwrap();
        assert_eq!(current_version(&conn), SCHEMA_VERSION);
    }

    #[test]
    fn migrate_v11_to_v12_adds_dedup_index() {
        let conn = Connection::open_in_memory().unwrap();
        // Start at v11 (full schema)
        conn.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
        create_current_schema(&conn).unwrap();
        conn.pragma_update(None, "user_version", 11u32).unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();

        assert!(needs_migration(&conn));
        migrate(&conn).unwrap();
        assert_eq!(current_version(&conn), SCHEMA_VERSION);

        // Verify the dedup index exists
        let has_idx: bool = conn
            .prepare("SELECT count(*) FROM sqlite_master WHERE type='index' AND name='idx_messages_dedup'")
            .unwrap()
            .query_row([], |r| r.get::<_, i64>(0))
            .map(|c| c > 0)
            .unwrap();
        assert!(has_idx, "idx_messages_dedup should exist after v11→v12 migration");
    }
}
