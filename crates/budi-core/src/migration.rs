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

    if version >= SCHEMA_VERSION {
        return Ok(false);
    }

    // Disable FK checks during migration (table rebuilds may temporarily
    // violate constraints before the replacement table is in place).
    conn.execute_batch("PRAGMA foreign_keys=OFF;")?;

    if version == 0 {
        // ── Fresh install ──────────────────────────────────────────────
        create_current_schema(conn)?;
        needs_tag_backfill = true;
    } else if version <= 6 {
        // ── Upgrade from v6.0.0 release (user_version 1–6) ────────────
        upgrade_from_v6(conn)?;
        needs_tag_backfill = true;
    }

    // Future migrations go here as: if version < N { ... }

    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    conn.execute_batch("PRAGMA foreign_keys=ON;")?;
    Ok(needs_tag_backfill)
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
            FOREIGN KEY (message_uuid) REFERENCES messages(uuid) ON DELETE CASCADE
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

// ── Upgrade from v6.0.0 ───────────────────────────────────────────────
//
// v6.0.0 schema (user_version 1–6):
//   sessions: session_id, project_dir, first_seen, last_seen, version,
//             git_branch, repo_id, provider, session_title, interaction_mode,
//             lines_added, lines_removed, user_name, machine_name
//   messages: uuid, session_id, role, timestamp, model, input_tokens,
//             output_tokens, cache_creation_tokens, cache_read_tokens,
//             has_thinking, stop_reason, text_length, cwd, repo_id, provider,
//             cost_cents, context_tokens_used, context_token_limit, interaction_mode
//   tool_usage: id, message_uuid, tool_name
//   sync_state: file_path, byte_offset, last_synced

fn upgrade_from_v6(conn: &Connection) -> Result<()> {
    // Step 1: Create tags table.
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS tags (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            message_uuid TEXT NOT NULL,
            key          TEXT NOT NULL,
            value        TEXT NOT NULL,
            UNIQUE(message_uuid, key, value),
            FOREIGN KEY (message_uuid) REFERENCES messages(uuid) ON DELETE CASCADE
        );
        ",
    )?;

    // Step 2: Migrate useful session metadata to tags on first message of each session.
    conn.execute_batch(
        "
        WITH first_msgs AS (
            SELECT session_id, MIN(timestamp) as min_ts FROM messages GROUP BY session_id
        )
        INSERT OR IGNORE INTO tags (message_uuid, key, value)
        SELECT m.uuid, 'session_title', s.session_title
        FROM sessions s
        JOIN first_msgs fm ON fm.session_id = s.session_id
        JOIN messages m ON m.session_id = s.session_id AND m.timestamp = fm.min_ts
        WHERE s.session_title IS NOT NULL AND s.session_title != '';

        WITH first_msgs AS (
            SELECT session_id, MIN(timestamp) as min_ts FROM messages GROUP BY session_id
        )
        INSERT OR IGNORE INTO tags (message_uuid, key, value)
        SELECT m.uuid, 'branch', s.git_branch
        FROM sessions s
        JOIN first_msgs fm ON fm.session_id = s.session_id
        JOIN messages m ON m.session_id = s.session_id AND m.timestamp = fm.min_ts
        WHERE s.git_branch IS NOT NULL AND s.git_branch != '';
        ",
    )?;

    // Step 3: Rebuild messages table — drop old columns, add new ones.
    conn.execute_batch(
        "
        CREATE TABLE messages_new (
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
        INSERT INTO messages_new (
            uuid, session_id, role, timestamp, model,
            input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
            cwd, repo_id, provider, cost_cents,
            context_tokens_used, context_token_limit
        )
        SELECT
            uuid, session_id, role, timestamp, model,
            input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
            cwd, repo_id, provider, cost_cents,
            context_tokens_used, context_token_limit
        FROM messages;
        DROP TABLE messages;
        ALTER TABLE messages_new RENAME TO messages;
        ",
    )?;

    // Step 4: Backfill git_branch from session tags we just created.
    conn.execute_batch(
        "
        UPDATE messages SET git_branch = (
            SELECT t.value FROM tags t
            WHERE t.message_uuid = messages.uuid AND t.key = 'branch'
            LIMIT 1
        ) WHERE git_branch IS NULL;
        ",
    )?;

    // Step 5: Backfill cost_confidence for existing data.
    conn.execute_batch(
        "
        UPDATE messages SET cost_confidence = 'estimated'
          WHERE provider = 'cursor' AND (cost_cents IS NULL OR cost_cents = 0);
        UPDATE messages SET cost_confidence = 'exact_cost'
          WHERE provider = 'cursor' AND cost_cents IS NOT NULL AND cost_cents > 0;
        UPDATE messages SET cost_confidence = 'estimated'
          WHERE provider != 'cursor' AND role = 'assistant';
        UPDATE messages SET cost_confidence = 'estimated'
          WHERE role = 'user';
        ",
    )?;

    // Step 6: Drop old sessions table, create new sessions + hook_events.
    conn.execute_batch("DROP TABLE IF EXISTS sessions;")?;
    create_sessions_and_hook_events(conn)?;

    // Step 7: Backfill new sessions from existing messages.
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

    // Step 8: Drop legacy tables, create indexes.
    conn.execute_batch("DROP TABLE IF EXISTS commits;")?;
    create_indexes(conn)?;

    // Step 9: Reset sync_state to force full re-sync (pipeline enrichers will
    // populate tags properly on re-ingestion).
    conn.execute_batch("DELETE FROM sync_state;")?;

    Ok(())
}

// ── Shared helpers ─────────────────────────────────────────────────────

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
        CREATE INDEX IF NOT EXISTS idx_messages_role_ts ON messages(role, timestamp);
        CREATE INDEX IF NOT EXISTS idx_messages_cwd ON messages(cwd);

        -- tool_usage
        CREATE INDEX IF NOT EXISTS idx_tool_usage_message ON tool_usage(message_uuid);
        CREATE INDEX IF NOT EXISTS idx_tool_usage_name ON tool_usage(tool_name);

        -- tags
        CREATE INDEX IF NOT EXISTS idx_tags_key_value ON tags(key, value);
        CREATE INDEX IF NOT EXISTS idx_tags_message ON tags(message_uuid);

        -- sessions
        CREATE INDEX IF NOT EXISTS idx_sessions_provider ON sessions(provider);
        CREATE INDEX IF NOT EXISTS idx_sessions_started ON sessions(started_at);
        CREATE INDEX IF NOT EXISTS idx_sessions_conversation ON sessions(conversation_id);

        -- hook_events
        CREATE INDEX IF NOT EXISTS idx_hook_events_conversation ON hook_events(conversation_id);
        CREATE INDEX IF NOT EXISTS idx_hook_events_timestamp ON hook_events(timestamp);
        CREATE INDEX IF NOT EXISTS idx_hook_events_event ON hook_events(event);
        CREATE INDEX IF NOT EXISTS idx_hook_events_provider ON hook_events(provider);
        CREATE INDEX IF NOT EXISTS idx_hook_events_event_timestamp ON hook_events(event, timestamp);
        CREATE INDEX IF NOT EXISTS idx_hook_events_event_tool ON hook_events(event, tool_name);
        CREATE INDEX IF NOT EXISTS idx_hook_events_event_conversation ON hook_events(event, conversation_id);
        CREATE INDEX IF NOT EXISTS idx_hook_events_mcp_server ON hook_events(mcp_server);
        ",
    )?;
    Ok(())
}
