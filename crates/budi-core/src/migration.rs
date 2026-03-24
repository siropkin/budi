//! Database schema migration for the analytics SQLite database.
//!
//! Migrations run explicitly via `budi update` or `budi sync`, not on every `open_db()`.

use anyhow::Result;
use rusqlite::Connection;

/// Expected schema version for the current binary.
pub const SCHEMA_VERSION: u32 = 2;

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
                session_id       TEXT PRIMARY KEY,
                project_dir      TEXT,
                first_seen       TEXT NOT NULL,
                last_seen        TEXT NOT NULL,
                version          TEXT,
                git_branch       TEXT,
                repo_id          TEXT,
                provider         TEXT DEFAULT 'claude_code',
                session_title    TEXT,
                interaction_mode TEXT,
                lines_added      INTEGER DEFAULT 0,
                lines_removed    INTEGER DEFAULT 0,
                user_name        TEXT,
                machine_name     TEXT,
                git_author_name  TEXT,
                git_author_email TEXT,
                git_enriched_at  TEXT
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
                repo_id                TEXT,
                provider               TEXT DEFAULT 'claude_code',
                cost_cents             REAL,
                context_tokens_used    INTEGER,
                context_token_limit    INTEGER,
                interaction_mode       TEXT,
                parent_uuid            TEXT,
                FOREIGN KEY (session_id) REFERENCES sessions(session_id)
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
                FOREIGN KEY (message_uuid) REFERENCES messages(uuid)
            );

            CREATE TABLE IF NOT EXISTS commits (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id    TEXT NOT NULL,
                hash          TEXT NOT NULL,
                author_name   TEXT,
                author_email  TEXT,
                timestamp     TEXT NOT NULL,
                message       TEXT,
                lines_added   INTEGER NOT NULL DEFAULT 0,
                lines_removed INTEGER NOT NULL DEFAULT 0,
                pr_number     INTEGER,
                ai_created    INTEGER NOT NULL DEFAULT 0,
                FOREIGN KEY (session_id) REFERENCES sessions(session_id)
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
            CREATE INDEX IF NOT EXISTS idx_sessions_repo ON sessions(repo_id);
            CREATE INDEX IF NOT EXISTS idx_sessions_provider ON sessions(provider);
            CREATE INDEX IF NOT EXISTS idx_sessions_title ON sessions(session_title);
            CREATE INDEX IF NOT EXISTS idx_tags_key_value ON tags(key, value);
            CREATE INDEX IF NOT EXISTS idx_tags_message ON tags(message_uuid);
            CREATE INDEX IF NOT EXISTS idx_commits_session ON commits(session_id);
            CREATE INDEX IF NOT EXISTS idx_commits_hash ON commits(hash);
            CREATE INDEX IF NOT EXISTS idx_commits_pr ON commits(pr_number);
            ",
        )?;
        needs_tag_backfill = true;
    }

    if version < 2 {
        // Deduplicate tags: remove duplicate (message_uuid, key, value) rows,
        // then recreate the table with a UNIQUE constraint.
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

    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(needs_tag_backfill)
}
