//! Database schema migration for the analytics SQLite database.
//!
//! Budi 8.0.0 starts the schema at version 1. There are no incremental upgrades from
//! pre-release betas: if `user_version` is not 0 and not [`SCHEMA_VERSION`], all user
//! tables are dropped and the schema is recreated (JSONL remains the source of truth).

use anyhow::Result;
use rusqlite::Connection;

/// Expected schema version for the current binary.
pub const SCHEMA_VERSION: u32 = 1;

/// Result of running schema repair.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RepairReport {
    pub from_version: u32,
    pub to_version: u32,
    pub migrated: bool,
    pub added_columns: Vec<String>,
    pub added_indexes: Vec<String>,
}

/// Report from [`reconcile_schema`] (additive repairs and rollup healing).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SchemaReconcileReport {
    pub added_columns: Vec<String>,
    pub added_indexes: Vec<String>,
}

/// Check the current schema version without migrating.
pub fn current_version(conn: &Connection) -> u32 {
    conn.pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap_or(0)
}

/// Returns true if the database needs migration to match this binary.
pub fn needs_migration(conn: &Connection) -> bool {
    current_version(conn) != SCHEMA_VERSION
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

/// Run all pending migrations up to [`SCHEMA_VERSION`].
pub fn migrate(conn: &Connection) -> Result<()> {
    run_version_migrations(conn)?;
    let _ = reconcile_schema(conn)?;
    Ok(())
}

/// Run migrations and reconcile additive schema drift.
///
/// This is safe to run repeatedly. It rebuilds legacy beta databases from scratch
/// and repairs missing rollup tables, triggers, and indexes on current schemas.
pub fn repair(conn: &Connection) -> Result<RepairReport> {
    let from_version = current_version(conn);
    run_version_migrations(conn)?;
    let reconcile = reconcile_schema(conn)?;
    let to_version = current_version(conn);
    Ok(RepairReport {
        from_version,
        to_version,
        migrated: from_version != to_version,
        added_columns: reconcile.added_columns,
        added_indexes: reconcile.added_indexes,
    })
}

fn run_version_migrations(conn: &Connection) -> Result<()> {
    let version = current_version(conn);

    if version == SCHEMA_VERSION {
        return Ok(());
    }

    conn.execute_batch("PRAGMA foreign_keys=OFF;")?;

    if version == 0 {
        create_current_schema(conn)?;
    } else {
        tracing::info!(
            from_version = version,
            to_version = SCHEMA_VERSION,
            "Destructive migration: dropping all tables and recreating schema (beta or mismatched version)"
        );
        drop_all_tables(conn)?;
        create_current_schema(conn)?;
    }

    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    conn.execute_batch("PRAGMA foreign_keys=ON;")?;
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
    create_sessions(conn)?;
    ensure_rollup_schema(conn, false)?;
    create_indexes(conn)?;
    Ok(())
}

/// Create sessions table.
fn create_sessions(conn: &Connection) -> Result<()> {
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
        ",
    )?;
    Ok(())
}

fn ensure_rollup_schema(conn: &Connection, backfill: bool) -> Result<()> {
    create_rollup_tables(conn)?;
    create_rollup_triggers(conn)?;
    if backfill {
        backfill_rollup_tables(conn)?;
    }
    Ok(())
}

fn create_rollup_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS message_rollups_hourly (
            bucket_start           TEXT NOT NULL,
            role                   TEXT NOT NULL,
            provider               TEXT NOT NULL,
            model                  TEXT NOT NULL,
            repo_id                TEXT NOT NULL,
            git_branch             TEXT NOT NULL,
            message_count          INTEGER NOT NULL DEFAULT 0,
            input_tokens           INTEGER NOT NULL DEFAULT 0,
            output_tokens          INTEGER NOT NULL DEFAULT 0,
            cache_creation_tokens  INTEGER NOT NULL DEFAULT 0,
            cache_read_tokens      INTEGER NOT NULL DEFAULT 0,
            cost_cents             REAL NOT NULL DEFAULT 0,
            PRIMARY KEY(bucket_start, role, provider, model, repo_id, git_branch)
        );

        CREATE TABLE IF NOT EXISTS message_rollups_daily (
            bucket_day             TEXT NOT NULL,
            role                   TEXT NOT NULL,
            provider               TEXT NOT NULL,
            model                  TEXT NOT NULL,
            repo_id                TEXT NOT NULL,
            git_branch             TEXT NOT NULL,
            message_count          INTEGER NOT NULL DEFAULT 0,
            input_tokens           INTEGER NOT NULL DEFAULT 0,
            output_tokens          INTEGER NOT NULL DEFAULT 0,
            cache_creation_tokens  INTEGER NOT NULL DEFAULT 0,
            cache_read_tokens      INTEGER NOT NULL DEFAULT 0,
            cost_cents             REAL NOT NULL DEFAULT 0,
            PRIMARY KEY(bucket_day, role, provider, model, repo_id, git_branch)
        );
        ",
    )?;
    Ok(())
}

fn create_rollup_triggers(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        DROP TRIGGER IF EXISTS trg_messages_rollup_insert;
        DROP TRIGGER IF EXISTS trg_messages_rollup_delete;
        DROP TRIGGER IF EXISTS trg_messages_rollup_update;

        CREATE TRIGGER IF NOT EXISTS trg_messages_rollup_insert
        AFTER INSERT ON messages
        BEGIN
            INSERT INTO message_rollups_hourly (
                bucket_start, role, provider, model, repo_id, git_branch,
                message_count, input_tokens, output_tokens,
                cache_creation_tokens, cache_read_tokens, cost_cents
            )
            VALUES (
                strftime('%Y-%m-%dT%H:00:00Z', NEW.timestamp),
                COALESCE(NULLIF(NEW.role, ''), 'assistant'),
                COALESCE(NULLIF(NEW.provider, ''), 'claude_code'),
                CASE
                    WHEN NEW.model IS NULL OR NEW.model = '' OR SUBSTR(NEW.model, 1, 1) = '<'
                    THEN '(untagged)'
                    ELSE NEW.model
                END,
                COALESCE(NULLIF(NULLIF(NEW.repo_id, ''), 'unknown'), '(untagged)'),
                COALESCE(
                    NULLIF(
                        CASE
                            WHEN COALESCE(NEW.git_branch, '') LIKE 'refs/heads/%'
                            THEN SUBSTR(COALESCE(NEW.git_branch, ''), 12)
                            ELSE COALESCE(NEW.git_branch, '')
                        END,
                        ''
                    ),
                    '(untagged)'
                ),
                1,
                COALESCE(NEW.input_tokens, 0),
                COALESCE(NEW.output_tokens, 0),
                COALESCE(NEW.cache_creation_tokens, 0),
                COALESCE(NEW.cache_read_tokens, 0),
                COALESCE(NEW.cost_cents, 0.0)
            )
            ON CONFLICT(bucket_start, role, provider, model, repo_id, git_branch) DO UPDATE SET
                message_count = message_count + excluded.message_count,
                input_tokens = input_tokens + excluded.input_tokens,
                output_tokens = output_tokens + excluded.output_tokens,
                cache_creation_tokens = cache_creation_tokens + excluded.cache_creation_tokens,
                cache_read_tokens = cache_read_tokens + excluded.cache_read_tokens,
                cost_cents = cost_cents + excluded.cost_cents;

            INSERT INTO message_rollups_daily (
                bucket_day, role, provider, model, repo_id, git_branch,
                message_count, input_tokens, output_tokens,
                cache_creation_tokens, cache_read_tokens, cost_cents
            )
            VALUES (
                strftime('%Y-%m-%d', NEW.timestamp),
                COALESCE(NULLIF(NEW.role, ''), 'assistant'),
                COALESCE(NULLIF(NEW.provider, ''), 'claude_code'),
                CASE
                    WHEN NEW.model IS NULL OR NEW.model = '' OR SUBSTR(NEW.model, 1, 1) = '<'
                    THEN '(untagged)'
                    ELSE NEW.model
                END,
                COALESCE(NULLIF(NULLIF(NEW.repo_id, ''), 'unknown'), '(untagged)'),
                COALESCE(
                    NULLIF(
                        CASE
                            WHEN COALESCE(NEW.git_branch, '') LIKE 'refs/heads/%'
                            THEN SUBSTR(COALESCE(NEW.git_branch, ''), 12)
                            ELSE COALESCE(NEW.git_branch, '')
                        END,
                        ''
                    ),
                    '(untagged)'
                ),
                1,
                COALESCE(NEW.input_tokens, 0),
                COALESCE(NEW.output_tokens, 0),
                COALESCE(NEW.cache_creation_tokens, 0),
                COALESCE(NEW.cache_read_tokens, 0),
                COALESCE(NEW.cost_cents, 0.0)
            )
            ON CONFLICT(bucket_day, role, provider, model, repo_id, git_branch) DO UPDATE SET
                message_count = message_count + excluded.message_count,
                input_tokens = input_tokens + excluded.input_tokens,
                output_tokens = output_tokens + excluded.output_tokens,
                cache_creation_tokens = cache_creation_tokens + excluded.cache_creation_tokens,
                cache_read_tokens = cache_read_tokens + excluded.cache_read_tokens,
                cost_cents = cost_cents + excluded.cost_cents;
        END;

        CREATE TRIGGER IF NOT EXISTS trg_messages_rollup_delete
        AFTER DELETE ON messages
        BEGIN
            INSERT INTO message_rollups_hourly (
                bucket_start, role, provider, model, repo_id, git_branch,
                message_count, input_tokens, output_tokens,
                cache_creation_tokens, cache_read_tokens, cost_cents
            )
            VALUES (
                strftime('%Y-%m-%dT%H:00:00Z', OLD.timestamp),
                COALESCE(NULLIF(OLD.role, ''), 'assistant'),
                COALESCE(NULLIF(OLD.provider, ''), 'claude_code'),
                CASE
                    WHEN OLD.model IS NULL OR OLD.model = '' OR SUBSTR(OLD.model, 1, 1) = '<'
                    THEN '(untagged)'
                    ELSE OLD.model
                END,
                COALESCE(NULLIF(NULLIF(OLD.repo_id, ''), 'unknown'), '(untagged)'),
                COALESCE(
                    NULLIF(
                        CASE
                            WHEN COALESCE(OLD.git_branch, '') LIKE 'refs/heads/%'
                            THEN SUBSTR(COALESCE(OLD.git_branch, ''), 12)
                            ELSE COALESCE(OLD.git_branch, '')
                        END,
                        ''
                    ),
                    '(untagged)'
                ),
                -1,
                -COALESCE(OLD.input_tokens, 0),
                -COALESCE(OLD.output_tokens, 0),
                -COALESCE(OLD.cache_creation_tokens, 0),
                -COALESCE(OLD.cache_read_tokens, 0),
                -COALESCE(OLD.cost_cents, 0.0)
            )
            ON CONFLICT(bucket_start, role, provider, model, repo_id, git_branch) DO UPDATE SET
                message_count = message_count + excluded.message_count,
                input_tokens = input_tokens + excluded.input_tokens,
                output_tokens = output_tokens + excluded.output_tokens,
                cache_creation_tokens = cache_creation_tokens + excluded.cache_creation_tokens,
                cache_read_tokens = cache_read_tokens + excluded.cache_read_tokens,
                cost_cents = cost_cents + excluded.cost_cents;

            DELETE FROM message_rollups_hourly
             WHERE bucket_start = strftime('%Y-%m-%dT%H:00:00Z', OLD.timestamp)
               AND role = COALESCE(NULLIF(OLD.role, ''), 'assistant')
               AND provider = COALESCE(NULLIF(OLD.provider, ''), 'claude_code')
               AND model = CASE
                    WHEN OLD.model IS NULL OR OLD.model = '' OR SUBSTR(OLD.model, 1, 1) = '<'
                    THEN '(untagged)'
                    ELSE OLD.model
               END
               AND repo_id = COALESCE(NULLIF(NULLIF(OLD.repo_id, ''), 'unknown'), '(untagged)')
               AND git_branch = COALESCE(
                    NULLIF(
                        CASE
                            WHEN COALESCE(OLD.git_branch, '') LIKE 'refs/heads/%'
                            THEN SUBSTR(COALESCE(OLD.git_branch, ''), 12)
                            ELSE COALESCE(OLD.git_branch, '')
                        END,
                        ''
                    ),
                    '(untagged)'
               )
               AND message_count <= 0;

            INSERT INTO message_rollups_daily (
                bucket_day, role, provider, model, repo_id, git_branch,
                message_count, input_tokens, output_tokens,
                cache_creation_tokens, cache_read_tokens, cost_cents
            )
            VALUES (
                strftime('%Y-%m-%d', OLD.timestamp),
                COALESCE(NULLIF(OLD.role, ''), 'assistant'),
                COALESCE(NULLIF(OLD.provider, ''), 'claude_code'),
                CASE
                    WHEN OLD.model IS NULL OR OLD.model = '' OR SUBSTR(OLD.model, 1, 1) = '<'
                    THEN '(untagged)'
                    ELSE OLD.model
                END,
                COALESCE(NULLIF(NULLIF(OLD.repo_id, ''), 'unknown'), '(untagged)'),
                COALESCE(
                    NULLIF(
                        CASE
                            WHEN COALESCE(OLD.git_branch, '') LIKE 'refs/heads/%'
                            THEN SUBSTR(COALESCE(OLD.git_branch, ''), 12)
                            ELSE COALESCE(OLD.git_branch, '')
                        END,
                        ''
                    ),
                    '(untagged)'
                ),
                -1,
                -COALESCE(OLD.input_tokens, 0),
                -COALESCE(OLD.output_tokens, 0),
                -COALESCE(OLD.cache_creation_tokens, 0),
                -COALESCE(OLD.cache_read_tokens, 0),
                -COALESCE(OLD.cost_cents, 0.0)
            )
            ON CONFLICT(bucket_day, role, provider, model, repo_id, git_branch) DO UPDATE SET
                message_count = message_count + excluded.message_count,
                input_tokens = input_tokens + excluded.input_tokens,
                output_tokens = output_tokens + excluded.output_tokens,
                cache_creation_tokens = cache_creation_tokens + excluded.cache_creation_tokens,
                cache_read_tokens = cache_read_tokens + excluded.cache_read_tokens,
                cost_cents = cost_cents + excluded.cost_cents;

            DELETE FROM message_rollups_daily
             WHERE bucket_day = strftime('%Y-%m-%d', OLD.timestamp)
               AND role = COALESCE(NULLIF(OLD.role, ''), 'assistant')
               AND provider = COALESCE(NULLIF(OLD.provider, ''), 'claude_code')
               AND model = CASE
                    WHEN OLD.model IS NULL OR OLD.model = '' OR SUBSTR(OLD.model, 1, 1) = '<'
                    THEN '(untagged)'
                    ELSE OLD.model
               END
               AND repo_id = COALESCE(NULLIF(NULLIF(OLD.repo_id, ''), 'unknown'), '(untagged)')
               AND git_branch = COALESCE(
                    NULLIF(
                        CASE
                            WHEN COALESCE(OLD.git_branch, '') LIKE 'refs/heads/%'
                            THEN SUBSTR(COALESCE(OLD.git_branch, ''), 12)
                            ELSE COALESCE(OLD.git_branch, '')
                        END,
                        ''
                    ),
                    '(untagged)'
               )
               AND message_count <= 0;
        END;

        CREATE TRIGGER IF NOT EXISTS trg_messages_rollup_update
        AFTER UPDATE ON messages
        BEGIN
            INSERT INTO message_rollups_hourly (
                bucket_start, role, provider, model, repo_id, git_branch,
                message_count, input_tokens, output_tokens,
                cache_creation_tokens, cache_read_tokens, cost_cents
            )
            VALUES (
                strftime('%Y-%m-%dT%H:00:00Z', OLD.timestamp),
                COALESCE(NULLIF(OLD.role, ''), 'assistant'),
                COALESCE(NULLIF(OLD.provider, ''), 'claude_code'),
                CASE
                    WHEN OLD.model IS NULL OR OLD.model = '' OR SUBSTR(OLD.model, 1, 1) = '<'
                    THEN '(untagged)'
                    ELSE OLD.model
                END,
                COALESCE(NULLIF(NULLIF(OLD.repo_id, ''), 'unknown'), '(untagged)'),
                COALESCE(
                    NULLIF(
                        CASE
                            WHEN COALESCE(OLD.git_branch, '') LIKE 'refs/heads/%'
                            THEN SUBSTR(COALESCE(OLD.git_branch, ''), 12)
                            ELSE COALESCE(OLD.git_branch, '')
                        END,
                        ''
                    ),
                    '(untagged)'
                ),
                -1,
                -COALESCE(OLD.input_tokens, 0),
                -COALESCE(OLD.output_tokens, 0),
                -COALESCE(OLD.cache_creation_tokens, 0),
                -COALESCE(OLD.cache_read_tokens, 0),
                -COALESCE(OLD.cost_cents, 0.0)
            )
            ON CONFLICT(bucket_start, role, provider, model, repo_id, git_branch) DO UPDATE SET
                message_count = message_count + excluded.message_count,
                input_tokens = input_tokens + excluded.input_tokens,
                output_tokens = output_tokens + excluded.output_tokens,
                cache_creation_tokens = cache_creation_tokens + excluded.cache_creation_tokens,
                cache_read_tokens = cache_read_tokens + excluded.cache_read_tokens,
                cost_cents = cost_cents + excluded.cost_cents;

            DELETE FROM message_rollups_hourly
             WHERE bucket_start = strftime('%Y-%m-%dT%H:00:00Z', OLD.timestamp)
               AND role = COALESCE(NULLIF(OLD.role, ''), 'assistant')
               AND provider = COALESCE(NULLIF(OLD.provider, ''), 'claude_code')
               AND model = CASE
                    WHEN OLD.model IS NULL OR OLD.model = '' OR SUBSTR(OLD.model, 1, 1) = '<'
                    THEN '(untagged)'
                    ELSE OLD.model
               END
               AND repo_id = COALESCE(NULLIF(NULLIF(OLD.repo_id, ''), 'unknown'), '(untagged)')
               AND git_branch = COALESCE(
                    NULLIF(
                        CASE
                            WHEN COALESCE(OLD.git_branch, '') LIKE 'refs/heads/%'
                            THEN SUBSTR(COALESCE(OLD.git_branch, ''), 12)
                            ELSE COALESCE(OLD.git_branch, '')
                        END,
                        ''
                    ),
                    '(untagged)'
               )
               AND message_count <= 0;

            INSERT INTO message_rollups_daily (
                bucket_day, role, provider, model, repo_id, git_branch,
                message_count, input_tokens, output_tokens,
                cache_creation_tokens, cache_read_tokens, cost_cents
            )
            VALUES (
                strftime('%Y-%m-%d', OLD.timestamp),
                COALESCE(NULLIF(OLD.role, ''), 'assistant'),
                COALESCE(NULLIF(OLD.provider, ''), 'claude_code'),
                CASE
                    WHEN OLD.model IS NULL OR OLD.model = '' OR SUBSTR(OLD.model, 1, 1) = '<'
                    THEN '(untagged)'
                    ELSE OLD.model
                END,
                COALESCE(NULLIF(NULLIF(OLD.repo_id, ''), 'unknown'), '(untagged)'),
                COALESCE(
                    NULLIF(
                        CASE
                            WHEN COALESCE(OLD.git_branch, '') LIKE 'refs/heads/%'
                            THEN SUBSTR(COALESCE(OLD.git_branch, ''), 12)
                            ELSE COALESCE(OLD.git_branch, '')
                        END,
                        ''
                    ),
                    '(untagged)'
                ),
                -1,
                -COALESCE(OLD.input_tokens, 0),
                -COALESCE(OLD.output_tokens, 0),
                -COALESCE(OLD.cache_creation_tokens, 0),
                -COALESCE(OLD.cache_read_tokens, 0),
                -COALESCE(OLD.cost_cents, 0.0)
            )
            ON CONFLICT(bucket_day, role, provider, model, repo_id, git_branch) DO UPDATE SET
                message_count = message_count + excluded.message_count,
                input_tokens = input_tokens + excluded.input_tokens,
                output_tokens = output_tokens + excluded.output_tokens,
                cache_creation_tokens = cache_creation_tokens + excluded.cache_creation_tokens,
                cache_read_tokens = cache_read_tokens + excluded.cache_read_tokens,
                cost_cents = cost_cents + excluded.cost_cents;

            DELETE FROM message_rollups_daily
             WHERE bucket_day = strftime('%Y-%m-%d', OLD.timestamp)
               AND role = COALESCE(NULLIF(OLD.role, ''), 'assistant')
               AND provider = COALESCE(NULLIF(OLD.provider, ''), 'claude_code')
               AND model = CASE
                    WHEN OLD.model IS NULL OR OLD.model = '' OR SUBSTR(OLD.model, 1, 1) = '<'
                    THEN '(untagged)'
                    ELSE OLD.model
               END
               AND repo_id = COALESCE(NULLIF(NULLIF(OLD.repo_id, ''), 'unknown'), '(untagged)')
               AND git_branch = COALESCE(
                    NULLIF(
                        CASE
                            WHEN COALESCE(OLD.git_branch, '') LIKE 'refs/heads/%'
                            THEN SUBSTR(COALESCE(OLD.git_branch, ''), 12)
                            ELSE COALESCE(OLD.git_branch, '')
                        END,
                        ''
                    ),
                    '(untagged)'
               )
               AND message_count <= 0;

            INSERT INTO message_rollups_hourly (
                bucket_start, role, provider, model, repo_id, git_branch,
                message_count, input_tokens, output_tokens,
                cache_creation_tokens, cache_read_tokens, cost_cents
            )
            VALUES (
                strftime('%Y-%m-%dT%H:00:00Z', NEW.timestamp),
                COALESCE(NULLIF(NEW.role, ''), 'assistant'),
                COALESCE(NULLIF(NEW.provider, ''), 'claude_code'),
                CASE
                    WHEN NEW.model IS NULL OR NEW.model = '' OR SUBSTR(NEW.model, 1, 1) = '<'
                    THEN '(untagged)'
                    ELSE NEW.model
                END,
                COALESCE(NULLIF(NULLIF(NEW.repo_id, ''), 'unknown'), '(untagged)'),
                COALESCE(
                    NULLIF(
                        CASE
                            WHEN COALESCE(NEW.git_branch, '') LIKE 'refs/heads/%'
                            THEN SUBSTR(COALESCE(NEW.git_branch, ''), 12)
                            ELSE COALESCE(NEW.git_branch, '')
                        END,
                        ''
                    ),
                    '(untagged)'
                ),
                1,
                COALESCE(NEW.input_tokens, 0),
                COALESCE(NEW.output_tokens, 0),
                COALESCE(NEW.cache_creation_tokens, 0),
                COALESCE(NEW.cache_read_tokens, 0),
                COALESCE(NEW.cost_cents, 0.0)
            )
            ON CONFLICT(bucket_start, role, provider, model, repo_id, git_branch) DO UPDATE SET
                message_count = message_count + excluded.message_count,
                input_tokens = input_tokens + excluded.input_tokens,
                output_tokens = output_tokens + excluded.output_tokens,
                cache_creation_tokens = cache_creation_tokens + excluded.cache_creation_tokens,
                cache_read_tokens = cache_read_tokens + excluded.cache_read_tokens,
                cost_cents = cost_cents + excluded.cost_cents;

            INSERT INTO message_rollups_daily (
                bucket_day, role, provider, model, repo_id, git_branch,
                message_count, input_tokens, output_tokens,
                cache_creation_tokens, cache_read_tokens, cost_cents
            )
            VALUES (
                strftime('%Y-%m-%d', NEW.timestamp),
                COALESCE(NULLIF(NEW.role, ''), 'assistant'),
                COALESCE(NULLIF(NEW.provider, ''), 'claude_code'),
                CASE
                    WHEN NEW.model IS NULL OR NEW.model = '' OR SUBSTR(NEW.model, 1, 1) = '<'
                    THEN '(untagged)'
                    ELSE NEW.model
                END,
                COALESCE(NULLIF(NULLIF(NEW.repo_id, ''), 'unknown'), '(untagged)'),
                COALESCE(
                    NULLIF(
                        CASE
                            WHEN COALESCE(NEW.git_branch, '') LIKE 'refs/heads/%'
                            THEN SUBSTR(COALESCE(NEW.git_branch, ''), 12)
                            ELSE COALESCE(NEW.git_branch, '')
                        END,
                        ''
                    ),
                    '(untagged)'
                ),
                1,
                COALESCE(NEW.input_tokens, 0),
                COALESCE(NEW.output_tokens, 0),
                COALESCE(NEW.cache_creation_tokens, 0),
                COALESCE(NEW.cache_read_tokens, 0),
                COALESCE(NEW.cost_cents, 0.0)
            )
            ON CONFLICT(bucket_day, role, provider, model, repo_id, git_branch) DO UPDATE SET
                message_count = message_count + excluded.message_count,
                input_tokens = input_tokens + excluded.input_tokens,
                output_tokens = output_tokens + excluded.output_tokens,
                cache_creation_tokens = cache_creation_tokens + excluded.cache_creation_tokens,
                cache_read_tokens = cache_read_tokens + excluded.cache_read_tokens,
                cost_cents = cost_cents + excluded.cost_cents;
        END;
        ",
    )?;
    Ok(())
}

fn backfill_rollup_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        DELETE FROM message_rollups_hourly;
        DELETE FROM message_rollups_daily;

        WITH normalized AS (
            SELECT
                strftime('%Y-%m-%dT%H:00:00Z', timestamp) AS bucket_hour,
                strftime('%Y-%m-%d', timestamp) AS bucket_day,
                COALESCE(NULLIF(role, ''), 'assistant') AS role,
                COALESCE(NULLIF(provider, ''), 'claude_code') AS provider,
                CASE
                    WHEN model IS NULL OR model = '' OR SUBSTR(model, 1, 1) = '<' THEN '(untagged)'
                    ELSE model
                END AS model,
                COALESCE(NULLIF(NULLIF(repo_id, ''), 'unknown'), '(untagged)') AS repo_id,
                COALESCE(
                    NULLIF(
                        CASE
                            WHEN COALESCE(git_branch, '') LIKE 'refs/heads/%'
                            THEN SUBSTR(COALESCE(git_branch, ''), 12)
                            ELSE COALESCE(git_branch, '')
                        END,
                        ''
                    ),
                    '(untagged)'
                ) AS git_branch,
                COALESCE(input_tokens, 0) AS input_tokens,
                COALESCE(output_tokens, 0) AS output_tokens,
                COALESCE(cache_creation_tokens, 0) AS cache_creation_tokens,
                COALESCE(cache_read_tokens, 0) AS cache_read_tokens,
                COALESCE(cost_cents, 0.0) AS cost_cents
            FROM messages
        )
        INSERT INTO message_rollups_hourly (
            bucket_start, role, provider, model, repo_id, git_branch,
            message_count, input_tokens, output_tokens,
            cache_creation_tokens, cache_read_tokens, cost_cents
        )
        SELECT
            bucket_hour, role, provider, model, repo_id, git_branch,
            COUNT(*) AS message_count,
            COALESCE(SUM(input_tokens), 0),
            COALESCE(SUM(output_tokens), 0),
            COALESCE(SUM(cache_creation_tokens), 0),
            COALESCE(SUM(cache_read_tokens), 0),
            COALESCE(SUM(cost_cents), 0.0)
        FROM normalized
        GROUP BY bucket_hour, role, provider, model, repo_id, git_branch;

        WITH normalized AS (
            SELECT
                strftime('%Y-%m-%d', timestamp) AS bucket_day,
                COALESCE(NULLIF(role, ''), 'assistant') AS role,
                COALESCE(NULLIF(provider, ''), 'claude_code') AS provider,
                CASE
                    WHEN model IS NULL OR model = '' OR SUBSTR(model, 1, 1) = '<' THEN '(untagged)'
                    ELSE model
                END AS model,
                COALESCE(NULLIF(NULLIF(repo_id, ''), 'unknown'), '(untagged)') AS repo_id,
                COALESCE(
                    NULLIF(
                        CASE
                            WHEN COALESCE(git_branch, '') LIKE 'refs/heads/%'
                            THEN SUBSTR(COALESCE(git_branch, ''), 12)
                            ELSE COALESCE(git_branch, '')
                        END,
                        ''
                    ),
                    '(untagged)'
                ) AS git_branch,
                COALESCE(input_tokens, 0) AS input_tokens,
                COALESCE(output_tokens, 0) AS output_tokens,
                COALESCE(cache_creation_tokens, 0) AS cache_creation_tokens,
                COALESCE(cache_read_tokens, 0) AS cache_read_tokens,
                COALESCE(cost_cents, 0.0) AS cost_cents
            FROM messages
        )
        INSERT INTO message_rollups_daily (
            bucket_day, role, provider, model, repo_id, git_branch,
            message_count, input_tokens, output_tokens,
            cache_creation_tokens, cache_read_tokens, cost_cents
        )
        SELECT
            bucket_day, role, provider, model, repo_id, git_branch,
            COUNT(*) AS message_count,
            COALESCE(SUM(input_tokens), 0),
            COALESCE(SUM(output_tokens), 0),
            COALESCE(SUM(cache_creation_tokens), 0),
            COALESCE(SUM(cache_read_tokens), 0),
            COALESCE(SUM(cost_cents), 0.0)
        FROM normalized
        GROUP BY bucket_day, role, provider, model, repo_id, git_branch;
        ",
    )?;
    Ok(())
}

fn create_indexes(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id);
        CREATE INDEX IF NOT EXISTS idx_messages_timestamp ON messages(timestamp);
        CREATE INDEX IF NOT EXISTS idx_messages_session_ts ON messages(session_id, timestamp);
        CREATE INDEX IF NOT EXISTS idx_messages_repo ON messages(repo_id);
        CREATE INDEX IF NOT EXISTS idx_messages_provider ON messages(provider);
        CREATE INDEX IF NOT EXISTS idx_messages_parent ON messages(parent_uuid);
        CREATE INDEX IF NOT EXISTS idx_messages_branch ON messages(git_branch);
        CREATE INDEX IF NOT EXISTS idx_messages_role ON messages(role);

        CREATE INDEX IF NOT EXISTS idx_tags_key_value ON tags(key, value);
        CREATE INDEX IF NOT EXISTS idx_tags_message ON tags(message_id);
        CREATE INDEX IF NOT EXISTS idx_tags_msg_key_val ON tags(message_id, key, value);

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

        CREATE INDEX IF NOT EXISTS idx_sessions_provider ON sessions(provider);
        CREATE INDEX IF NOT EXISTS idx_sessions_started ON sessions(started_at);
        CREATE INDEX IF NOT EXISTS idx_sessions_id ON sessions(id);
        CREATE INDEX IF NOT EXISTS idx_sessions_session_id ON sessions(id);

        CREATE INDEX IF NOT EXISTS idx_message_tags_pair ON tags(message_id, key, value);
        CREATE INDEX IF NOT EXISTS idx_messages_primary_id ON messages(id);
        ",
    )?;

    if table_exists(conn, "message_rollups_hourly")? {
        conn.execute_batch(
            "
            CREATE INDEX IF NOT EXISTS idx_rollups_hourly_bucket ON message_rollups_hourly(bucket_start);
            CREATE INDEX IF NOT EXISTS idx_rollups_hourly_dims ON message_rollups_hourly(provider, model, repo_id, git_branch, role);
            ",
        )?;
    }

    if table_exists(conn, "message_rollups_daily")? {
        conn.execute_batch(
            "
            CREATE INDEX IF NOT EXISTS idx_rollups_daily_bucket ON message_rollups_daily(bucket_day);
            CREATE INDEX IF NOT EXISTS idx_rollups_daily_dims ON message_rollups_daily(provider, model, repo_id, git_branch, role);
            ",
        )?;
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

#[allow(dead_code)]
fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let cols = stmt.query_map([], |row| row.get::<_, String>(1))?;
    Ok(cols.filter_map(|c| c.ok()).any(|c| c == column))
}

#[allow(dead_code)]
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

fn index_exists(conn: &Connection, name: &str) -> Result<bool> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='index' AND name = ?1)",
        [name],
        |row| row.get(0),
    )?;
    Ok(exists)
}

fn trigger_exists(conn: &Connection, name: &str) -> Result<bool> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='trigger' AND name = ?1)",
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
        "idx_tags_message".to_string(),
        "idx_tags_msg_key_val".to_string(),
        "idx_sessions_id".to_string(),
        "idx_sessions_session_id".to_string(),
        "idx_message_tags_pair".to_string(),
        "idx_messages_primary_id".to_string(),
    ];

    if table_exists(conn, "message_rollups_hourly")? {
        indexes.push("idx_rollups_hourly_bucket".to_string());
        indexes.push("idx_rollups_hourly_dims".to_string());
    }

    if table_exists(conn, "message_rollups_daily")? {
        indexes.push("idx_rollups_daily_bucket".to_string());
        indexes.push("idx_rollups_daily_dims".to_string());
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

    let has_hourly_rollups = table_exists(conn, "message_rollups_hourly")?;
    let has_daily_rollups = table_exists(conn, "message_rollups_daily")?;
    let has_rollup_insert_trigger = trigger_exists(conn, "trg_messages_rollup_insert")?;
    let has_rollup_delete_trigger = trigger_exists(conn, "trg_messages_rollup_delete")?;
    let has_rollup_update_trigger = trigger_exists(conn, "trg_messages_rollup_update")?;
    let needs_rollup_repair = !has_hourly_rollups
        || !has_daily_rollups
        || !has_rollup_insert_trigger
        || !has_rollup_delete_trigger
        || !has_rollup_update_trigger;
    if needs_rollup_repair {
        ensure_rollup_schema(conn, true)?;
        if !has_hourly_rollups {
            added_columns.push("message_rollups_hourly".to_string());
        }
        if !has_daily_rollups {
            added_columns.push("message_rollups_daily".to_string());
        }
        if !has_rollup_insert_trigger {
            added_columns.push("trg_messages_rollup_insert".to_string());
        }
        if !has_rollup_delete_trigger {
            added_columns.push("trg_messages_rollup_delete".to_string());
        }
        if !has_rollup_update_trigger {
            added_columns.push("trg_messages_rollup_update".to_string());
        }
    }

    let added_indexes = missing_reconcile_indexes(conn)?;

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

    fn assert_core_schema(conn: &Connection) {
        assert_eq!(current_version(conn), SCHEMA_VERSION);
        conn.execute_batch("SELECT id FROM messages LIMIT 0")
            .unwrap();
        conn.execute_batch("SELECT id FROM sessions LIMIT 0")
            .unwrap();
        conn.execute_batch("SELECT message_id FROM tags LIMIT 0")
            .unwrap();
        assert!(table_exists(conn, "message_rollups_hourly").unwrap());
        assert!(table_exists(conn, "message_rollups_daily").unwrap());
        assert!(trigger_exists(conn, "trg_messages_rollup_insert").unwrap());
        assert!(trigger_exists(conn, "trg_messages_rollup_delete").unwrap());
        assert!(trigger_exists(conn, "trg_messages_rollup_update").unwrap());
    }

    #[test]
    fn fresh_install_creates_correct_schema() {
        let conn = Connection::open_in_memory().unwrap();
        assert_eq!(current_version(&conn), 0);
        assert!(needs_migration(&conn));

        migrate(&conn).unwrap();

        assert!(!needs_migration(&conn));
        assert_core_schema(&conn);
    }

    #[test]
    fn repair_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        let first = repair(&conn).unwrap();
        assert_eq!(first.from_version, SCHEMA_VERSION);
        assert_eq!(first.to_version, SCHEMA_VERSION);
        assert!(!first.migrated);
        assert!(first.added_columns.is_empty());
        assert!(first.added_indexes.is_empty());

        let second = repair(&conn).unwrap();
        assert_eq!(second.from_version, SCHEMA_VERSION);
        assert!(!second.migrated);
        assert!(second.added_columns.is_empty());
        assert!(second.added_indexes.is_empty());
    }

    #[test]
    fn drop_and_recreate_for_non_matching_version() {
        for old_version in [2u32, 7, 10, 22, 99] {
            let conn = Connection::open_in_memory().unwrap();
            conn.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
            conn.execute_batch(
                "
                CREATE TABLE legacy_junk (x INTEGER);
                CREATE TABLE messages (wrong_schema INTEGER);
                ",
            )
            .unwrap();
            conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
            conn.pragma_update(None, "user_version", old_version)
                .unwrap();

            assert_ne!(current_version(&conn), SCHEMA_VERSION);
            assert!(needs_migration(&conn));

            migrate(&conn).unwrap();

            assert_eq!(current_version(&conn), SCHEMA_VERSION);
            assert!(!needs_migration(&conn));
            assert_core_schema(&conn);

            let junk: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='legacy_junk'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(junk, 0, "old tables should be dropped (v{old_version})");
        }
    }
}
