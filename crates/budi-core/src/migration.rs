//! Database schema migration for the analytics SQLite database.
//!
//! Migrations run explicitly via `budi update` or `budi sync`, not on every `open_db()`.

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
                parent_uuid            TEXT,
                git_branch             TEXT,
                cost_confidence        TEXT DEFAULT 'exact'
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
            CREATE INDEX IF NOT EXISTS idx_messages_branch ON messages(git_branch);
            CREATE INDEX IF NOT EXISTS idx_tool_usage_message ON tool_usage(message_uuid);
            CREATE INDEX IF NOT EXISTS idx_tool_usage_name ON tool_usage(tool_name);
            CREATE INDEX IF NOT EXISTS idx_tags_key_value ON tags(key, value);
            CREATE INDEX IF NOT EXISTS idx_tags_message ON tags(message_uuid);
            CREATE INDEX IF NOT EXISTS idx_messages_cwd ON messages(cwd);
            ",
        )?;
        create_sessions_and_hook_events(conn)?;
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

    if version >= 1 && version < 7 {
        // v7: Add sessions and hook_events tables for hooks integration.
        create_sessions_and_hook_events(conn)?;

        // Backfill sessions from existing messages.
        conn.execute_batch(
            "
            INSERT OR IGNORE INTO sessions (conversation_id, provider, started_at, model)
            SELECT
                m.session_id,
                m.provider,
                MIN(m.timestamp),
                (SELECT m2.model FROM messages m2
                 WHERE m2.session_id = m.session_id AND m2.role = 'assistant' AND m2.model IS NOT NULL
                 ORDER BY m2.timestamp ASC LIMIT 1)
            FROM messages m
            WHERE m.session_id IS NOT NULL
            GROUP BY m.session_id, m.provider;
            ",
        )?;
    }

    if version >= 1 && version < 8 {
        // v8: Add git_branch column to messages (denormalized from tags for fast queries).
        // Add repo_id + git_branch to sessions, mcp_server to hook_events.
        conn.execute_batch(
            "
            ALTER TABLE messages ADD COLUMN git_branch TEXT;
            UPDATE messages SET git_branch = (
                SELECT t.value FROM tags t
                WHERE t.message_uuid = messages.uuid AND t.key = 'branch'
                LIMIT 1
            ) WHERE git_branch IS NULL;
            CREATE INDEX IF NOT EXISTS idx_messages_branch ON messages(git_branch);

            ALTER TABLE sessions ADD COLUMN repo_id TEXT;
            ALTER TABLE sessions ADD COLUMN git_branch TEXT;

            ALTER TABLE hook_events ADD COLUMN mcp_server TEXT;
            ",
        )?;
    }

    if version >= 1 && version < 9 {
        // v9: Add cost_confidence column to messages.
        conn.execute_batch(
            "
            ALTER TABLE messages ADD COLUMN cost_confidence TEXT DEFAULT 'exact';

            -- Backfill: Claude Code messages are exact (tokens from JSONL).
            UPDATE messages SET cost_confidence = 'exact' WHERE provider != 'cursor';
            -- Cursor messages with cost from composerData are exact_cost (cost known, tokens estimated).
            UPDATE messages SET cost_confidence = 'exact_cost'
              WHERE provider = 'cursor' AND cost_cents IS NOT NULL AND cost_cents > 0;
            -- Cursor messages without cost are estimated.
            UPDATE messages SET cost_confidence = 'estimated'
              WHERE provider = 'cursor' AND (cost_cents IS NULL OR cost_cents = 0);
            ",
        )?;
    }

    if version >= 1 && version < 10 {
        // v10: Add missing indexes, drop unused index, fix Cursor cost_confidence backfill.
        conn.execute_batch(
            "
            CREATE INDEX IF NOT EXISTS idx_messages_cwd ON messages(cwd);
            CREATE INDEX IF NOT EXISTS idx_hook_events_event_timestamp ON hook_events(event, timestamp);
            DROP INDEX IF EXISTS idx_sessions_title;

            -- Fix v8→v9 migration: Cursor JSONL fallback messages were marked 'exact'
            -- but have no cost data. They should be 'estimated'.
            UPDATE messages SET cost_confidence = 'estimated'
              WHERE provider = 'cursor' AND cost_confidence = 'exact' AND cost_cents IS NULL;
            ",
        )?;
    }

    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    conn.execute_batch("PRAGMA foreign_keys=ON;")?;
    Ok(needs_tag_backfill)
}

/// Create sessions and hook_events tables (used by both fresh install and v6→v7 migration).
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
            context_tokens      INTEGER,
            context_window_size INTEGER,
            context_usage_pct   REAL,
            message_count       INTEGER,
            subagent_type       TEXT,
            tool_call_count     INTEGER,
            loop_count          INTEGER,
            files_json          TEXT,
            raw_json            TEXT,
            mcp_server          TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_sessions_provider ON sessions(provider);
        CREATE INDEX IF NOT EXISTS idx_sessions_started ON sessions(started_at);
        CREATE INDEX IF NOT EXISTS idx_hook_events_conversation ON hook_events(conversation_id);
        CREATE INDEX IF NOT EXISTS idx_hook_events_timestamp ON hook_events(timestamp);
        CREATE INDEX IF NOT EXISTS idx_hook_events_event ON hook_events(event);
        CREATE INDEX IF NOT EXISTS idx_hook_events_provider ON hook_events(provider);
        CREATE INDEX IF NOT EXISTS idx_hook_events_event_timestamp ON hook_events(event, timestamp);
        ",
    )?;
    Ok(())
}
