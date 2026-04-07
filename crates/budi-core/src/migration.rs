//! Database schema migration for the analytics SQLite database.
//!
//! Three migration paths:
//!   1. Fresh install (user_version = 0) → create current schema from scratch.
//!   2. Upgrade from pre-stable version (1–9) → drop all tables, recreate from scratch.
//!   3. Upgrade from stable version (≥ 10) → incremental migrations.
//!
//! Migrations run explicitly via `budi sync` or daemon auto-sync, not on every `open_db()`.

use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::Connection;

/// Expected schema version for the current binary.
pub const SCHEMA_VERSION: u32 = 20;

/// Result of running schema repair.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RepairReport {
    pub from_version: u32,
    pub to_version: u32,
    pub migrated: bool,
    pub added_columns: Vec<String>,
    pub added_indexes: Vec<String>,
}

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
/// Returns `true` if migration is needed, `false` if not, or `true` if the
/// database cannot be opened (erring on the side of attempting migration).
pub fn needs_migration_at(db_path: &std::path::Path) -> bool {
    match Connection::open(db_path) {
        Ok(conn) => needs_migration(&conn),
        Err(e) => {
            tracing::warn!("Cannot open database at {}: {e}", db_path.display());
            true
        }
    }
}

/// Run all pending migrations up to SCHEMA_VERSION.
pub fn migrate(conn: &Connection) -> Result<()> {
    run_version_migrations(conn)?;
    let _ = reconcile_schema(conn)?;
    Ok(())
}

/// Run migrations and reconcile additive schema drift.
///
/// This is safe to run repeatedly. It upgrades old schema versions and repairs
/// missing additive columns on already-current schemas.
pub fn repair(conn: &Connection) -> Result<RepairReport> {
    let from_version = current_version(conn);
    run_version_migrations(conn)?;
    let reconcile = reconcile_schema(conn)?;
    let to_version = current_version(conn);
    Ok(RepairReport {
        from_version,
        to_version,
        migrated: from_version < to_version,
        added_columns: reconcile.added_columns,
        added_indexes: reconcile.added_indexes,
    })
}

fn run_version_migrations(conn: &Connection) -> Result<()> {
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
    if current_version(conn) == 12 {
        migrate_v12_to_v13(conn)?;
    }
    if current_version(conn) == 13 {
        migrate_v13_to_v14(conn)?;
    }
    if current_version(conn) == 14 {
        migrate_v14_to_v15(conn)?;
    }
    if current_version(conn) == 15 {
        migrate_v15_to_v16(conn)?;
    }
    if current_version(conn) == 16 {
        migrate_v16_to_v17(conn)?;
    }
    if current_version(conn) == 17 {
        migrate_v17_to_v18(conn)?;
    }
    if current_version(conn) == 18 {
        migrate_v18_to_v19(conn)?;
    }
    if current_version(conn) == 19 {
        migrate_v19_to_v20(conn)?;
    }
    Ok(())
}

/// Drop all user tables so the schema can be recreated from scratch.
fn drop_all_tables(conn: &Connection) -> Result<()> {
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")?
        .query_map([], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    for table in tables {
        let safe_name = table.replace('"', "\"\"");
        conn.execute_batch(&format!("DROP TABLE IF EXISTS \"{safe_name}\";"))?;
    }
    Ok(())
}

// ── Fresh install schema ───────────────────────────────────────────────

fn create_current_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS messages (
            id                     TEXT PRIMARY KEY,
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
            cost_confidence        TEXT DEFAULT 'estimated',
            request_id             TEXT
        );

        CREATE TABLE IF NOT EXISTS tags (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            message_id   TEXT NOT NULL,
            key          TEXT NOT NULL,
            value        TEXT NOT NULL,
            UNIQUE(message_id, key, value),
            FOREIGN KEY (message_id) REFERENCES messages(id) ON DELETE CASCADE
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
/// for cost queries) and `sessions` (metadata context from any source).
/// Messages are the source of truth for cost queries.
fn create_sessions_and_hook_events(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS sessions (
            id                 TEXT PRIMARY KEY,
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
            git_branch         TEXT,
            title              TEXT
        );

        CREATE TABLE IF NOT EXISTS hook_events (
            id                  INTEGER PRIMARY KEY AUTOINCREMENT,
            provider            TEXT NOT NULL,
            event               TEXT NOT NULL,
            session_id          TEXT,
            timestamp           TEXT NOT NULL,
            model               TEXT,
            tool_name           TEXT,
            tool_duration_ms    INTEGER,
            tool_call_count     INTEGER,
            raw_json            TEXT,
            mcp_server          TEXT,
            message_id          TEXT,
            message_request_id  TEXT,
            tool_use_id         TEXT,
            link_confidence     TEXT
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
            processed   INTEGER NOT NULL DEFAULT 0,
            message_id TEXT,
            timestamp_nano TEXT,
            model TEXT,
            cost_usd_reported REAL,
            cost_cents_computed REAL
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
    conn.pragma_update(None, "user_version", 11u32)?;
    Ok(())
}

/// Incremental migration from v11 to v12: add composite dedup index for OTEL/JSONL matching.
fn migrate_v11_to_v12(conn: &Connection) -> Result<()> {
    tracing::info!("Migrating schema v11 → v12: adding dedup index for OTEL/JSONL matching");
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_messages_dedup
            ON messages(session_id, model, role, cost_confidence, timestamp);",
    )?;
    conn.pragma_update(None, "user_version", 12u32)?;
    Ok(())
}

/// Incremental migration from v12 to v13: add request_id column for cross-parse dedup.
///
/// Also deduplicates existing rows that were created by the cross-parse dedup bug:
/// when Claude Code streams a multi-content-block response, intermediate JSONL entries
/// (with full cache_read but partial output_tokens) could be ingested alongside the
/// final entry if budi synced mid-stream. This inflates cache_read tokens.
fn migrate_v12_to_v13(conn: &Connection) -> Result<()> {
    tracing::info!(
        "Migrating schema v12 → v13: adding request_id column + deduplicating stale rows"
    );

    // Add request_id column
    conn.execute_batch(
        "ALTER TABLE messages ADD COLUMN request_id TEXT;
         CREATE INDEX IF NOT EXISTS idx_messages_request_id ON messages(request_id) WHERE request_id IS NOT NULL;",
    )?;

    // Deduplicate existing data: find rows that are likely duplicates from the
    // cross-parse bug. Two assistant rows in the same session, same model, within
    // ±1 second, with identical input_tokens + cache_read_tokens but different
    // output_tokens — the one with fewer output_tokens is the stale intermediate.
    let deleted: usize = conn.execute(
        "DELETE FROM messages WHERE uuid IN (
            SELECT m1.uuid FROM messages m1
            INNER JOIN messages m2
                ON m1.session_id = m2.session_id
                AND m1.model = m2.model
                AND m1.role = 'assistant'
                AND m2.role = 'assistant'
                AND m1.uuid != m2.uuid
                AND m1.input_tokens = m2.input_tokens
                AND m1.cache_read_tokens = m2.cache_read_tokens
                AND m1.cache_creation_tokens = m2.cache_creation_tokens
                AND m1.output_tokens < m2.output_tokens
                AND ABS(JULIANDAY(m1.timestamp) - JULIANDAY(m2.timestamp)) < (2.0 / 86400.0)
        )",
        [],
    )?;
    if deleted > 0 {
        tracing::info!("Dedup migration: removed {deleted} stale duplicate rows");
    }

    // Clean up orphaned tags for deleted messages
    conn.execute(
        "DELETE FROM tags WHERE message_uuid NOT IN (SELECT uuid FROM messages)",
        [],
    )?;

    conn.pragma_update(None, "user_version", 13u32)?;
    Ok(())
}

/// Incremental migration from v13 to v14: rename conversation_id → session_id
/// in sessions and hook_events tables for unified session identity.
fn migrate_v13_to_v14(conn: &Connection) -> Result<()> {
    tracing::info!(
        "Migrating schema v13 → v14: renaming conversation_id → session_id in sessions and hook_events"
    );

    // Recreate sessions with session_id as the PK
    conn.execute_batch(
        "
        CREATE TABLE sessions_new (
            session_id         TEXT PRIMARY KEY,
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
        INSERT INTO sessions_new (session_id, provider, started_at, ended_at, duration_ms,
            composer_mode, permission_mode, user_email, workspace_root, end_reason,
            prompt_category, model, raw_json, repo_id, git_branch)
        SELECT conversation_id, provider, started_at, ended_at, duration_ms,
            composer_mode, permission_mode, user_email, workspace_root, end_reason,
            prompt_category, model, raw_json, repo_id, git_branch
        FROM sessions;
        DROP TABLE sessions;
        ALTER TABLE sessions_new RENAME TO sessions;
        ",
    )?;

    // Recreate hook_events with session_id
    conn.execute_batch(
        "
        CREATE TABLE hook_events_new (
            id                  INTEGER PRIMARY KEY AUTOINCREMENT,
            provider            TEXT NOT NULL,
            event               TEXT NOT NULL,
            session_id          TEXT,
            timestamp           TEXT NOT NULL,
            model               TEXT,
            tool_name           TEXT,
            tool_duration_ms    INTEGER,
            tool_call_count     INTEGER,
            raw_json            TEXT,
            mcp_server          TEXT
        );
        INSERT INTO hook_events_new (id, provider, event, session_id, timestamp,
            model, tool_name, tool_duration_ms, tool_call_count, raw_json, mcp_server)
        SELECT id, provider, event, conversation_id, timestamp,
            model, tool_name, tool_duration_ms, tool_call_count, raw_json, mcp_server
        FROM hook_events;
        DROP TABLE hook_events;
        ALTER TABLE hook_events_new RENAME TO hook_events;
        ",
    )?;

    // Rebuild indexes for both tables
    conn.execute_batch(
        "
        CREATE INDEX IF NOT EXISTS idx_sessions_session_id ON sessions(session_id);
        CREATE INDEX IF NOT EXISTS idx_sessions_provider ON sessions(provider);
        CREATE INDEX IF NOT EXISTS idx_sessions_started ON sessions(started_at);

        CREATE INDEX IF NOT EXISTS idx_hook_events_session ON hook_events(session_id);
        CREATE INDEX IF NOT EXISTS idx_hook_events_timestamp ON hook_events(timestamp);
        CREATE INDEX IF NOT EXISTS idx_hook_events_event ON hook_events(event);
        CREATE INDEX IF NOT EXISTS idx_hook_events_provider ON hook_events(provider);
        CREATE INDEX IF NOT EXISTS idx_hook_events_event_timestamp ON hook_events(event, timestamp);
        CREATE INDEX IF NOT EXISTS idx_hook_events_event_tool ON hook_events(event, tool_name);
        CREATE INDEX IF NOT EXISTS idx_hook_events_event_session ON hook_events(event, session_id);
        CREATE INDEX IF NOT EXISTS idx_hook_events_mcp_server ON hook_events(mcp_server);
        CREATE INDEX IF NOT EXISTS idx_hook_events_event_tool_provider ON hook_events(event, tool_name, provider);
        CREATE INDEX IF NOT EXISTS idx_hook_events_event_mcp ON hook_events(event, mcp_server);
        CREATE INDEX IF NOT EXISTS idx_hook_events_event_session_ts ON hook_events(event, session_id, timestamp);
        ",
    )?;

    // Backfill: create stub session rows from messages that have a session_id
    // but no corresponding row in sessions.
    let from_messages: usize = conn.execute(
        "INSERT OR IGNORE INTO sessions (session_id, provider)
         SELECT DISTINCT m.session_id, COALESCE(m.provider, 'claude_code')
         FROM messages m
         WHERE m.session_id IS NOT NULL
           AND m.session_id NOT IN (SELECT session_id FROM sessions)",
        [],
    )?;
    if from_messages > 0 {
        tracing::info!("Session backfill: created {from_messages} stub rows from messages");
    }

    // Backfill: create stub session rows from hook_events that recorded a session_id
    // but never had a session_start event (e.g. Cursor only sends post_tool_use).
    let from_hooks: usize = conn.execute(
        "INSERT OR IGNORE INTO sessions (session_id, provider, started_at)
         SELECT h.session_id, h.provider, MIN(h.timestamp)
         FROM hook_events h
         WHERE h.session_id IS NOT NULL
           AND h.session_id NOT IN (SELECT session_id FROM sessions)
         GROUP BY h.session_id, h.provider",
        [],
    )?;
    if from_hooks > 0 {
        tracing::info!("Session backfill: created {from_hooks} stub rows from hook_events");
    }

    conn.pragma_update(None, "user_version", 14u32)?;
    Ok(())
}

/// Incremental migration from v14 to v15: add `title` column to sessions table.
fn migrate_v14_to_v15(conn: &Connection) -> Result<()> {
    tracing::info!("Migrating schema v14 → v15: adding title column to sessions");
    conn.execute_batch("ALTER TABLE sessions ADD COLUMN title TEXT;")?;
    conn.pragma_update(None, "user_version", 15u32)?;
    Ok(())
}

/// Incremental migration from v15 to v16: normalize provider-prefixed session IDs.
///
/// Historical Cursor transcript parsing prefixed session IDs with `cursor-` when
/// the underlying ID was already a UUID. This migration rewrites those rows to
/// canonical plain UUID form across sessions/messages/hook_events/otel_events.
fn migrate_v15_to_v16(conn: &Connection) -> Result<()> {
    tracing::info!("Migrating schema v15 → v16: normalizing prefixed session IDs");
    normalize_session_ids(conn)?;
    conn.pragma_update(None, "user_version", 16u32)?;
    Ok(())
}

/// Incremental migration from v16 to v17: purge legacy Cursor artifacts.
///
/// Removes synthetic Cursor sessions and legacy non-UUID Cursor message IDs
/// (`cursor-*`) so future syncs rebuild clean data using canonical UUID IDs.
fn migrate_v16_to_v17(conn: &Connection) -> Result<()> {
    tracing::info!("Migrating schema v16 → v17: purging legacy Cursor artifacts");
    let message_id_col = if table_exists(conn, "messages")? && has_column(conn, "messages", "id")? {
        "id"
    } else {
        "uuid"
    };
    let tags_message_col = if table_exists(conn, "tags")? && has_column(conn, "tags", "message_id")?
    {
        "message_id"
    } else {
        "message_uuid"
    };
    let sessions_id_col = if table_exists(conn, "sessions")? && has_column(conn, "sessions", "id")?
    {
        "id"
    } else {
        "session_id"
    };

    let deleted_tags = conn.execute(
        &format!(
            "DELETE FROM tags
         WHERE {tags_message_col} IN (
            SELECT {message_id_col}
            FROM messages
            WHERE provider = 'cursor'
              AND ({message_id_col} LIKE 'cursor-%' OR session_id LIKE 'cursor-synth-%')
         )"
        ),
        [],
    )?;

    let deleted_messages = conn.execute(
        &format!(
            "DELETE FROM messages
         WHERE provider = 'cursor'
           AND ({message_id_col} LIKE 'cursor-%' OR session_id LIKE 'cursor-synth-%')"
        ),
        [],
    )?;

    let deleted_sessions = conn.execute(
        &format!(
            "DELETE FROM sessions
         WHERE provider = 'cursor'
           AND {sessions_id_col} LIKE 'cursor-synth-%'"
        ),
        [],
    )?;

    // Clear Cursor sync watermarks/offsets so next sync rebuilds from source.
    let deleted_sync_state = conn.execute(
        "DELETE FROM sync_state
         WHERE file_path = 'cursor-api-usage'
            OR file_path LIKE '%/agent-transcripts/%'",
        [],
    )?;

    if deleted_tags + deleted_messages + deleted_sessions + deleted_sync_state > 0 {
        tracing::info!(
            "Cursor cleanup: removed {deleted_messages} messages, {deleted_sessions} synthetic sessions, {deleted_tags} tags, reset {deleted_sync_state} sync offsets"
        );
    }

    conn.pragma_update(None, "user_version", 17u32)?;
    Ok(())
}

/// Incremental migration from v17 to v18: add message-centric linkage columns
/// for hook/OTEL event drilldowns and backfill deterministic links.
fn migrate_v17_to_v18(conn: &Connection) -> Result<()> {
    tracing::info!(
        "Migrating schema v17 → v18: adding hook/otel linkage columns and backfilling links"
    );

    let _ = ensure_column(conn, "hook_events", "message_uuid", "message_uuid TEXT")?;
    let _ = ensure_column(
        conn,
        "hook_events",
        "message_request_id",
        "message_request_id TEXT",
    )?;
    let _ = ensure_column(conn, "hook_events", "tool_use_id", "tool_use_id TEXT")?;
    let _ = ensure_column(
        conn,
        "hook_events",
        "link_confidence",
        "link_confidence TEXT",
    )?;

    let _ = ensure_column(conn, "otel_events", "message_uuid", "message_uuid TEXT")?;
    let _ = ensure_column(conn, "otel_events", "timestamp_nano", "timestamp_nano TEXT")?;
    let _ = ensure_column(conn, "otel_events", "model", "model TEXT")?;
    let _ = ensure_column(
        conn,
        "otel_events",
        "cost_usd_reported",
        "cost_usd_reported REAL",
    )?;
    let _ = ensure_column(
        conn,
        "otel_events",
        "cost_cents_computed",
        "cost_cents_computed REAL",
    )?;

    create_indexes(conn)?;
    backfill_hook_event_links(conn)?;
    backfill_otel_event_links(conn)?;

    conn.pragma_update(None, "user_version", 18u32)?;
    Ok(())
}

/// Incremental migration from v18 to v19: rename event linkage columns from
/// `message_uuid` to `message_id` for hook/OTEL events.
fn migrate_v18_to_v19(conn: &Connection) -> Result<()> {
    tracing::info!("Migrating schema v18 → v19: renaming hook/otel linkage columns to message_id");

    if table_exists(conn, "hook_events")?
        && has_column(conn, "hook_events", "message_uuid")?
        && !has_column(conn, "hook_events", "message_id")?
    {
        conn.execute_batch("ALTER TABLE hook_events RENAME COLUMN message_uuid TO message_id;")?;
    }

    if table_exists(conn, "otel_events")?
        && has_column(conn, "otel_events", "message_uuid")?
        && !has_column(conn, "otel_events", "message_id")?
    {
        conn.execute_batch("ALTER TABLE otel_events RENAME COLUMN message_uuid TO message_id;")?;
    }

    conn.execute_batch(
        "
        DROP INDEX IF EXISTS idx_hook_events_message_ts;
        DROP INDEX IF EXISTS idx_otel_events_message_ts;
        ",
    )?;
    create_indexes(conn)?;

    conn.pragma_update(None, "user_version", 19u32)?;
    Ok(())
}

/// Incremental migration from v19 to v20: normalize core identifier column names
/// across messages/sessions/tags.
fn migrate_v19_to_v20(conn: &Connection) -> Result<()> {
    tracing::info!(
        "Migrating schema v19 → v20: renaming messages.uuid→id, sessions.session_id→id, tags.message_uuid→message_id"
    );

    if table_exists(conn, "messages")?
        && has_column(conn, "messages", "uuid")?
        && !has_column(conn, "messages", "id")?
    {
        conn.execute_batch("ALTER TABLE messages RENAME COLUMN uuid TO id;")?;
    }

    if table_exists(conn, "sessions")?
        && has_column(conn, "sessions", "session_id")?
        && !has_column(conn, "sessions", "id")?
    {
        conn.execute_batch("ALTER TABLE sessions RENAME COLUMN session_id TO id;")?;
    }

    if table_exists(conn, "tags")?
        && has_column(conn, "tags", "message_uuid")?
        && !has_column(conn, "tags", "message_id")?
    {
        conn.execute_batch("ALTER TABLE tags RENAME COLUMN message_uuid TO message_id;")?;
    }

    conn.execute_batch(
        "
        DROP INDEX IF EXISTS idx_tags_message;
        DROP INDEX IF EXISTS idx_tags_msg_key_val;
        DROP INDEX IF EXISTS idx_sessions_session_id;
        ",
    )?;
    create_indexes(conn)?;
    conn.pragma_update(None, "user_version", 20u32)?;
    Ok(())
}

fn parse_hook_row_ids(raw_json: Option<&str>) -> (Option<String>, Option<String>) {
    let Some(raw) = raw_json else {
        return (None, None);
    };
    let Ok(value): std::result::Result<serde_json::Value, _> = serde_json::from_str(raw) else {
        return (None, None);
    };
    (
        crate::hooks::extract_hook_message_request_id(&value),
        crate::hooks::extract_hook_tool_use_id(&value),
    )
}

fn backfill_hook_event_links(conn: &Connection) -> Result<()> {
    if !table_exists(conn, "hook_events")? {
        return Ok(());
    }

    let rows: Vec<(i64, Option<String>, Option<String>)> = {
        let mut stmt = conn.prepare(
            "SELECT id, session_id, raw_json
             FROM hook_events
             ORDER BY id ASC",
        )?;
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<std::result::Result<_, _>>()?
    };

    let mut linked_count = 0usize;
    for (id, session_id, raw_json) in &rows {
        let (message_request_id, tool_use_id) = parse_hook_row_ids(raw_json.as_deref());
        let (message_uuid, link_confidence) = crate::hooks::resolve_hook_message_link(
            conn,
            session_id.as_deref(),
            message_request_id.as_deref(),
            tool_use_id.as_deref(),
        )?;
        if message_uuid.is_some() {
            linked_count += 1;
        }

        conn.execute(
            "UPDATE hook_events SET
                message_request_id = COALESCE(message_request_id, ?2),
                tool_use_id = COALESCE(tool_use_id, ?3),
                message_uuid = COALESCE(message_uuid, ?4),
                link_confidence = COALESCE(link_confidence, ?5)
             WHERE id = ?1",
            rusqlite::params![
                id,
                message_request_id,
                tool_use_id,
                message_uuid,
                link_confidence
            ],
        )?;
    }

    if linked_count > 0 {
        tracing::info!("Hook backfill: deterministically linked {linked_count} hook events");
    }
    Ok(())
}

fn extract_otel_snapshot_fields(
    raw_json: Option<&str>,
) -> (Option<String>, Option<String>, Option<f64>) {
    let Some(raw) = raw_json else {
        return (None, None, None);
    };
    let Ok(value): std::result::Result<serde_json::Value, _> = serde_json::from_str(raw) else {
        return (None, None, None);
    };
    let timestamp_nano = value
        .get("timestamp_nano")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let model = value
        .get("model")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let cost_usd_reported = value
        .get("cost_usd_reported")
        .and_then(|v| v.as_f64())
        .or_else(|| value.get("cost_usd").and_then(|v| v.as_f64()));
    (timestamp_nano, model, cost_usd_reported)
}

fn backfill_otel_event_links(conn: &Connection) -> Result<()> {
    if !table_exists(conn, "otel_events")? {
        return Ok(());
    }

    let rows: Vec<(i64, Option<String>, String, Option<String>)> = {
        let mut stmt = conn.prepare(
            "SELECT id, session_id, timestamp, raw_json
             FROM otel_events
             ORDER BY id ASC",
        )?;
        stmt.query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<std::result::Result<_, _>>()?
    };

    let mut linked_count = 0usize;
    let message_id_col = if table_exists(conn, "messages")? && has_column(conn, "messages", "id")? {
        "id"
    } else {
        "uuid"
    };
    for (id, session_id, timestamp, raw_json) in &rows {
        let (timestamp_nano_from_raw, model_from_raw, cost_usd_reported) =
            extract_otel_snapshot_fields(raw_json.as_deref());

        let mut linked_uuid: Option<String> = None;
        let mut linked_model: Option<String> = None;
        let mut linked_cost_cents: Option<f64> = None;

        if let Some(sid) = session_id.as_deref().filter(|s| !s.trim().is_empty())
            && let Ok(event_ts) = DateTime::parse_from_rfc3339(timestamp)
        {
            let event_ts = event_ts.with_timezone(&Utc);
            let ts_lo = (event_ts - chrono::Duration::seconds(1)).to_rfc3339();
            let ts_hi = (event_ts + chrono::Duration::seconds(1)).to_rfc3339();
            let candidates: Vec<(String, String, Option<String>, Option<f64>)> = {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {message_id_col}, timestamp, model, cost_cents
                     FROM messages
                     WHERE session_id = ?1
                       AND role = 'assistant'
                       AND cost_confidence = 'otel_exact'
                       AND timestamp BETWEEN ?2 AND ?3"
                ))?;
                stmt.query_map(rusqlite::params![sid, ts_lo, ts_hi], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })?
                .collect::<std::result::Result<_, _>>()?
            };

            if let Some((uuid, _, model, cost_cents)) =
                candidates
                    .into_iter()
                    .min_by_key(|(_, candidate_ts, _, _)| {
                        DateTime::parse_from_rfc3339(candidate_ts)
                            .map(|dt| {
                                (dt.with_timezone(&Utc).timestamp() - event_ts.timestamp())
                                    .unsigned_abs()
                            })
                            .unwrap_or(u64::MAX)
                    })
            {
                linked_uuid = Some(uuid);
                linked_model = model;
                linked_cost_cents = cost_cents;
                linked_count += 1;
            }
        }

        let model = linked_model.or(model_from_raw);
        conn.execute(
            "UPDATE otel_events SET
                message_uuid = COALESCE(message_uuid, ?2),
                timestamp_nano = COALESCE(timestamp_nano, ?3),
                model = COALESCE(model, ?4),
                cost_usd_reported = COALESCE(cost_usd_reported, ?5),
                cost_cents_computed = COALESCE(cost_cents_computed, ?6)
             WHERE id = ?1",
            rusqlite::params![
                id,
                linked_uuid,
                timestamp_nano_from_raw,
                model,
                cost_usd_reported,
                linked_cost_cents
            ],
        )?;
    }

    if linked_count > 0 {
        tracing::info!("OTEL backfill: linked {linked_count} otel_events to messages");
    }
    Ok(())
}

fn normalize_session_ids(conn: &Connection) -> Result<()> {
    let sessions_id_col = if table_exists(conn, "sessions")? && has_column(conn, "sessions", "id")?
    {
        "id"
    } else {
        "session_id"
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT DISTINCT session_id
         FROM (
            SELECT {sessions_id_col} AS session_id FROM sessions
            UNION ALL
            SELECT session_id FROM messages
            UNION ALL
            SELECT session_id FROM hook_events
            UNION ALL
            SELECT session_id FROM otel_events
         )
         WHERE session_id IS NOT NULL"
    ))?;

    let all_ids: Vec<String> = stmt
        .query_map([], |row| row.get(0))?
        .collect::<std::result::Result<_, _>>()?;

    let mut mappings: Vec<(String, String)> = Vec::new();
    for old in all_ids {
        let normalized = crate::identity::normalize_session_id(&old);
        if !normalized.is_empty() && normalized != old {
            mappings.push((old, normalized));
        }
    }
    mappings.sort_unstable();
    mappings.dedup();

    if mappings.is_empty() {
        return Ok(());
    }

    for (old_id, new_id) in &mappings {
        normalize_single_session_id(conn, old_id, new_id)?;
    }

    tracing::info!(
        "Session ID normalization: rewrote {} id mappings",
        mappings.len()
    );
    Ok(())
}

fn normalize_single_session_id(conn: &Connection, old_id: &str, new_id: &str) -> Result<()> {
    if old_id == new_id {
        return Ok(());
    }

    let sessions_id_col = if table_exists(conn, "sessions")? && has_column(conn, "sessions", "id")?
    {
        "id"
    } else {
        "session_id"
    };

    let old_session_exists: bool = conn.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM sessions WHERE {sessions_id_col} = ?1)"),
        [old_id],
        |r| r.get(0),
    )?;
    let new_session_exists: bool = conn.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM sessions WHERE {sessions_id_col} = ?1)"),
        [new_id],
        |r| r.get(0),
    )?;

    if old_session_exists {
        if new_session_exists {
            merge_session_row(conn, old_id, new_id)?;
            conn.execute(
                &format!("DELETE FROM sessions WHERE {sessions_id_col} = ?1"),
                [old_id],
            )?;
        } else {
            conn.execute(
                &format!("UPDATE sessions SET {sessions_id_col} = ?1 WHERE {sessions_id_col} = ?2"),
                rusqlite::params![new_id, old_id],
            )?;
        }
    }

    conn.execute(
        "UPDATE messages SET session_id = ?1 WHERE session_id = ?2",
        rusqlite::params![new_id, old_id],
    )?;
    conn.execute(
        "UPDATE hook_events SET session_id = ?1 WHERE session_id = ?2",
        rusqlite::params![new_id, old_id],
    )?;
    conn.execute(
        "UPDATE otel_events SET session_id = ?1 WHERE session_id = ?2",
        rusqlite::params![new_id, old_id],
    )?;

    Ok(())
}

fn merge_session_row(conn: &Connection, old_id: &str, new_id: &str) -> Result<()> {
    let sessions_id_col = if table_exists(conn, "sessions")? && has_column(conn, "sessions", "id")?
    {
        "id"
    } else {
        "session_id"
    };
    conn.execute(
        &format!(
            "UPDATE sessions SET
            provider = COALESCE(NULLIF(provider, ''), (SELECT provider FROM sessions WHERE {sessions_id_col} = ?2), 'claude_code'),
            started_at = COALESCE(started_at, (SELECT started_at FROM sessions WHERE {sessions_id_col} = ?2)),
            ended_at = COALESCE(ended_at, (SELECT ended_at FROM sessions WHERE {sessions_id_col} = ?2)),
            duration_ms = COALESCE(duration_ms, (SELECT duration_ms FROM sessions WHERE {sessions_id_col} = ?2)),
            composer_mode = COALESCE(NULLIF(composer_mode, ''), (SELECT composer_mode FROM sessions WHERE {sessions_id_col} = ?2)),
            permission_mode = COALESCE(NULLIF(permission_mode, ''), (SELECT permission_mode FROM sessions WHERE {sessions_id_col} = ?2)),
            user_email = COALESCE(NULLIF(user_email, ''), (SELECT user_email FROM sessions WHERE {sessions_id_col} = ?2)),
            workspace_root = COALESCE(NULLIF(workspace_root, ''), (SELECT workspace_root FROM sessions WHERE {sessions_id_col} = ?2)),
            end_reason = COALESCE(NULLIF(end_reason, ''), (SELECT end_reason FROM sessions WHERE {sessions_id_col} = ?2)),
            prompt_category = COALESCE(NULLIF(prompt_category, ''), (SELECT prompt_category FROM sessions WHERE {sessions_id_col} = ?2)),
            model = COALESCE(NULLIF(model, ''), (SELECT model FROM sessions WHERE {sessions_id_col} = ?2)),
            raw_json = COALESCE(NULLIF(raw_json, ''), (SELECT raw_json FROM sessions WHERE {sessions_id_col} = ?2)),
            repo_id = COALESCE(
                NULLIF(NULLIF(repo_id, ''), 'unknown'),
                NULLIF(NULLIF((SELECT repo_id FROM sessions WHERE {sessions_id_col} = ?2), ''), 'unknown')
            ),
            git_branch = COALESCE(NULLIF(git_branch, ''), (SELECT git_branch FROM sessions WHERE {sessions_id_col} = ?2)),
            title = COALESCE(NULLIF(title, ''), (SELECT title FROM sessions WHERE {sessions_id_col} = ?2))
         WHERE {sessions_id_col} = ?1"
        ),
        rusqlite::params![new_id, old_id],
    )?;
    Ok(())
}

fn create_indexes(conn: &Connection) -> Result<()> {
    let sessions_id_col = if table_exists(conn, "sessions")? && has_column(conn, "sessions", "id")?
    {
        "id"
    } else {
        "session_id"
    };
    let messages_id_col = if table_exists(conn, "messages")? && has_column(conn, "messages", "id")?
    {
        "id"
    } else {
        "uuid"
    };
    let tags_message_col = if table_exists(conn, "tags")? && has_column(conn, "tags", "message_id")?
    {
        "message_id"
    } else {
        "message_uuid"
    };

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
        CREATE INDEX IF NOT EXISTS idx_messages_request_id ON messages(request_id) WHERE request_id IS NOT NULL;

        -- sessions
        CREATE INDEX IF NOT EXISTS idx_sessions_provider ON sessions(provider);
        CREATE INDEX IF NOT EXISTS idx_sessions_started ON sessions(started_at);

        -- hook_events
        CREATE INDEX IF NOT EXISTS idx_hook_events_session ON hook_events(session_id);
        CREATE INDEX IF NOT EXISTS idx_hook_events_timestamp ON hook_events(timestamp);
        CREATE INDEX IF NOT EXISTS idx_hook_events_event ON hook_events(event);
        CREATE INDEX IF NOT EXISTS idx_hook_events_provider ON hook_events(provider);
        CREATE INDEX IF NOT EXISTS idx_hook_events_event_timestamp ON hook_events(event, timestamp);
        CREATE INDEX IF NOT EXISTS idx_hook_events_event_tool ON hook_events(event, tool_name);
        CREATE INDEX IF NOT EXISTS idx_hook_events_event_session ON hook_events(event, session_id);
        CREATE INDEX IF NOT EXISTS idx_hook_events_mcp_server ON hook_events(mcp_server);
        CREATE INDEX IF NOT EXISTS idx_hook_events_event_tool_provider ON hook_events(event, tool_name, provider);
        CREATE INDEX IF NOT EXISTS idx_hook_events_event_mcp ON hook_events(event, mcp_server);
        CREATE INDEX IF NOT EXISTS idx_hook_events_event_session_ts ON hook_events(event, session_id, timestamp);
        CREATE INDEX IF NOT EXISTS idx_hook_events_tool_use_id_partial ON hook_events(tool_use_id) WHERE tool_use_id IS NOT NULL;
        CREATE INDEX IF NOT EXISTS idx_hook_events_message_request_id_partial ON hook_events(message_request_id) WHERE message_request_id IS NOT NULL;

        -- otel_events
        CREATE INDEX IF NOT EXISTS idx_otel_events_session ON otel_events(session_id);
        CREATE INDEX IF NOT EXISTS idx_otel_events_timestamp ON otel_events(timestamp);
        ",
    )?;

    conn.execute_batch(&format!(
        "
        CREATE INDEX IF NOT EXISTS idx_tags_message ON tags({tags_message_col});
        CREATE INDEX IF NOT EXISTS idx_tags_msg_key_val ON tags({tags_message_col}, key, value);
        CREATE INDEX IF NOT EXISTS idx_sessions_id ON sessions({sessions_id_col});
        CREATE INDEX IF NOT EXISTS idx_sessions_session_id ON sessions({sessions_id_col});
        "
    ))?;

    if table_exists(conn, "hook_events")? {
        if has_column(conn, "hook_events", "message_id")? {
            conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_hook_events_message_id_ts ON hook_events(message_id, timestamp);",
            )?;
        } else if has_column(conn, "hook_events", "message_uuid")? {
            conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_hook_events_message_ts ON hook_events(message_uuid, timestamp);",
            )?;
        }
    }

    if table_exists(conn, "otel_events")? {
        if has_column(conn, "otel_events", "message_id")? {
            conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_otel_events_message_id_ts ON otel_events(message_id, timestamp);",
            )?;
        } else if has_column(conn, "otel_events", "message_uuid")? {
            conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_otel_events_message_ts ON otel_events(message_uuid, timestamp);",
            )?;
        }
    }

    if table_exists(conn, "messages")? && table_exists(conn, "tags")? {
        conn.execute_batch(&format!(
            "
            CREATE INDEX IF NOT EXISTS idx_message_tags_pair ON tags({tags_message_col}, key, value);
            CREATE INDEX IF NOT EXISTS idx_messages_primary_id ON messages({messages_id_col});
            "
        ))?;
    }

    Ok(())
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = ?1",
        [table],
        |r| r.get(0),
    )?;
    Ok(count > 0)
}

fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let cols = stmt.query_map([], |row| row.get::<_, String>(1))?;
    Ok(cols.filter_map(|c| c.ok()).any(|c| c == column))
}

fn ensure_column(conn: &Connection, table: &str, column: &str, column_decl: &str) -> Result<bool> {
    if !table_exists(conn, table)? {
        return Ok(false);
    }
    if has_column(conn, table, column)? {
        return Ok(false);
    }

    conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column_decl};"))?;
    tracing::info!("Schema reconcile: added missing {table}.{column}");
    Ok(true)
}

/// Repair additive schema drift when DB claims current version but misses columns.
///
/// This can happen if an old migration partially applied or a table was rebuilt by
/// external tooling while `user_version` remained current. We only add missing
/// columns and recreate indexes; we do not drop or rewrite user data.
struct SchemaReconcileReport {
    added_columns: Vec<String>,
    added_indexes: Vec<String>,
}

fn index_exists(conn: &Connection, name: &str) -> Result<bool> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='index' AND name = ?1)",
        [name],
        |row| row.get(0),
    )?;
    Ok(exists)
}

fn expected_reconcile_indexes(conn: &Connection) -> Result<Vec<String>> {
    let mut indexes = vec![
        "idx_messages_session".to_string(),
        "idx_messages_timestamp".to_string(),
        "idx_messages_session_ts".to_string(),
        "idx_messages_repo".to_string(),
        "idx_messages_provider".to_string(),
        "idx_messages_parent".to_string(),
        "idx_messages_branch".to_string(),
        "idx_messages_role".to_string(),
        "idx_tags_key_value".to_string(),
        "idx_messages_ts_cost".to_string(),
        "idx_messages_role_ts_cost".to_string(),
        "idx_messages_role_branch_cost".to_string(),
        "idx_messages_role_branch_ts".to_string(),
        "idx_messages_role_cwd".to_string(),
        "idx_messages_session_role".to_string(),
        "idx_messages_cwd_role".to_string(),
        "idx_messages_session_role_cost".to_string(),
        "idx_messages_dedup".to_string(),
        "idx_messages_request_id".to_string(),
        "idx_sessions_provider".to_string(),
        "idx_sessions_started".to_string(),
        "idx_hook_events_session".to_string(),
        "idx_hook_events_timestamp".to_string(),
        "idx_hook_events_event".to_string(),
        "idx_hook_events_provider".to_string(),
        "idx_hook_events_event_timestamp".to_string(),
        "idx_hook_events_event_tool".to_string(),
        "idx_hook_events_event_session".to_string(),
        "idx_hook_events_mcp_server".to_string(),
        "idx_hook_events_event_tool_provider".to_string(),
        "idx_hook_events_event_mcp".to_string(),
        "idx_hook_events_event_session_ts".to_string(),
        "idx_hook_events_tool_use_id_partial".to_string(),
        "idx_hook_events_message_request_id_partial".to_string(),
        "idx_otel_events_session".to_string(),
        "idx_otel_events_timestamp".to_string(),
        "idx_tags_message".to_string(),
        "idx_tags_msg_key_val".to_string(),
        "idx_sessions_id".to_string(),
        "idx_sessions_session_id".to_string(),
    ];

    if table_exists(conn, "hook_events")? {
        if has_column(conn, "hook_events", "message_id")? {
            indexes.push("idx_hook_events_message_id_ts".to_string());
        } else if has_column(conn, "hook_events", "message_uuid")? {
            indexes.push("idx_hook_events_message_ts".to_string());
        }
    }

    if table_exists(conn, "otel_events")? {
        if has_column(conn, "otel_events", "message_id")? {
            indexes.push("idx_otel_events_message_id_ts".to_string());
        } else if has_column(conn, "otel_events", "message_uuid")? {
            indexes.push("idx_otel_events_message_ts".to_string());
        }
    }

    if table_exists(conn, "messages")? && table_exists(conn, "tags")? {
        indexes.push("idx_message_tags_pair".to_string());
        indexes.push("idx_messages_primary_id".to_string());
    }

    Ok(indexes)
}

fn missing_reconcile_indexes(conn: &Connection) -> Result<Vec<String>> {
    let mut missing = Vec::new();
    for index in expected_reconcile_indexes(conn)? {
        if !index_exists(conn, &index)? {
            missing.push(index);
        }
    }
    Ok(missing)
}

fn reconcile_schema(conn: &Connection) -> Result<SchemaReconcileReport> {
    let mut added_columns: Vec<String> = Vec::new();

    if ensure_column(
        conn,
        "messages",
        "cost_confidence",
        "cost_confidence TEXT DEFAULT 'estimated'",
    )? {
        added_columns.push("messages.cost_confidence".to_string());
    }
    if ensure_column(conn, "messages", "request_id", "request_id TEXT")? {
        added_columns.push("messages.request_id".to_string());
    }
    if table_exists(conn, "messages")?
        && has_column(conn, "messages", "uuid")?
        && !has_column(conn, "messages", "id")?
    {
        conn.execute_batch("ALTER TABLE messages RENAME COLUMN uuid TO id;")?;
        added_columns.push("messages.id".to_string());
    }

    if ensure_column(conn, "sessions", "title", "title TEXT")? {
        added_columns.push("sessions.title".to_string());
    }
    if table_exists(conn, "sessions")?
        && has_column(conn, "sessions", "session_id")?
        && !has_column(conn, "sessions", "id")?
    {
        conn.execute_batch("ALTER TABLE sessions RENAME COLUMN session_id TO id;")?;
        added_columns.push("sessions.id".to_string());
    }

    if table_exists(conn, "tags")?
        && has_column(conn, "tags", "message_uuid")?
        && !has_column(conn, "tags", "message_id")?
    {
        conn.execute_batch("ALTER TABLE tags RENAME COLUMN message_uuid TO message_id;")?;
        added_columns.push("tags.message_id".to_string());
    }

    if ensure_column(conn, "hook_events", "mcp_server", "mcp_server TEXT")? {
        added_columns.push("hook_events.mcp_server".to_string());
    }
    if table_exists(conn, "hook_events")?
        && has_column(conn, "hook_events", "message_uuid")?
        && !has_column(conn, "hook_events", "message_id")?
    {
        conn.execute_batch("ALTER TABLE hook_events RENAME COLUMN message_uuid TO message_id;")?;
        added_columns.push("hook_events.message_id".to_string());
    }
    if ensure_column(conn, "hook_events", "message_id", "message_id TEXT")? {
        added_columns.push("hook_events.message_id".to_string());
    }
    if ensure_column(
        conn,
        "hook_events",
        "message_request_id",
        "message_request_id TEXT",
    )? {
        added_columns.push("hook_events.message_request_id".to_string());
    }
    if ensure_column(conn, "hook_events", "tool_use_id", "tool_use_id TEXT")? {
        added_columns.push("hook_events.tool_use_id".to_string());
    }
    if ensure_column(
        conn,
        "hook_events",
        "link_confidence",
        "link_confidence TEXT",
    )? {
        added_columns.push("hook_events.link_confidence".to_string());
    }
    if ensure_column(
        conn,
        "otel_events",
        "processed",
        "processed INTEGER NOT NULL DEFAULT 0",
    )? {
        added_columns.push("otel_events.processed".to_string());
    }
    if table_exists(conn, "otel_events")?
        && has_column(conn, "otel_events", "message_uuid")?
        && !has_column(conn, "otel_events", "message_id")?
    {
        conn.execute_batch("ALTER TABLE otel_events RENAME COLUMN message_uuid TO message_id;")?;
        added_columns.push("otel_events.message_id".to_string());
    }
    if ensure_column(conn, "otel_events", "message_id", "message_id TEXT")? {
        added_columns.push("otel_events.message_id".to_string());
    }
    if ensure_column(conn, "otel_events", "timestamp_nano", "timestamp_nano TEXT")? {
        added_columns.push("otel_events.timestamp_nano".to_string());
    }
    if ensure_column(conn, "otel_events", "model", "model TEXT")? {
        added_columns.push("otel_events.model".to_string());
    }
    if ensure_column(
        conn,
        "otel_events",
        "cost_usd_reported",
        "cost_usd_reported REAL",
    )? {
        added_columns.push("otel_events.cost_usd_reported".to_string());
    }
    if ensure_column(
        conn,
        "otel_events",
        "cost_cents_computed",
        "cost_cents_computed REAL",
    )? {
        added_columns.push("otel_events.cost_cents_computed".to_string());
    }

    let added_indexes = missing_reconcile_indexes(conn)?;

    // Index creation is idempotent and also heals drift where indexes were dropped.
    create_indexes(conn)?;

    if !added_columns.is_empty() || !added_indexes.is_empty() {
        tracing::info!("Schema reconcile completed");
    }
    Ok(SchemaReconcileReport {
        added_columns,
        added_indexes,
    })
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
        conn.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
        // Create v11 schema (no request_id column, no dedup index)
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
            CREATE TABLE otel_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT, event_name TEXT NOT NULL, session_id TEXT,
                timestamp TEXT NOT NULL, raw_json TEXT, processed INTEGER NOT NULL DEFAULT 0
            );
            ",
        ).unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        conn.pragma_update(None, "user_version", 11u32).unwrap();

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
        assert!(
            has_idx,
            "idx_messages_dedup should exist after v11→v12 migration"
        );
        // Verify request_id column was added by v12→v13
        conn.execute("SELECT request_id FROM messages LIMIT 0", [])
            .expect("request_id column should exist after migration");
    }

    #[test]
    fn migrate_v12_to_v13_adds_request_id_and_deduplicates() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
        // Create v12 schema (no request_id)
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
            CREATE TABLE otel_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT, event_name TEXT NOT NULL, session_id TEXT,
                timestamp TEXT NOT NULL, raw_json TEXT, processed INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX idx_messages_dedup ON messages(session_id, model, role, cost_confidence, timestamp);
            ",
        ).unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        conn.pragma_update(None, "user_version", 12u32).unwrap();

        // Insert duplicate rows (simulating the cross-parse bug)
        conn.execute_batch(
            "
            INSERT INTO messages (uuid, session_id, role, timestamp, model, input_tokens, output_tokens,
                cache_creation_tokens, cache_read_tokens, cost_cents, cost_confidence)
            VALUES ('a1', 'sess-1', 'assistant', '2026-03-25T00:00:01.000Z', 'claude-sonnet-4-6', 3, 10, 21559, 50000, 1.5, 'estimated');
            INSERT INTO messages (uuid, session_id, role, timestamp, model, input_tokens, output_tokens,
                cache_creation_tokens, cache_read_tokens, cost_cents, cost_confidence)
            VALUES ('a3', 'sess-1', 'assistant', '2026-03-25T00:00:01.500Z', 'claude-sonnet-4-6', 3, 425, 21559, 50000, 5.0, 'estimated');
            INSERT INTO tags (message_uuid, key, value) VALUES ('a1', 'model', 'claude-sonnet-4-6');
            INSERT INTO tags (message_uuid, key, value) VALUES ('a3', 'model', 'claude-sonnet-4-6');
            ",
        ).unwrap();

        // Verify duplicates exist
        let count: i64 = conn
            .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);

        migrate(&conn).unwrap();
        assert_eq!(current_version(&conn), SCHEMA_VERSION);

        // Verify duplicate was removed (a1 with output_tokens=10 should be gone)
        let count: i64 = conn
            .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "duplicate should have been removed");

        // The remaining row should be the one with higher output_tokens
        let output: i64 = conn
            .query_row("SELECT output_tokens FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(output, 425, "should keep the row with higher output_tokens");

        // Orphaned tags for a1 should be cleaned up
        let tag_count: i64 = conn
            .query_row(
                "SELECT count(*) FROM tags WHERE message_id = 'a1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tag_count, 0, "orphaned tags should be cleaned up");

        // request_id column should exist
        conn.execute("SELECT request_id FROM messages LIMIT 0", [])
            .expect("request_id column should exist");
    }

    #[test]
    fn migrate_reconciles_missing_sessions_title_at_current_version() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        // Simulate schema drift: sessions table without `title`, but user_version still current.
        conn.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
        conn.execute_batch(
            "
            CREATE TABLE sessions_new (
                id                 TEXT PRIMARY KEY,
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
            INSERT INTO sessions_new (
                id, provider, started_at, ended_at, duration_ms,
                composer_mode, permission_mode, user_email, workspace_root, end_reason,
                prompt_category, model, raw_json, repo_id, git_branch
            )
            SELECT
                id, provider, started_at, ended_at, duration_ms,
                composer_mode, permission_mode, user_email, workspace_root, end_reason,
                prompt_category, model, raw_json, repo_id, git_branch
            FROM sessions;
            DROP TABLE sessions;
            ALTER TABLE sessions_new RENAME TO sessions;
            ",
        )
        .unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)
            .unwrap();

        let missing_before = conn.prepare("SELECT title FROM sessions LIMIT 1").is_err();
        assert!(missing_before, "test setup should remove sessions.title");

        migrate(&conn).unwrap();

        conn.prepare("SELECT title FROM sessions LIMIT 1")
            .expect("migrate should repair missing sessions.title");
    }

    #[test]
    fn repair_reports_added_columns_for_drift() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        conn.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
        conn.execute_batch(
            "
            CREATE TABLE sessions_new (
                id                 TEXT PRIMARY KEY,
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
            INSERT INTO sessions_new (
                id, provider, started_at, ended_at, duration_ms,
                composer_mode, permission_mode, user_email, workspace_root, end_reason,
                prompt_category, model, raw_json, repo_id, git_branch
            )
            SELECT
                id, provider, started_at, ended_at, duration_ms,
                composer_mode, permission_mode, user_email, workspace_root, end_reason,
                prompt_category, model, raw_json, repo_id, git_branch
            FROM sessions;
            DROP TABLE sessions;
            ALTER TABLE sessions_new RENAME TO sessions;
            ",
        )
        .unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)
            .unwrap();

        let report = repair(&conn).unwrap();
        assert_eq!(report.to_version, SCHEMA_VERSION);
        assert!(!report.migrated);
        assert!(
            report.added_columns.contains(&"sessions.title".to_string()),
            "repair should report sessions.title addition"
        );
    }

    #[test]
    fn repair_reports_index_only_drift() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        conn.execute_batch("DROP INDEX IF EXISTS idx_messages_session;")
            .unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)
            .unwrap();

        let report = repair(&conn).unwrap();

        assert_eq!(report.to_version, SCHEMA_VERSION);
        assert!(!report.migrated);
        assert!(
            report.added_columns.is_empty(),
            "index-only drift should not report column additions"
        );
        assert!(
            report
                .added_indexes
                .contains(&"idx_messages_session".to_string()),
            "repair should report recreated index"
        );

        let index_exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='index' AND name='idx_messages_session')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(index_exists, "repair should recreate missing index");
    }

    #[test]
    fn migrate_v15_to_v16_normalizes_prefixed_cursor_session_ids() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.pragma_update(None, "user_version", 15u32).unwrap();

        let old_id = "cursor-d99dfe22-d05c-4c78-8698-015d06e5dabb";
        let new_id = "d99dfe22-d05c-4c78-8698-015d06e5dabb";

        conn.execute(
            "INSERT INTO sessions (id, provider, started_at) VALUES (?1, 'cursor', '2026-03-31T16:43:25+00:00')",
            [old_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, session_id, role, timestamp, provider)
             VALUES ('m1', ?1, 'assistant', '2026-03-31T16:43:25+00:00', 'cursor')",
            [old_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO hook_events (provider, event, session_id, timestamp)
             VALUES ('cursor', 'session_start', ?1, '2026-03-31T16:43:25+00:00')",
            [old_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO otel_events (event_name, session_id, timestamp, processed)
             VALUES ('claude_code.api_request', ?1, '2026-03-31T16:43:25+00:00', 1)",
            [old_id],
        )
        .unwrap();

        migrate(&conn).unwrap();
        assert_eq!(current_version(&conn), SCHEMA_VERSION);

        let old_exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)",
                [old_id],
                |r| r.get(0),
            )
            .unwrap();
        let new_exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)",
                [new_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!old_exists, "old prefixed id should be removed");
        assert!(new_exists, "normalized id should exist");

        let msg_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE session_id = ?1",
                [new_id],
                |r| r.get(0),
            )
            .unwrap();
        let hook_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM hook_events WHERE session_id = ?1",
                [new_id],
                |r| r.get(0),
            )
            .unwrap();
        let otel_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM otel_events WHERE session_id = ?1",
                [new_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(msg_count, 1);
        assert_eq!(hook_count, 1);
        assert_eq!(otel_count, 1);
    }

    #[test]
    fn migrate_v15_to_v16_merges_colliding_session_rows() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.pragma_update(None, "user_version", 15u32).unwrap();

        let old_id = "cursor-d99dfe22-d05c-4c78-8698-015d06e5dabb";
        let new_id = "d99dfe22-d05c-4c78-8698-015d06e5dabb";

        conn.execute(
            "INSERT INTO sessions (id, provider, started_at, repo_id, git_branch)
             VALUES (?1, 'cursor', '2026-03-31T16:43:25+00:00', 'github.com/acme/repo', 'main')",
            [old_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, provider, started_at, title)
             VALUES (?1, 'cursor', '2026-03-31T16:43:00+00:00', 'Already normalized row')",
            [new_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, session_id, role, timestamp, provider)
             VALUES ('m1', ?1, 'assistant', '2026-03-31T16:43:25+00:00', 'cursor')",
            [old_id],
        )
        .unwrap();

        migrate(&conn).unwrap();

        let sessions_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE id IN (?1, ?2)",
                rusqlite::params![old_id, new_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sessions_count, 1, "rows should be merged into one session");

        let (repo_id, git_branch, title): (Option<String>, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT repo_id, git_branch, title FROM sessions WHERE id = ?1",
                [new_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(repo_id.as_deref(), Some("github.com/acme/repo"));
        assert_eq!(git_branch.as_deref(), Some("main"));
        assert_eq!(title.as_deref(), Some("Already normalized row"));

        let msg_sid: String = conn
            .query_row("SELECT session_id FROM messages WHERE id = 'm1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(msg_sid, new_id);
    }

    #[test]
    fn migrate_v16_to_v17_purges_legacy_cursor_artifacts() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.pragma_update(None, "user_version", 16u32).unwrap();

        conn.execute(
            "INSERT INTO sessions (id, provider, started_at)
             VALUES ('cursor-synth-1774974046000', 'cursor', '2026-03-31T10:00:00+00:00')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, provider, started_at)
             VALUES ('d99dfe22-d05c-4c78-8698-015d06e5dabb', 'cursor', '2026-03-31T10:00:00+00:00')",
            [],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO messages (id, session_id, role, timestamp, provider)
             VALUES ('cursor-api-legacy', 'cursor-synth-1774974046000', 'assistant', '2026-03-31T10:01:00+00:00', 'cursor')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, session_id, role, timestamp, provider)
             VALUES ('clean-uuid', 'd99dfe22-d05c-4c78-8698-015d06e5dabb', 'assistant', '2026-03-31T10:02:00+00:00', 'cursor')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (message_id, key, value)
             VALUES ('cursor-api-legacy', 'provider', 'cursor')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (message_id, key, value)
             VALUES ('clean-uuid', 'provider', 'cursor')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sync_state (file_path, byte_offset, last_synced)
             VALUES ('cursor-api-usage', 123, '2026-03-31T10:00:00+00:00')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sync_state (file_path, byte_offset, last_synced)
             VALUES ('/Users/me/.cursor/projects/p/agent-transcripts/s.jsonl', 456, '2026-03-31T10:00:00+00:00')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sync_state (file_path, byte_offset, last_synced)
             VALUES ('/Users/me/.claude/projects/p/session.jsonl', 789, '2026-03-31T10:00:00+00:00')",
            [],
        )
        .unwrap();

        migrate(&conn).unwrap();
        assert_eq!(current_version(&conn), SCHEMA_VERSION);

        let legacy_msg_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE id = 'cursor-api-legacy'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            legacy_msg_count, 0,
            "legacy cursor message should be purged"
        );

        let clean_msg_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE id = 'clean-uuid'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            clean_msg_count, 1,
            "canonical cursor message should be kept"
        );

        let synth_session_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE id LIKE 'cursor-synth-%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            synth_session_count, 0,
            "synthetic sessions should be purged"
        );

        let legacy_tag_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tags WHERE message_id = 'cursor-api-legacy'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(legacy_tag_count, 0, "legacy tags should be purged");

        let kept_tag_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tags WHERE message_id = 'clean-uuid'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            kept_tag_count, 1,
            "tags for clean messages should be preserved"
        );

        let cursor_watermark_exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sync_state WHERE file_path = 'cursor-api-usage')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!cursor_watermark_exists, "cursor watermark should be reset");

        let cursor_transcript_offset_exists: bool = conn
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM sync_state
                    WHERE file_path LIKE '%/agent-transcripts/%'
                )",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            !cursor_transcript_offset_exists,
            "cursor transcript offsets should be reset"
        );

        let claude_offset_exists: bool = conn
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM sync_state
                    WHERE file_path = '/Users/me/.claude/projects/p/session.jsonl'
                )",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(claude_offset_exists, "non-cursor offsets must be preserved");
    }

    #[test]
    fn migrate_v17_to_v18_adds_linkage_columns_and_indexes() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        conn.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
        conn.execute_batch(
            "
            CREATE TABLE hook_events_new (
                id                  INTEGER PRIMARY KEY AUTOINCREMENT,
                provider            TEXT NOT NULL,
                event               TEXT NOT NULL,
                session_id          TEXT,
                timestamp           TEXT NOT NULL,
                model               TEXT,
                tool_name           TEXT,
                tool_duration_ms    INTEGER,
                tool_call_count     INTEGER,
                raw_json            TEXT,
                mcp_server          TEXT
            );
            INSERT INTO hook_events_new (id, provider, event, session_id, timestamp, model, tool_name, tool_duration_ms, tool_call_count, raw_json, mcp_server)
            SELECT id, provider, event, session_id, timestamp, model, tool_name, tool_duration_ms, tool_call_count, raw_json, mcp_server
            FROM hook_events;
            DROP TABLE hook_events;
            ALTER TABLE hook_events_new RENAME TO hook_events;

            CREATE TABLE otel_events_new (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                event_name  TEXT NOT NULL,
                session_id  TEXT,
                timestamp   TEXT NOT NULL,
                raw_json    TEXT,
                processed   INTEGER NOT NULL DEFAULT 0
            );
            INSERT INTO otel_events_new (id, event_name, session_id, timestamp, raw_json, processed)
            SELECT id, event_name, session_id, timestamp, raw_json, processed
            FROM otel_events;
            DROP TABLE otel_events;
            ALTER TABLE otel_events_new RENAME TO otel_events;
            ",
        )
        .unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        conn.pragma_update(None, "user_version", 17u32).unwrap();

        migrate(&conn).unwrap();
        assert_eq!(current_version(&conn), SCHEMA_VERSION);

        conn.execute("SELECT message_id FROM hook_events LIMIT 0", [])
            .expect("hook_events.message_id should exist");
        conn.execute("SELECT message_request_id FROM hook_events LIMIT 0", [])
            .expect("hook_events.message_request_id should exist");
        conn.execute("SELECT tool_use_id FROM hook_events LIMIT 0", [])
            .expect("hook_events.tool_use_id should exist");
        conn.execute("SELECT link_confidence FROM hook_events LIMIT 0", [])
            .expect("hook_events.link_confidence should exist");

        conn.execute("SELECT message_id FROM otel_events LIMIT 0", [])
            .expect("otel_events.message_id should exist");
        conn.execute("SELECT timestamp_nano FROM otel_events LIMIT 0", [])
            .expect("otel_events.timestamp_nano should exist");
        conn.execute("SELECT model FROM otel_events LIMIT 0", [])
            .expect("otel_events.model should exist");
        conn.execute("SELECT cost_usd_reported FROM otel_events LIMIT 0", [])
            .expect("otel_events.cost_usd_reported should exist");
        conn.execute("SELECT cost_cents_computed FROM otel_events LIMIT 0", [])
            .expect("otel_events.cost_cents_computed should exist");

        let has_hook_message_idx: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='index' AND name='idx_hook_events_message_id_ts')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let has_otel_message_idx: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='index' AND name='idx_otel_events_message_id_ts')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(has_hook_message_idx);
        assert!(has_otel_message_idx);
    }

    #[test]
    fn migrate_v17_to_v18_backfills_hook_and_otel_links() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        conn.execute(
            "INSERT INTO messages (id, session_id, role, timestamp, model, request_id, cost_confidence, cost_cents, provider)
             VALUES ('m-link', 'sess-link', 'assistant', '2026-03-25T00:00:01Z', 'claude-opus-4-6', 'msg_123', 'otel_exact', 7.5, 'claude_code')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (message_id, key, value)
             VALUES ('m-link', 'tool_use_id', 'toolu_456')",
            [],
        )
        .unwrap();

        conn.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
        conn.execute_batch(
            "
            CREATE TABLE hook_events_new (
                id                  INTEGER PRIMARY KEY AUTOINCREMENT,
                provider            TEXT NOT NULL,
                event               TEXT NOT NULL,
                session_id          TEXT,
                timestamp           TEXT NOT NULL,
                model               TEXT,
                tool_name           TEXT,
                tool_duration_ms    INTEGER,
                tool_call_count     INTEGER,
                raw_json            TEXT,
                mcp_server          TEXT
            );
            INSERT INTO hook_events_new (provider, event, session_id, timestamp, raw_json)
            VALUES (
              'claude_code',
              'post_tool_use',
              'sess-link',
              '2026-03-25T00:00:01Z',
              '{\"message_id\":\"msg_123\",\"tool_use_id\":\"toolu_456\"}'
            );
            DROP TABLE hook_events;
            ALTER TABLE hook_events_new RENAME TO hook_events;

            CREATE TABLE otel_events_new (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                event_name  TEXT NOT NULL,
                session_id  TEXT,
                timestamp   TEXT NOT NULL,
                raw_json    TEXT,
                processed   INTEGER NOT NULL DEFAULT 0
            );
            INSERT INTO otel_events_new (event_name, session_id, timestamp, raw_json, processed)
            VALUES (
              'claude_code.api_request',
              'sess-link',
              '2026-03-25T00:00:01.100Z',
              '{\"timestamp_nano\":\"1711324801100000000\",\"model\":\"claude-opus-4-6\",\"cost_usd_reported\":0.075}',
              1
            );
            DROP TABLE otel_events;
            ALTER TABLE otel_events_new RENAME TO otel_events;
            ",
        )
        .unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        conn.pragma_update(None, "user_version", 17u32).unwrap();

        migrate(&conn).unwrap();

        let (message_request_id, tool_use_id, message_id, confidence): (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT message_request_id, tool_use_id, message_id, link_confidence
                 FROM hook_events
                 LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(message_request_id.as_deref(), Some("msg_123"));
        assert_eq!(tool_use_id.as_deref(), Some("toolu_456"));
        assert_eq!(message_id.as_deref(), Some("m-link"));
        assert_eq!(confidence.as_deref(), Some("exact_request_id"));

        let (otel_message_id, model, cost_cents_computed): (
            Option<String>,
            Option<String>,
            Option<f64>,
        ) = conn
            .query_row(
                "SELECT message_id, model, cost_cents_computed
                 FROM otel_events
                 LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(otel_message_id.as_deref(), Some("m-link"));
        assert_eq!(model.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(cost_cents_computed, Some(7.5));
    }

    #[test]
    fn migrate_v19_to_v20_renames_identifier_columns_and_preserves_data() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        conn.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
        conn.execute_batch(
            "
            ALTER TABLE messages RENAME COLUMN id TO uuid;
            ALTER TABLE sessions RENAME COLUMN id TO session_id;
            ALTER TABLE tags RENAME COLUMN message_id TO message_uuid;
            ",
        )
        .unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        conn.pragma_update(None, "user_version", 19u32).unwrap();

        conn.execute(
            "INSERT INTO sessions (session_id, provider, started_at)
             VALUES ('sess-v19', 'claude_code', '2026-04-01T10:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (uuid, session_id, role, timestamp, provider, cost_confidence, cost_cents)
             VALUES ('msg-v19', 'sess-v19', 'assistant', '2026-04-01T10:00:01Z', 'claude_code', 'estimated', 1.25)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (message_uuid, key, value)
             VALUES ('msg-v19', 'ticket_id', 'ABC-123')",
            [],
        )
        .unwrap();

        migrate(&conn).unwrap();
        assert_eq!(current_version(&conn), SCHEMA_VERSION);

        conn.execute("SELECT id FROM messages LIMIT 0", [])
            .expect("messages.id should exist");
        conn.execute("SELECT id FROM sessions LIMIT 0", [])
            .expect("sessions.id should exist");
        conn.execute("SELECT message_id FROM tags LIMIT 0", [])
            .expect("tags.message_id should exist");

        let has_legacy_messages_uuid = conn.prepare("SELECT uuid FROM messages LIMIT 0").is_ok();
        let has_legacy_sessions_session_id = conn
            .prepare("SELECT session_id FROM sessions LIMIT 0")
            .is_ok();
        let has_legacy_tags_message_uuid = conn
            .prepare("SELECT message_uuid FROM tags LIMIT 0")
            .is_ok();
        assert!(!has_legacy_messages_uuid);
        assert!(!has_legacy_sessions_session_id);
        assert!(!has_legacy_tags_message_uuid);

        let kept_message_id: String = conn
            .query_row("SELECT id FROM messages WHERE id = 'msg-v19'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let kept_session_id: String = conn
            .query_row("SELECT id FROM sessions WHERE id = 'sess-v19'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let kept_tag_message_id: String = conn
            .query_row(
                "SELECT message_id FROM tags WHERE key = 'ticket_id' LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept_message_id, "msg-v19");
        assert_eq!(kept_session_id, "sess-v19");
        assert_eq!(kept_tag_message_id, "msg-v19");
    }
}
