//! Database schema migration for the analytics SQLite database.
//!
//! Migrations run explicitly via `budi update` or `budi sync`, not on every `open_db()`.

use anyhow::Result;
use rusqlite::Connection;

/// Expected schema version for the current binary.
pub const SCHEMA_VERSION: u32 = 6;

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

    // Disable FK checks during migration (table rebuilds may temporarily
    // violate constraints before the replacement table is in place).
    if version < SCHEMA_VERSION {
        conn.execute_batch("PRAGMA foreign_keys=OFF;")?;
    }

    if version < 1 {
        // Fresh database — create message-first schema (v6).
        // No sessions table; session data lives in tags.
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
                context_tokens_used    INTEGER,
                context_token_limit    INTEGER,
                parent_uuid            TEXT
            );

            CREATE TABLE IF NOT EXISTS tool_usage (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                message_uuid TEXT NOT NULL,
                tool_name    TEXT NOT NULL,
                FOREIGN KEY (message_uuid) REFERENCES messages(uuid)
            );

            CREATE TABLE IF NOT EXISTS tags (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                message_uuid TEXT NOT NULL,
                key          TEXT NOT NULL,
                value        TEXT NOT NULL,
                UNIQUE(message_uuid, key, value),
                FOREIGN KEY (message_uuid) REFERENCES messages(uuid)
            );

            CREATE TABLE IF NOT EXISTS sync_state (
                file_path    TEXT PRIMARY KEY,
                byte_offset  INTEGER NOT NULL DEFAULT 0,
                last_synced  TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id);
            CREATE INDEX IF NOT EXISTS idx_messages_timestamp ON messages(timestamp);
            CREATE INDEX IF NOT EXISTS idx_messages_session_ts ON messages(session_id, timestamp);
            CREATE INDEX IF NOT EXISTS idx_messages_repo ON messages(repo_id);
            CREATE INDEX IF NOT EXISTS idx_messages_provider ON messages(provider);
            CREATE INDEX IF NOT EXISTS idx_messages_parent ON messages(parent_uuid);
            CREATE INDEX IF NOT EXISTS idx_tool_usage_message ON tool_usage(message_uuid);
            CREATE INDEX IF NOT EXISTS idx_tool_usage_name ON tool_usage(tool_name);
            CREATE INDEX IF NOT EXISTS idx_tags_key_value ON tags(key, value);
            CREATE INDEX IF NOT EXISTS idx_tags_message ON tags(message_uuid);
            ",
        )?;
        needs_tag_backfill = true;
    }

    // Legacy migrations for existing databases (v1→v5).
    // These run on databases created before v6 which still have sessions tables.

    if version >= 1 && version < 2 {
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS tags_new (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                message_uuid TEXT NOT NULL,
                key          TEXT NOT NULL,
                value        TEXT NOT NULL,
                UNIQUE(message_uuid, key, value),
                FOREIGN KEY (message_uuid) REFERENCES messages(uuid)
            );
            INSERT OR IGNORE INTO tags_new (message_uuid, key, value)
                SELECT message_uuid, key, value FROM tags;
            DROP TABLE tags;
            ALTER TABLE tags_new RENAME TO tags;
            CREATE INDEX IF NOT EXISTS idx_tags_key_value ON tags(key, value);
            CREATE INDEX IF NOT EXISTS idx_tags_message ON tags(message_uuid);
            ",
        )?;
    }

    if version >= 1 && version < 3 {
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS sessions_new (
                session_id       TEXT PRIMARY KEY,
                project_dir      TEXT,
                first_seen       TEXT NOT NULL,
                last_seen        TEXT NOT NULL,
                version          TEXT,
                git_branch       TEXT,
                repo_id          TEXT,
                provider         TEXT DEFAULT 'claude_code',
                session_title    TEXT,
                lines_added      INTEGER DEFAULT 0,
                lines_removed    INTEGER DEFAULT 0,
                user_name        TEXT,
                machine_name     TEXT
            );
            INSERT OR REPLACE INTO sessions_new
                SELECT session_id, project_dir, first_seen, last_seen, version,
                       git_branch, repo_id, provider, session_title,
                       lines_added, lines_removed, user_name, machine_name
                FROM sessions;
            DROP TABLE sessions;
            ALTER TABLE sessions_new RENAME TO sessions;
            CREATE INDEX IF NOT EXISTS idx_sessions_repo ON sessions(repo_id);
            CREATE INDEX IF NOT EXISTS idx_sessions_provider ON sessions(provider);
            CREATE INDEX IF NOT EXISTS idx_sessions_title ON sessions(session_title);
            ",
        )?;
    }

    if version >= 1 && version < 4 {
        conn.execute_batch(
            "
            DROP TABLE IF EXISTS commits;
            ",
        )?;
    }

    if version >= 1 && version < 5 {
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS sessions_v5 (
                session_id       TEXT PRIMARY KEY,
                project_dir      TEXT,
                first_seen       TEXT NOT NULL,
                last_seen        TEXT NOT NULL,
                version          TEXT,
                git_branch       TEXT,
                repo_id          TEXT,
                provider         TEXT DEFAULT 'claude_code',
                session_title    TEXT,
                lines_added      INTEGER DEFAULT 0,
                lines_removed    INTEGER DEFAULT 0,
                user_name        TEXT,
                machine_name     TEXT
            );
            INSERT OR REPLACE INTO sessions_v5
                SELECT session_id, project_dir, first_seen, last_seen, version,
                       git_branch, repo_id, provider, session_title,
                       lines_added, lines_removed, user_name, machine_name
                FROM sessions;
            DROP TABLE sessions;
            ALTER TABLE sessions_v5 RENAME TO sessions;
            CREATE INDEX IF NOT EXISTS idx_sessions_repo ON sessions(repo_id);
            CREATE INDEX IF NOT EXISTS idx_sessions_provider ON sessions(provider);
            CREATE INDEX IF NOT EXISTS idx_sessions_title ON sessions(session_title);
            DROP TABLE IF EXISTS commits;
            ",
        )?;
    }

    if version >= 1 && version < 6 {
        // v6: Message-first migration.
        // 1. Migrate session metadata to tags on the first message of each session.
        // 2. Rebuild messages table without dropped columns.
        // 3. Drop sessions table.

        // Step 1: Migrate session fields to tags
        conn.execute_batch(
            "
            INSERT OR IGNORE INTO tags (message_uuid, key, value)
            SELECT m.uuid, 'session_title', s.session_title
            FROM sessions s
            JOIN messages m ON m.session_id = s.session_id
            WHERE s.session_title IS NOT NULL AND s.session_title != ''
            AND m.timestamp = (SELECT MIN(m2.timestamp) FROM messages m2 WHERE m2.session_id = s.session_id);

            INSERT OR IGNORE INTO tags (message_uuid, key, value)
            SELECT m.uuid, 'branch', s.git_branch
            FROM sessions s
            JOIN messages m ON m.session_id = s.session_id
            WHERE s.git_branch IS NOT NULL AND s.git_branch != ''
            AND m.timestamp = (SELECT MIN(m2.timestamp) FROM messages m2 WHERE m2.session_id = s.session_id);

            INSERT OR IGNORE INTO tags (message_uuid, key, value)
            SELECT m.uuid, 'user', s.user_name
            FROM sessions s
            JOIN messages m ON m.session_id = s.session_id
            WHERE s.user_name IS NOT NULL AND s.user_name != ''
            AND m.timestamp = (SELECT MIN(m2.timestamp) FROM messages m2 WHERE m2.session_id = s.session_id);

            INSERT OR IGNORE INTO tags (message_uuid, key, value)
            SELECT m.uuid, 'machine', s.machine_name
            FROM sessions s
            JOIN messages m ON m.session_id = s.session_id
            WHERE s.machine_name IS NOT NULL AND s.machine_name != ''
            AND m.timestamp = (SELECT MIN(m2.timestamp) FROM messages m2 WHERE m2.session_id = s.session_id);
            ",
        )?;

        // Step 2: Rebuild messages table without dropped columns
        conn.execute_batch(
            "
            CREATE TABLE messages_v6 (
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
                context_tokens_used    INTEGER,
                context_token_limit    INTEGER,
                parent_uuid            TEXT
            );
            INSERT INTO messages_v6
                SELECT uuid, session_id, role, timestamp, model,
                       input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
                       cwd, repo_id, provider, cost_cents,
                       context_tokens_used, context_token_limit, parent_uuid
                FROM messages;
            DROP TABLE messages;
            ALTER TABLE messages_v6 RENAME TO messages;

            CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id);
            CREATE INDEX IF NOT EXISTS idx_messages_timestamp ON messages(timestamp);
            CREATE INDEX IF NOT EXISTS idx_messages_session_ts ON messages(session_id, timestamp);
            CREATE INDEX IF NOT EXISTS idx_messages_repo ON messages(repo_id);
            CREATE INDEX IF NOT EXISTS idx_messages_provider ON messages(provider);
            CREATE INDEX IF NOT EXISTS idx_messages_parent ON messages(parent_uuid);
            ",
        )?;

        // Step 3: Drop sessions table
        conn.execute_batch(
            "
            DROP TABLE IF EXISTS sessions;
            ",
        )?;

        needs_tag_backfill = true;
    }

    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    conn.execute_batch("PRAGMA foreign_keys=ON;")?;
    Ok(needs_tag_backfill)
}
