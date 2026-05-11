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
    pub removed_tables: Vec<String>,
}

/// Report from [`reconcile_schema`] (additive repairs and rollup healing).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SchemaReconcileReport {
    pub added_columns: Vec<String>,
    pub added_indexes: Vec<String>,
    pub removed_tables: Vec<String>,
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
        removed_tables: reconcile.removed_tables,
    })
}

/// Diagnose schema drift without applying any changes.
///
/// Returns a [`RepairReport`] populated as if [`repair`] had been called,
/// but the database is left untouched. `migrated` indicates that a
/// migration is needed; `added_columns` / `added_indexes` /
/// `removed_tables` enumerate the additive drift that [`repair`] would
/// fix. When `migrated` is true the drift lists are empty: the version
/// migration would recreate the schema from scratch, so per-item drift
/// is moot.
pub fn check(conn: &Connection) -> Result<RepairReport> {
    let from_version = current_version(conn);
    let to_version = SCHEMA_VERSION;
    let migrated = from_version != to_version;

    let (added_columns, added_indexes, removed_tables) = if migrated {
        (Vec::new(), Vec::new(), Vec::new())
    } else {
        detect_drift(conn)?
    };

    Ok(RepairReport {
        from_version,
        to_version,
        migrated,
        added_columns,
        added_indexes,
        removed_tables,
    })
}

fn detect_drift(conn: &Connection) -> Result<(Vec<String>, Vec<String>, Vec<String>)> {
    let mut added_columns: Vec<String> = Vec::new();
    let mut removed_tables: Vec<String> = Vec::new();

    if !table_exists(conn, "message_rollups_hourly")? {
        added_columns.push("message_rollups_hourly".to_string());
    }
    if !table_exists(conn, "message_rollups_daily")? {
        added_columns.push("message_rollups_daily".to_string());
    }
    for trigger in [
        "trg_messages_rollup_insert",
        "trg_messages_rollup_delete",
        "trg_messages_rollup_update",
    ] {
        if !trigger_exists(conn, trigger)? {
            added_columns.push(trigger.to_string());
        }
    }
    if !table_exists(conn, "tail_offsets")? {
        added_columns.push("tail_offsets".to_string());
    }
    if table_exists(conn, "messages")? && !has_column(conn, "messages", "pricing_source")? {
        added_columns.push("messages.pricing_source".to_string());
    }
    if !table_exists(conn, "pricing_manifests")? {
        added_columns.push("pricing_manifests".to_string());
    }
    if !table_exists(conn, "recalculation_runs_local")? {
        added_columns.push("recalculation_runs_local".to_string());
    }
    // #730 / ADR-0094 §1: dual-column cost shape on messages and both rollup
    // tables. Missing either column on any of these tables is drift.
    for table in [
        "messages",
        "message_rollups_hourly",
        "message_rollups_daily",
    ] {
        if !table_exists(conn, table)? {
            continue;
        }
        if !has_column(conn, table, "cost_cents_ingested")? {
            added_columns.push(format!("{table}.cost_cents_ingested"));
        }
        if !has_column(conn, table, "cost_cents_effective")? {
            added_columns.push(format!("{table}.cost_cents_effective"));
        }
    }
    if table_exists(conn, "messages")? && !has_column(conn, "messages", "surface")? {
        added_columns.push("messages.surface".to_string());
    }
    if table_exists(conn, "sessions")? && !has_column(conn, "sessions", "surface")? {
        added_columns.push("sessions.surface".to_string());
    }
    if table_exists(conn, "proxy_events")? {
        removed_tables.push("proxy_events".to_string());
    }

    let added_indexes = missing_reconcile_indexes(conn)?;

    Ok((added_columns, added_indexes, removed_tables))
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
            cost_cents_ingested    REAL NOT NULL DEFAULT 0,
            cost_cents_effective   REAL,
            parent_uuid            TEXT,
            git_branch             TEXT,
            cost_confidence        TEXT DEFAULT 'estimated',
            request_id             TEXT,
            pricing_source         TEXT NOT NULL DEFAULT 'legacy:pre-manifest',
            surface                TEXT NOT NULL DEFAULT 'unknown'
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
    ensure_tail_offsets(conn)?;
    ensure_pricing_manifests(conn)?;
    seed_pricing_manifests_baseline(conn)?;
    ensure_recalculation_runs_local(conn)?;
    create_indexes(conn)?;
    Ok(())
}

/// #731 / ADR-0094 §7: lightweight local audit log of team-pricing
/// recompute passes. One row per [`pricing::team::recompute_messages`]
/// invocation. Mirrors the cloud's `recalculation_runs` shape minus the
/// columns that only make sense org-side (`org_id`, `price_list_ids[]`,
/// `triggered_by`). Surfaced by `budi pricing status` (#732).
fn ensure_recalculation_runs_local(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS recalculation_runs_local (
            id                  INTEGER PRIMARY KEY AUTOINCREMENT,
            started_at          TEXT NOT NULL,
            finished_at         TEXT NOT NULL,
            list_version        INTEGER NOT NULL DEFAULT 0,
            rows_processed      INTEGER NOT NULL DEFAULT 0,
            rows_changed        INTEGER NOT NULL DEFAULT 0,
            before_total_cents  REAL NOT NULL DEFAULT 0,
            after_total_cents   REAL NOT NULL DEFAULT 0
        );
        ",
    )?;
    Ok(())
}

/// Per-(provider, file) byte offset table used by the daemon's live tailer
/// (see [ADR-0089] §1 and #319).
///
/// This is intentionally distinct from `sync_state` (which is keyed on file
/// path alone and shared with `budi db import`). The tailer needs:
/// - a per-provider scope so two providers sharing a watch root cannot
///   stomp on each other's offsets,
/// - a `last_seen` timestamp so future tooling can prune stale rows
///   without crawling the filesystem.
///
/// Offsets are byte counts into the JSONL file, identical in semantics to
/// `sync_state.byte_offset` so the `Provider::parse_file(path, content,
/// offset)` contract works unchanged.
///
/// [ADR-0089]: https://github.com/siropkin/budi/blob/main/docs/adr/0089-reverse-proxy-first-jsonl-tailing-as-sole-live-path.md
fn ensure_tail_offsets(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS tail_offsets (
            provider     TEXT NOT NULL,
            path         TEXT NOT NULL,
            byte_offset  INTEGER NOT NULL DEFAULT 0,
            last_seen    TEXT NOT NULL,
            PRIMARY KEY (provider, path)
        );
        ",
    )?;
    Ok(())
}

/// Pricing manifest audit log per ADR-0091 §7.
///
/// One row per successful manifest install — including the synthetic
/// `version = 0` row for pre-manifest history and the `version = 1` row
/// for the embedded baseline loaded at migration time. Subsequent refresh
/// worker fetches append `version = 2, 3, ...`. `version` is the monotonic
/// identifier embedded in `pricing_source` column values (`manifest:vNNN`
/// / `backfilled:vNNN`).
fn ensure_pricing_manifests(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS pricing_manifests (
            version             INTEGER PRIMARY KEY,
            fetched_at          TEXT,
            source              TEXT NOT NULL,
            upstream_etag       TEXT,
            known_model_count   INTEGER NOT NULL DEFAULT 0
        );
        ",
    )?;
    Ok(())
}

/// Seed the version-0 pre-manifest anchor and the version-1 embedded
/// baseline row per ADR-0091 §7 steps 3 + 5.
///
/// DB-only — no network fetch happens here (§7 step 4 deferred to the
/// daemon refresh worker so `budi init` stays fast on flaky networks).
/// `INSERT OR IGNORE` keeps this idempotent across repeated migrations.
fn seed_pricing_manifests_baseline(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "INSERT OR IGNORE INTO pricing_manifests
            (version, fetched_at, source, upstream_etag, known_model_count)
         VALUES (0, NULL, 'pre-manifest', NULL, 0);",
    )?;
    // Count the embedded baseline so the audit row is honest. If parsing
    // fails (broken vendored JSON — caught by the #376 §10 CI guard in
    // practice) we still insert the row so the version ladder is
    // contiguous, but with a zero count.
    let count = crate::pricing::load_embedded_manifest()
        .map(|m| m.entries.len())
        .unwrap_or(0) as i64;
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT OR IGNORE INTO pricing_manifests
            (version, fetched_at, source, upstream_etag, known_model_count)
         VALUES (1, ?1, 'embedded', NULL, ?2);",
        rusqlite::params![now, count],
    )?;
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
            title              TEXT,
            surface            TEXT NOT NULL DEFAULT 'unknown'
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
            surface                TEXT NOT NULL DEFAULT 'unknown',
            message_count          INTEGER NOT NULL DEFAULT 0,
            input_tokens           INTEGER NOT NULL DEFAULT 0,
            output_tokens          INTEGER NOT NULL DEFAULT 0,
            cache_creation_tokens  INTEGER NOT NULL DEFAULT 0,
            cache_read_tokens      INTEGER NOT NULL DEFAULT 0,
            cost_cents_ingested    REAL NOT NULL DEFAULT 0,
            cost_cents_effective   REAL NOT NULL DEFAULT 0,
            PRIMARY KEY(bucket_start, role, provider, model, repo_id, git_branch, surface)
        );

        CREATE TABLE IF NOT EXISTS message_rollups_daily (
            bucket_day             TEXT NOT NULL,
            role                   TEXT NOT NULL,
            provider               TEXT NOT NULL,
            model                  TEXT NOT NULL,
            repo_id                TEXT NOT NULL,
            git_branch             TEXT NOT NULL,
            surface                TEXT NOT NULL DEFAULT 'unknown',
            message_count          INTEGER NOT NULL DEFAULT 0,
            input_tokens           INTEGER NOT NULL DEFAULT 0,
            output_tokens          INTEGER NOT NULL DEFAULT 0,
            cache_creation_tokens  INTEGER NOT NULL DEFAULT 0,
            cache_read_tokens      INTEGER NOT NULL DEFAULT 0,
            cost_cents_ingested    REAL NOT NULL DEFAULT 0,
            cost_cents_effective   REAL NOT NULL DEFAULT 0,
            PRIMARY KEY(bucket_day, role, provider, model, repo_id, git_branch, surface)
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
                bucket_start, role, provider, model, repo_id, git_branch, surface,
                message_count, input_tokens, output_tokens,
                cache_creation_tokens, cache_read_tokens,
                cost_cents_ingested, cost_cents_effective
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
                COALESCE(NULLIF(NEW.surface, ''), 'unknown'),
                1,
                COALESCE(NEW.input_tokens, 0),
                COALESCE(NEW.output_tokens, 0),
                COALESCE(NEW.cache_creation_tokens, 0),
                COALESCE(NEW.cache_read_tokens, 0),
                COALESCE(NEW.cost_cents_ingested, 0.0),
                COALESCE(NEW.cost_cents_effective, 0.0)
            )
            ON CONFLICT(bucket_start, role, provider, model, repo_id, git_branch, surface) DO UPDATE SET
                message_count = message_count + excluded.message_count,
                input_tokens = input_tokens + excluded.input_tokens,
                output_tokens = output_tokens + excluded.output_tokens,
                cache_creation_tokens = cache_creation_tokens + excluded.cache_creation_tokens,
                cache_read_tokens = cache_read_tokens + excluded.cache_read_tokens,
                cost_cents_ingested = cost_cents_ingested + excluded.cost_cents_ingested,
                cost_cents_effective = cost_cents_effective + excluded.cost_cents_effective;

            INSERT INTO message_rollups_daily (
                bucket_day, role, provider, model, repo_id, git_branch, surface,
                message_count, input_tokens, output_tokens,
                cache_creation_tokens, cache_read_tokens,
                cost_cents_ingested, cost_cents_effective
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
                COALESCE(NULLIF(NEW.surface, ''), 'unknown'),
                1,
                COALESCE(NEW.input_tokens, 0),
                COALESCE(NEW.output_tokens, 0),
                COALESCE(NEW.cache_creation_tokens, 0),
                COALESCE(NEW.cache_read_tokens, 0),
                COALESCE(NEW.cost_cents_ingested, 0.0),
                COALESCE(NEW.cost_cents_effective, 0.0)
            )
            ON CONFLICT(bucket_day, role, provider, model, repo_id, git_branch, surface) DO UPDATE SET
                message_count = message_count + excluded.message_count,
                input_tokens = input_tokens + excluded.input_tokens,
                output_tokens = output_tokens + excluded.output_tokens,
                cache_creation_tokens = cache_creation_tokens + excluded.cache_creation_tokens,
                cache_read_tokens = cache_read_tokens + excluded.cache_read_tokens,
                cost_cents_ingested = cost_cents_ingested + excluded.cost_cents_ingested,
                cost_cents_effective = cost_cents_effective + excluded.cost_cents_effective;
        END;

        CREATE TRIGGER IF NOT EXISTS trg_messages_rollup_delete
        AFTER DELETE ON messages
        BEGIN
            INSERT INTO message_rollups_hourly (
                bucket_start, role, provider, model, repo_id, git_branch, surface,
                message_count, input_tokens, output_tokens,
                cache_creation_tokens, cache_read_tokens,
                cost_cents_ingested, cost_cents_effective
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
                COALESCE(NULLIF(OLD.surface, ''), 'unknown'),
                -1,
                -COALESCE(OLD.input_tokens, 0),
                -COALESCE(OLD.output_tokens, 0),
                -COALESCE(OLD.cache_creation_tokens, 0),
                -COALESCE(OLD.cache_read_tokens, 0),
                -COALESCE(OLD.cost_cents_ingested, 0.0),
                -COALESCE(OLD.cost_cents_effective, 0.0)
            )
            ON CONFLICT(bucket_start, role, provider, model, repo_id, git_branch, surface) DO UPDATE SET
                message_count = message_count + excluded.message_count,
                input_tokens = input_tokens + excluded.input_tokens,
                output_tokens = output_tokens + excluded.output_tokens,
                cache_creation_tokens = cache_creation_tokens + excluded.cache_creation_tokens,
                cache_read_tokens = cache_read_tokens + excluded.cache_read_tokens,
                cost_cents_ingested = cost_cents_ingested + excluded.cost_cents_ingested,
                cost_cents_effective = cost_cents_effective + excluded.cost_cents_effective;

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
               AND surface = COALESCE(NULLIF(OLD.surface, ''), 'unknown')
               AND message_count <= 0;

            INSERT INTO message_rollups_daily (
                bucket_day, role, provider, model, repo_id, git_branch, surface,
                message_count, input_tokens, output_tokens,
                cache_creation_tokens, cache_read_tokens,
                cost_cents_ingested, cost_cents_effective
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
                COALESCE(NULLIF(OLD.surface, ''), 'unknown'),
                -1,
                -COALESCE(OLD.input_tokens, 0),
                -COALESCE(OLD.output_tokens, 0),
                -COALESCE(OLD.cache_creation_tokens, 0),
                -COALESCE(OLD.cache_read_tokens, 0),
                -COALESCE(OLD.cost_cents_ingested, 0.0),
                -COALESCE(OLD.cost_cents_effective, 0.0)
            )
            ON CONFLICT(bucket_day, role, provider, model, repo_id, git_branch, surface) DO UPDATE SET
                message_count = message_count + excluded.message_count,
                input_tokens = input_tokens + excluded.input_tokens,
                output_tokens = output_tokens + excluded.output_tokens,
                cache_creation_tokens = cache_creation_tokens + excluded.cache_creation_tokens,
                cache_read_tokens = cache_read_tokens + excluded.cache_read_tokens,
                cost_cents_ingested = cost_cents_ingested + excluded.cost_cents_ingested,
                cost_cents_effective = cost_cents_effective + excluded.cost_cents_effective;

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
               AND surface = COALESCE(NULLIF(OLD.surface, ''), 'unknown')
               AND message_count <= 0;
        END;

        CREATE TRIGGER IF NOT EXISTS trg_messages_rollup_update
        AFTER UPDATE ON messages
        BEGIN
            INSERT INTO message_rollups_hourly (
                bucket_start, role, provider, model, repo_id, git_branch, surface,
                message_count, input_tokens, output_tokens,
                cache_creation_tokens, cache_read_tokens,
                cost_cents_ingested, cost_cents_effective
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
                COALESCE(NULLIF(OLD.surface, ''), 'unknown'),
                -1,
                -COALESCE(OLD.input_tokens, 0),
                -COALESCE(OLD.output_tokens, 0),
                -COALESCE(OLD.cache_creation_tokens, 0),
                -COALESCE(OLD.cache_read_tokens, 0),
                -COALESCE(OLD.cost_cents_ingested, 0.0),
                -COALESCE(OLD.cost_cents_effective, 0.0)
            )
            ON CONFLICT(bucket_start, role, provider, model, repo_id, git_branch, surface) DO UPDATE SET
                message_count = message_count + excluded.message_count,
                input_tokens = input_tokens + excluded.input_tokens,
                output_tokens = output_tokens + excluded.output_tokens,
                cache_creation_tokens = cache_creation_tokens + excluded.cache_creation_tokens,
                cache_read_tokens = cache_read_tokens + excluded.cache_read_tokens,
                cost_cents_ingested = cost_cents_ingested + excluded.cost_cents_ingested,
                cost_cents_effective = cost_cents_effective + excluded.cost_cents_effective;

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
               AND surface = COALESCE(NULLIF(OLD.surface, ''), 'unknown')
               AND message_count <= 0;

            INSERT INTO message_rollups_daily (
                bucket_day, role, provider, model, repo_id, git_branch, surface,
                message_count, input_tokens, output_tokens,
                cache_creation_tokens, cache_read_tokens,
                cost_cents_ingested, cost_cents_effective
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
                COALESCE(NULLIF(OLD.surface, ''), 'unknown'),
                -1,
                -COALESCE(OLD.input_tokens, 0),
                -COALESCE(OLD.output_tokens, 0),
                -COALESCE(OLD.cache_creation_tokens, 0),
                -COALESCE(OLD.cache_read_tokens, 0),
                -COALESCE(OLD.cost_cents_ingested, 0.0),
                -COALESCE(OLD.cost_cents_effective, 0.0)
            )
            ON CONFLICT(bucket_day, role, provider, model, repo_id, git_branch, surface) DO UPDATE SET
                message_count = message_count + excluded.message_count,
                input_tokens = input_tokens + excluded.input_tokens,
                output_tokens = output_tokens + excluded.output_tokens,
                cache_creation_tokens = cache_creation_tokens + excluded.cache_creation_tokens,
                cache_read_tokens = cache_read_tokens + excluded.cache_read_tokens,
                cost_cents_ingested = cost_cents_ingested + excluded.cost_cents_ingested,
                cost_cents_effective = cost_cents_effective + excluded.cost_cents_effective;

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
               AND surface = COALESCE(NULLIF(OLD.surface, ''), 'unknown')
               AND message_count <= 0;

            INSERT INTO message_rollups_hourly (
                bucket_start, role, provider, model, repo_id, git_branch, surface,
                message_count, input_tokens, output_tokens,
                cache_creation_tokens, cache_read_tokens,
                cost_cents_ingested, cost_cents_effective
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
                COALESCE(NULLIF(NEW.surface, ''), 'unknown'),
                1,
                COALESCE(NEW.input_tokens, 0),
                COALESCE(NEW.output_tokens, 0),
                COALESCE(NEW.cache_creation_tokens, 0),
                COALESCE(NEW.cache_read_tokens, 0),
                COALESCE(NEW.cost_cents_ingested, 0.0),
                COALESCE(NEW.cost_cents_effective, 0.0)
            )
            ON CONFLICT(bucket_start, role, provider, model, repo_id, git_branch, surface) DO UPDATE SET
                message_count = message_count + excluded.message_count,
                input_tokens = input_tokens + excluded.input_tokens,
                output_tokens = output_tokens + excluded.output_tokens,
                cache_creation_tokens = cache_creation_tokens + excluded.cache_creation_tokens,
                cache_read_tokens = cache_read_tokens + excluded.cache_read_tokens,
                cost_cents_ingested = cost_cents_ingested + excluded.cost_cents_ingested,
                cost_cents_effective = cost_cents_effective + excluded.cost_cents_effective;

            INSERT INTO message_rollups_daily (
                bucket_day, role, provider, model, repo_id, git_branch, surface,
                message_count, input_tokens, output_tokens,
                cache_creation_tokens, cache_read_tokens,
                cost_cents_ingested, cost_cents_effective
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
                COALESCE(NULLIF(NEW.surface, ''), 'unknown'),
                1,
                COALESCE(NEW.input_tokens, 0),
                COALESCE(NEW.output_tokens, 0),
                COALESCE(NEW.cache_creation_tokens, 0),
                COALESCE(NEW.cache_read_tokens, 0),
                COALESCE(NEW.cost_cents_ingested, 0.0),
                COALESCE(NEW.cost_cents_effective, 0.0)
            )
            ON CONFLICT(bucket_day, role, provider, model, repo_id, git_branch, surface) DO UPDATE SET
                message_count = message_count + excluded.message_count,
                input_tokens = input_tokens + excluded.input_tokens,
                output_tokens = output_tokens + excluded.output_tokens,
                cache_creation_tokens = cache_creation_tokens + excluded.cache_creation_tokens,
                cache_read_tokens = cache_read_tokens + excluded.cache_read_tokens,
                cost_cents_ingested = cost_cents_ingested + excluded.cost_cents_ingested,
                cost_cents_effective = cost_cents_effective + excluded.cost_cents_effective;
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
                COALESCE(NULLIF(surface, ''), 'unknown') AS surface,
                COALESCE(input_tokens, 0) AS input_tokens,
                COALESCE(output_tokens, 0) AS output_tokens,
                COALESCE(cache_creation_tokens, 0) AS cache_creation_tokens,
                COALESCE(cache_read_tokens, 0) AS cache_read_tokens,
                COALESCE(cost_cents_ingested, 0.0) AS cost_cents_ingested,
                COALESCE(cost_cents_effective, 0.0) AS cost_cents_effective
            FROM messages
        )
        INSERT INTO message_rollups_hourly (
            bucket_start, role, provider, model, repo_id, git_branch, surface,
            message_count, input_tokens, output_tokens,
            cache_creation_tokens, cache_read_tokens,
            cost_cents_ingested, cost_cents_effective
        )
        SELECT
            bucket_hour, role, provider, model, repo_id, git_branch, surface,
            COUNT(*) AS message_count,
            COALESCE(SUM(input_tokens), 0),
            COALESCE(SUM(output_tokens), 0),
            COALESCE(SUM(cache_creation_tokens), 0),
            COALESCE(SUM(cache_read_tokens), 0),
            COALESCE(SUM(cost_cents_ingested), 0.0),
            COALESCE(SUM(cost_cents_effective), 0.0)
        FROM normalized
        GROUP BY bucket_hour, role, provider, model, repo_id, git_branch, surface;

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
                COALESCE(NULLIF(surface, ''), 'unknown') AS surface,
                COALESCE(input_tokens, 0) AS input_tokens,
                COALESCE(output_tokens, 0) AS output_tokens,
                COALESCE(cache_creation_tokens, 0) AS cache_creation_tokens,
                COALESCE(cache_read_tokens, 0) AS cache_read_tokens,
                COALESCE(cost_cents_ingested, 0.0) AS cost_cents_ingested,
                COALESCE(cost_cents_effective, 0.0) AS cost_cents_effective
            FROM messages
        )
        INSERT INTO message_rollups_daily (
            bucket_day, role, provider, model, repo_id, git_branch, surface,
            message_count, input_tokens, output_tokens,
            cache_creation_tokens, cache_read_tokens,
            cost_cents_ingested, cost_cents_effective
        )
        SELECT
            bucket_day, role, provider, model, repo_id, git_branch, surface,
            COUNT(*) AS message_count,
            COALESCE(SUM(input_tokens), 0),
            COALESCE(SUM(output_tokens), 0),
            COALESCE(SUM(cache_creation_tokens), 0),
            COALESCE(SUM(cache_read_tokens), 0),
            COALESCE(SUM(cost_cents_ingested), 0.0),
            COALESCE(SUM(cost_cents_effective), 0.0)
        FROM normalized
        GROUP BY bucket_day, role, provider, model, repo_id, git_branch, surface;
        ",
    )?;
    Ok(())
}

/// #442: normalize pre-8.3 bare-folder-name `repo_id` values to NULL.
///
/// Pre-8.3 `resolve_repo_id` fell back to the git-root folder name when
/// a repo had no remote, and to the cwd's folder name when there was no
/// git at all. That produced rows like `Desktop`, `ivan.seredkin`,
/// `.cursor`, and `homebrew-budi` that sat alongside real
/// `github.com/owner/repo` rows in `budi stats --projects`.
///
/// The 8.3 classifier returns `None` for any cwd that isn't inside a
/// git repo with a remote, so new ingests stay clean. This one-shot
/// cleanup touches historical rows: anything whose `repo_id` doesn't
/// match the normalized `host/owner/repo` shape (host must contain a
/// `.`, plus `owner/repo` segments) is rewritten to NULL in both the
/// `messages` and `sessions` tables.
///
/// Idempotent: the `NOT (...)` predicate is already empty on rows that
/// passed a previous run, so subsequent boots no-op.
///
/// Returns the number of messages+sessions rows updated (used by the
/// caller to decide whether to rebuild rollups).
fn backfill_non_repo_ids_to_null(conn: &Connection) -> Result<usize> {
    // Matches `crate::repo_id::looks_like_repo_url` in SQL form: at least
    // two `/` separators AND the first segment contains a `.`.
    let predicate = "repo_id IS NOT NULL
         AND repo_id != ''
         AND NOT (
             repo_id LIKE '%/%/%'
             AND INSTR(repo_id, '/') > 1
             AND SUBSTR(repo_id, 1, INSTR(repo_id, '/') - 1) LIKE '%.%'
         )";

    let mut total = 0usize;
    total += conn.execute(
        &format!("UPDATE messages SET repo_id = NULL WHERE {predicate}"),
        [],
    )?;
    total += conn.execute(
        &format!("UPDATE sessions SET repo_id = NULL WHERE {predicate}"),
        [],
    )?;
    Ok(total)
}

/// #569 / #578 / #577: heal `sessions` rows whose `started_at`, `ended_at`,
/// `repo_id`, or `git_branch` are stale or missing but whose `messages` table
/// has data for them.
///
/// Pre-fix, the message ingest path inserted stub session rows with only
/// `(id, provider)`, leaving timestamps and repo/branch NULL.
/// `cloud_sync::fetch_session_summaries` requires `started_at` to be NOT NULL,
/// so those sessions never reached the cloud. This pass fills the four
/// derivable columns from the linked messages.
///
/// `started_at` is immutable (the session's first observed message), so it
/// uses `COALESCE` to preserve any already-set value (e.g. one written by a
/// future provider-authoritative path). `ended_at` is the session's *last*
/// observed message and must keep advancing as new messages stream in for
/// in-flight sessions — `COALESCE` would freeze it at the first ingest tick's
/// MAX (#578), so this pass always recomputes MAX(messages.timestamp) for any
/// session whose stored `ended_at` is older. `repo_id` and `git_branch`
/// (#577) likewise live on every message row; we backfill them from the
/// session's most-recent message so a branch switch mid-session is reflected.
///
/// The WHERE predicate is empty after one full sweep on a stable DB — so
/// steady-state cost stays the same as the v8.3.11 heal.
pub fn backfill_session_timestamps_from_messages(conn: &Connection) -> Result<usize> {
    let count = conn.execute(
        "UPDATE sessions SET
            started_at = COALESCE(started_at,
                (SELECT MIN(timestamp) FROM messages WHERE session_id = sessions.id)),
            ended_at =
                (SELECT MAX(timestamp) FROM messages WHERE session_id = sessions.id),
            repo_id = COALESCE(NULLIF(sessions.repo_id, ''),
                (SELECT m.repo_id FROM messages m
                  WHERE m.session_id = sessions.id
                    AND m.repo_id IS NOT NULL AND m.repo_id <> ''
                  ORDER BY m.timestamp DESC LIMIT 1)),
            git_branch = COALESCE(NULLIF(sessions.git_branch, ''),
                (SELECT m.git_branch FROM messages m
                  WHERE m.session_id = sessions.id
                    AND m.git_branch IS NOT NULL AND m.git_branch <> ''
                  ORDER BY m.timestamp DESC LIMIT 1)),
            surface = CASE
                WHEN sessions.surface IS NULL OR sessions.surface = '' OR sessions.surface = 'unknown'
                THEN COALESCE(
                    (SELECT m.surface FROM messages m
                      WHERE m.session_id = sessions.id
                        AND m.surface IS NOT NULL
                        AND m.surface <> ''
                        AND m.surface <> 'unknown'
                      GROUP BY m.surface
                      ORDER BY COUNT(*) DESC, m.surface ASC
                      LIMIT 1),
                    sessions.surface,
                    'unknown'
                )
                ELSE sessions.surface
            END
         WHERE EXISTS (SELECT 1 FROM messages WHERE session_id = sessions.id)
           AND (
                started_at IS NULL
                OR ended_at IS NULL
                OR ended_at < (SELECT MAX(timestamp) FROM messages WHERE session_id = sessions.id)
                OR (
                    (sessions.repo_id IS NULL OR sessions.repo_id = '')
                    AND EXISTS (SELECT 1 FROM messages m
                                WHERE m.session_id = sessions.id
                                  AND m.repo_id IS NOT NULL AND m.repo_id <> '')
                )
                OR (
                    (sessions.git_branch IS NULL OR sessions.git_branch = '')
                    AND EXISTS (SELECT 1 FROM messages m
                                WHERE m.session_id = sessions.id
                                  AND m.git_branch IS NOT NULL AND m.git_branch <> '')
                )
                OR (
                    (sessions.surface IS NULL OR sessions.surface = '' OR sessions.surface = 'unknown')
                    AND EXISTS (SELECT 1 FROM messages m
                                WHERE m.session_id = sessions.id
                                  AND m.surface IS NOT NULL
                                  AND m.surface <> ''
                                  AND m.surface <> 'unknown')
                )
           )",
        [],
    )?;
    Ok(count)
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

        CREATE INDEX IF NOT EXISTS idx_messages_ts_cost ON messages(timestamp, cost_cents_effective);
        CREATE INDEX IF NOT EXISTS idx_messages_role_ts_cost ON messages(role, timestamp, cost_cents_effective);
        CREATE INDEX IF NOT EXISTS idx_messages_role_branch_cost ON messages(role, git_branch, cost_cents_effective);
        CREATE INDEX IF NOT EXISTS idx_messages_role_branch_ts ON messages(role, git_branch, timestamp);
        CREATE INDEX IF NOT EXISTS idx_messages_role_cwd ON messages(role, cwd);
        CREATE INDEX IF NOT EXISTS idx_messages_session_role ON messages(session_id, role);
        CREATE INDEX IF NOT EXISTS idx_messages_cwd_role ON messages(cwd, role);
        CREATE INDEX IF NOT EXISTS idx_messages_session_role_cost ON messages(session_id, role, cost_cents_effective);
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

/// True iff the `message_rollups_hourly` PRIMARY KEY definition includes
/// `surface`. Used by the #701 reconcile to decide whether the rollup
/// tables need to be rebuilt with the new PK shape on an existing v1 DB.
fn rollup_pk_includes_surface(conn: &Connection) -> Result<bool> {
    if !table_exists(conn, "message_rollups_hourly")? {
        return Ok(false);
    }
    let sql: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='message_rollups_hourly'",
            [],
            |r| r.get(0),
        )
        .ok();
    Ok(sql.as_deref().is_some_and(|s| s.contains("surface")))
}

/// Drop the legacy rollup tables/triggers and recreate them with the
/// `surface`-aware PK shape. Used only in the #701 reconcile path; on a
/// fresh schema `create_rollup_tables` already produces the right PK.
fn rebuild_rollups_with_surface_pk(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        DROP TRIGGER IF EXISTS trg_messages_rollup_insert;
        DROP TRIGGER IF EXISTS trg_messages_rollup_delete;
        DROP TRIGGER IF EXISTS trg_messages_rollup_update;
        DROP TABLE IF EXISTS message_rollups_hourly;
        DROP TABLE IF EXISTS message_rollups_daily;
        ",
    )?;
    create_rollup_tables(conn)?;
    create_rollup_triggers(conn)?;
    Ok(())
}

/// Backfill the `surface` column on `messages` and `sessions` for rows
/// that pre-date #701. Per the ticket:
///
/// - `claude_code` / `codex` / `copilot_cli` → `terminal` (no IDE binding)
/// - `cursor` → `cursor`
/// - `copilot_chat` → best-effort path-based inference from `cwd`:
///   `Cursor/User/...` → `cursor`; `Code/User/...`,
///   `Code - Insiders`, `Code - Exploration`, `VSCodium`, or
///   `~/.vscode-server*`/`~/.vscode-remote*` → `vscode`; otherwise
///   `unknown`
/// - everything else → `terminal` as a coarse fallback
///
/// Idempotent: rows that already carry a non-default surface are not
/// touched, so a second migration run no-ops. Documented as a one-shot,
/// non-reversible step.
fn backfill_surface(conn: &Connection) -> Result<usize> {
    let mut total = 0usize;

    // Messages: stamp every row that still carries the default `unknown`
    // surface with the best inference we have. The `WHERE surface = ''
    // OR surface = 'unknown' OR surface IS NULL` predicate becomes empty
    // on subsequent runs once the column is fully populated.
    total += conn.execute(
        "UPDATE messages SET surface = CASE
            WHEN provider IN ('claude_code', 'codex', 'copilot_cli') THEN 'terminal'
            WHEN provider = 'cursor' THEN 'cursor'
            WHEN provider = 'copilot_chat' THEN
                CASE
                    WHEN cwd LIKE '%/Cursor/User/%' OR cwd LIKE '%/Cursor/%' THEN 'cursor'
                    WHEN cwd LIKE '%/Code/User/%'
                      OR cwd LIKE '%/Code - Insiders/%'
                      OR cwd LIKE '%/Code - Exploration/%'
                      OR cwd LIKE '%/VSCodium/%'
                      OR cwd LIKE '%/.vscode-server%'
                      OR cwd LIKE '%/.vscode-remote%'
                      OR cwd LIKE '%vscode-server%'
                      THEN 'vscode'
                    ELSE 'unknown'
                END
            ELSE 'terminal'
        END
        WHERE surface IS NULL OR surface = '' OR surface = 'unknown'",
        [],
    )?;

    // Sessions inherit the dominant surface of their messages. If a
    // session has no messages yet (cloud-shipped stub, in-flight tail),
    // fall back to the provider rule.
    total += conn.execute(
        "UPDATE sessions SET surface = COALESCE(
            (
                SELECT m.surface FROM messages m
                WHERE m.session_id = sessions.id
                  AND m.surface IS NOT NULL AND m.surface != '' AND m.surface != 'unknown'
                GROUP BY m.surface
                ORDER BY COUNT(*) DESC, m.surface ASC
                LIMIT 1
            ),
            CASE
                WHEN sessions.provider IN ('claude_code', 'codex', 'copilot_cli') THEN 'terminal'
                WHEN sessions.provider = 'cursor' THEN 'cursor'
                ELSE 'unknown'
            END
        )
        WHERE surface IS NULL OR surface = '' OR surface = 'unknown'",
        [],
    )?;

    Ok(total)
}

fn drop_legacy_proxy_events_table(conn: &Connection) -> Result<bool> {
    if !table_exists(conn, "proxy_events")? {
        return Ok(false);
    }

    conn.execute_batch("DROP TABLE proxy_events;")?;
    tracing::info!("Schema reconcile: dropped obsolete proxy_events table");
    Ok(true)
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

/// #730 / ADR-0094 §1: migrate an existing v1 DB from the single `cost_cents`
/// column to the dual `cost_cents_ingested` / `cost_cents_effective` pair.
///
/// Steps (per the ticket scope):
/// 1. `ALTER TABLE … ADD COLUMN cost_cents_ingested REAL NOT NULL DEFAULT 0`
/// 2. `UPDATE … SET cost_cents_ingested = cost_cents` (one-time copy)
/// 3. `ALTER TABLE … RENAME COLUMN cost_cents TO cost_cents_effective`
///
/// Applied to `messages`, `message_rollups_hourly`, and `message_rollups_daily`.
/// SQLite ≥ 3.25 automatically rewrites trigger bodies and index definitions
/// that referenced the renamed column, but the rollup triggers reference the
/// new `cost_cents_ingested` column (which didn't exist when they were
/// authored) so the caller must drop + recreate them after this returns.
///
/// Returns the list of `table.column` strings the caller should surface in
/// the [`SchemaReconcileReport`]. Idempotent: a second run reports no
/// changes because every target column already exists.
fn ensure_dual_cost_columns(conn: &Connection) -> Result<Vec<String>> {
    let mut added: Vec<String> = Vec::new();

    for table in [
        "messages",
        "message_rollups_hourly",
        "message_rollups_daily",
    ] {
        if !table_exists(conn, table)? {
            continue;
        }
        let has_effective = has_column(conn, table, "cost_cents_effective")?;
        let has_ingested = has_column(conn, table, "cost_cents_ingested")?;
        let has_legacy = has_column(conn, table, "cost_cents")?;

        if has_effective && has_ingested {
            continue;
        }

        // Drop rollup triggers up-front: they reference `NEW.cost_cents` /
        // `OLD.cost_cents`, and once we rename or drop the column the
        // trigger bodies become invalid. The caller recreates them with
        // the dual-column shape via `create_rollup_triggers`.
        conn.execute_batch(
            "DROP TRIGGER IF EXISTS trg_messages_rollup_insert;
             DROP TRIGGER IF EXISTS trg_messages_rollup_delete;
             DROP TRIGGER IF EXISTS trg_messages_rollup_update;",
        )?;

        if !has_ingested {
            // Rollups: NOT NULL DEFAULT 0 matches the existing rollup
            // column shape and the spec. messages.cost_cents was
            // nullable, but the new `_ingested` column is always
            // NOT NULL DEFAULT 0 per ADR-0094 §1 — incoming NULL legacy
            // rows get the column DEFAULT before the copy below.
            conn.execute_batch(&format!(
                "ALTER TABLE {table} ADD COLUMN cost_cents_ingested REAL NOT NULL DEFAULT 0;"
            ))?;
            added.push(format!("{table}.cost_cents_ingested"));
        }

        if has_legacy && !has_effective {
            // Copy the legacy column's value (treating NULL as 0) into
            // the new `_ingested` column so history is preserved
            // verbatim. ADR-0091 §5 Rule D: `_ingested` is the
            // ADR-0091-immutable column from this point forward.
            conn.execute_batch(&format!(
                "UPDATE {table} SET cost_cents_ingested = COALESCE(cost_cents, 0);"
            ))?;
            // Rename the legacy column. SQLite ≥ 3.25 rewrites trigger
            // bodies and index definitions transparently, so existing
            // `idx_messages_*_cost` indexes now reference the renamed
            // column with no further action.
            conn.execute_batch(&format!(
                "ALTER TABLE {table} RENAME COLUMN cost_cents TO cost_cents_effective;"
            ))?;
            added.push(format!("{table}.cost_cents_effective"));
        } else if !has_effective {
            // No legacy column to rename (the table was created without
            // any cost column — shouldn't happen in practice, but
            // defensive): add it explicitly with the rollup-table
            // semantics. Messages-table inserts always bind a value, so
            // the NOT NULL default never gets exercised on the live
            // path.
            conn.execute_batch(&format!(
                "ALTER TABLE {table} ADD COLUMN cost_cents_effective REAL NOT NULL DEFAULT 0;"
            ))?;
            added.push(format!("{table}.cost_cents_effective"));
        }
    }

    Ok(added)
}

fn reconcile_schema(conn: &Connection) -> Result<SchemaReconcileReport> {
    let mut added_columns: Vec<String> = Vec::new();
    let mut removed_tables: Vec<String> = Vec::new();

    // #701: add the `surface` column to messages/sessions BEFORE the
    // rollup repair runs. The rollup triggers and backfill query both
    // reference `messages.surface` (NEW.surface in trigger bodies, plus
    // a SELECT in `backfill_rollup_tables`); an older v1 DB would
    // otherwise hit "no such column: surface" the moment the new
    // trigger SQL touches an INSERT against `messages`.
    let mut surface_column_added = false;
    if ensure_column(
        conn,
        "messages",
        "surface",
        "surface TEXT NOT NULL DEFAULT 'unknown'",
    )? {
        added_columns.push("messages.surface".to_string());
        surface_column_added = true;
    }
    if ensure_column(
        conn,
        "sessions",
        "surface",
        "surface TEXT NOT NULL DEFAULT 'unknown'",
    )? {
        added_columns.push("sessions.surface".to_string());
        surface_column_added = true;
    }

    // #730 / ADR-0094 §1: migrate `cost_cents` to the dual
    // `cost_cents_ingested` / `cost_cents_effective` shape on
    // messages and both rollup tables before the rollup-repair
    // block below — `create_rollup_triggers` writes the new
    // column names and would fail against a legacy single-column
    // schema. If `ensure_dual_cost_columns` rewrites columns it
    // also drops the legacy triggers so the recreate path below
    // produces them with the dual-column shape.
    let dual_cost_added = ensure_dual_cost_columns(conn)?;
    let dual_cost_columns_added = !dual_cost_added.is_empty();
    for entry in dual_cost_added {
        added_columns.push(entry);
    }

    let has_hourly_rollups = table_exists(conn, "message_rollups_hourly")?;
    let has_daily_rollups = table_exists(conn, "message_rollups_daily")?;
    let has_rollup_insert_trigger = trigger_exists(conn, "trg_messages_rollup_insert")?;
    let has_rollup_delete_trigger = trigger_exists(conn, "trg_messages_rollup_delete")?;
    let has_rollup_update_trigger = trigger_exists(conn, "trg_messages_rollup_update")?;
    let rollup_pk_outdated_pre = has_hourly_rollups && !rollup_pk_includes_surface(conn)?;
    let needs_rollup_repair = !has_hourly_rollups
        || !has_daily_rollups
        || !has_rollup_insert_trigger
        || !has_rollup_delete_trigger
        || !has_rollup_update_trigger
        || rollup_pk_outdated_pre
        || dual_cost_columns_added;
    if rollup_pk_outdated_pre {
        // Pre-#701 rollup tables still carry the old PK. Drop them so
        // `ensure_rollup_schema` recreates with the surface-aware shape;
        // the rebuild below repopulates from `messages`.
        rebuild_rollups_with_surface_pk(conn)?;
    }
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

    let needs_tail_offsets = !table_exists(conn, "tail_offsets")?;
    if needs_tail_offsets {
        ensure_tail_offsets(conn)?;
        added_columns.push("tail_offsets".to_string());
    }

    // ADR-0091 §7: additive upgrade for existing v1 DBs predating 8.3.
    // `pricing_source` defaults every existing row to `legacy:pre-manifest`;
    // the `pricing_manifests` audit table gets seeded with the synthetic
    // v0 row and the embedded-baseline v1 row.
    if ensure_column(
        conn,
        "messages",
        "pricing_source",
        "pricing_source TEXT NOT NULL DEFAULT 'legacy:pre-manifest'",
    )? {
        added_columns.push("messages.pricing_source".to_string());
    }
    if !table_exists(conn, "pricing_manifests")? {
        ensure_pricing_manifests(conn)?;
        added_columns.push("pricing_manifests".to_string());
    }
    seed_pricing_manifests_baseline(conn)?;

    // #731 / ADR-0094 §7: local audit log for team-pricing recompute runs.
    // Additive — fresh installs get it from `create_current_schema`; this
    // backfills the table on DBs that predate 8.4.3.
    if !table_exists(conn, "recalculation_runs_local")? {
        ensure_recalculation_runs_local(conn)?;
        added_columns.push("recalculation_runs_local".to_string());
    }

    // #701: backfill the surface dimension on messages/sessions for
    // rows that pre-date the column add at the top of this function.
    // The column itself was added before `ensure_rollup_schema` so the
    // trigger SQL and the rollup backfill query both have a column to
    // reference; what remains here is to populate the per-row value
    // from provider-local rules and rebuild rollups so they partition
    // on surface from this point forward.
    let backfilled_surface = backfill_surface(conn)?;
    if backfilled_surface > 0 {
        tracing::info!(
            rows = backfilled_surface,
            "Backfilled surface dimension on messages/sessions (#701)"
        );
    }
    if surface_column_added || backfilled_surface > 0 {
        backfill_rollup_tables(conn)?;
    }

    if drop_legacy_proxy_events_table(conn)? {
        removed_tables.push("proxy_events".to_string());
    }

    // #442: normalize pre-8.3 bare-folder-name `repo_id` values to NULL
    // so `budi stats --projects` stops mixing real git remotes
    // (`github.com/…`) with ad-hoc dirs (`Desktop`, `~`, `.cursor`,
    // brew-tap checkouts). Idempotent — the predicate becomes empty on
    // every subsequent run.
    let scrubbed = backfill_non_repo_ids_to_null(conn)?;
    if scrubbed > 0 {
        tracing::info!(
            rows = scrubbed,
            "Normalized non-repo repo_id values to NULL (#442)"
        );
        // Rollups key on `repo_id`, so rebuild them whenever we mutate
        // `messages.repo_id` in bulk. Cheaper than firing per-row
        // UPDATE triggers across a large history.
        backfill_rollup_tables(conn)?;
    }

    // #569: heal sessions that were inserted with NULL timestamps by the
    // pre-fix message ingest path. Without this, claude_code/codex
    // sessions stranded in user DBs never make it to the cloud.
    let healed_timestamps = backfill_session_timestamps_from_messages(conn)?;
    if healed_timestamps > 0 {
        tracing::info!(
            rows = healed_timestamps,
            "Backfilled started_at/ended_at on sessions from messages (#569)"
        );
    }

    let added_indexes = missing_reconcile_indexes(conn)?;

    create_indexes(conn)?;

    if !added_columns.is_empty() || !added_indexes.is_empty() || !removed_tables.is_empty() {
        tracing::info!("Schema reconcile completed");
    }
    Ok(SchemaReconcileReport {
        added_columns,
        added_indexes,
        removed_tables,
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
        assert!(table_exists(conn, "tail_offsets").unwrap());
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
    fn check_on_clean_db_reports_no_drift() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        let report = check(&conn).unwrap();
        assert_eq!(report.from_version, SCHEMA_VERSION);
        assert_eq!(report.to_version, SCHEMA_VERSION);
        assert!(!report.migrated);
        assert!(report.added_columns.is_empty());
        assert!(report.added_indexes.is_empty());
        assert!(report.removed_tables.is_empty());
    }

    #[test]
    fn check_detects_drift_without_modifying_db() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute_batch("DROP TABLE tail_offsets;").unwrap();
        assert!(!table_exists(&conn, "tail_offsets").unwrap());

        let report = check(&conn).unwrap();
        assert!(!report.migrated);
        assert!(
            report.added_columns.iter().any(|c| c == "tail_offsets"),
            "check should report the missing table; got {:?}",
            report.added_columns
        );
        assert!(
            !table_exists(&conn, "tail_offsets").unwrap(),
            "check must not modify the database"
        );
    }

    #[test]
    fn check_reports_migration_needed_for_v0_db() {
        let conn = Connection::open_in_memory().unwrap();
        assert_eq!(current_version(&conn), 0);

        let report = check(&conn).unwrap();
        assert_eq!(report.from_version, 0);
        assert_eq!(report.to_version, SCHEMA_VERSION);
        assert!(report.migrated);
        // When migration is needed, drift lists are empty since the
        // migration would recreate everything from scratch.
        assert!(report.added_columns.is_empty());
        assert!(report.added_indexes.is_empty());
        assert!(report.removed_tables.is_empty());
        assert_eq!(
            current_version(&conn),
            0,
            "check must not modify the database"
        );
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
        assert!(first.removed_tables.is_empty());

        let second = repair(&conn).unwrap();
        assert_eq!(second.from_version, SCHEMA_VERSION);
        assert!(!second.migrated);
        assert!(second.added_columns.is_empty());
        assert!(second.added_indexes.is_empty());
        assert!(second.removed_tables.is_empty());
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

    /// #701 — an existing v1 DB that pre-dates the `surface` dimension
    /// must gain the column on both `messages` and `sessions` through
    /// `reconcile_schema`, populate it via the parser-rule backfill, and
    /// rebuild rollups with the new PK shape. The fixture seeds rows
    /// from each provider so the backfill matrix is exercised end-to-end.
    #[test]
    fn reconcile_adds_surface_and_backfills_existing_v1_db() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        // Simulate a pre-#701 database: drop the rollup triggers (they
        // reference `NEW.surface` after the migration) and the surface
        // column on both tables. The reconcile path under test must
        // recreate the triggers, the column, and the rollup PK shape.
        conn.execute_batch(
            "DROP TRIGGER IF EXISTS trg_messages_rollup_insert;
             DROP TRIGGER IF EXISTS trg_messages_rollup_delete;
             DROP TRIGGER IF EXISTS trg_messages_rollup_update;
             ALTER TABLE messages DROP COLUMN surface;
             ALTER TABLE sessions DROP COLUMN surface;",
        )
        .unwrap();
        assert!(!has_column(&conn, "messages", "surface").unwrap());
        assert!(!has_column(&conn, "sessions", "surface").unwrap());

        // Seed one row per inference rule. `cwd` drives the
        // copilot_chat path-based inference; the others are
        // provider-rule rows.
        conn.execute_batch(
            "INSERT INTO messages (id, role, timestamp, provider, cwd) VALUES
                ('m_cc', 'assistant', '2026-04-01T00:00:00Z', 'claude_code', '/repo/foo'),
                ('m_cur', 'assistant', '2026-04-01T00:00:00Z', 'cursor', '/repo/foo'),
                ('m_cli', 'assistant', '2026-04-01T00:00:00Z', 'copilot_cli', '/repo/foo'),
                ('m_cdx', 'assistant', '2026-04-01T00:00:00Z', 'codex', '/repo/foo'),
                ('m_chat_vscode', 'assistant', '2026-04-01T00:00:00Z', 'copilot_chat',
                 '/Users/x/Library/Application Support/Code/User/workspaceStorage/abc'),
                ('m_chat_cursor', 'assistant', '2026-04-01T00:00:00Z', 'copilot_chat',
                 '/Users/x/Library/Application Support/Cursor/User/workspaceStorage/abc'),
                ('m_chat_remote', 'assistant', '2026-04-01T00:00:00Z', 'copilot_chat',
                 '/home/x/.vscode-server/data/User/workspaceStorage/abc'),
                ('m_chat_unknown', 'assistant', '2026-04-01T00:00:00Z', 'copilot_chat', NULL);
             INSERT INTO sessions (id, provider) VALUES ('sess_cc', 'claude_code'), ('sess_cur', 'cursor');",
        )
        .unwrap();

        let report = repair(&conn).unwrap();
        assert!(
            report.added_columns.iter().any(|c| c == "messages.surface"),
            "report should mention added messages.surface column; got {:?}",
            report.added_columns
        );
        assert!(
            report.added_columns.iter().any(|c| c == "sessions.surface"),
            "report should mention added sessions.surface column; got {:?}",
            report.added_columns
        );

        let surface_for = |id: &str| -> String {
            conn.query_row("SELECT surface FROM messages WHERE id = ?1", [id], |r| {
                r.get::<_, String>(0)
            })
            .unwrap()
        };
        assert_eq!(surface_for("m_cc"), "terminal");
        assert_eq!(surface_for("m_cur"), "cursor");
        assert_eq!(surface_for("m_cli"), "terminal");
        assert_eq!(surface_for("m_cdx"), "terminal");
        assert_eq!(surface_for("m_chat_vscode"), "vscode");
        assert_eq!(surface_for("m_chat_cursor"), "cursor");
        assert_eq!(surface_for("m_chat_remote"), "vscode");
        assert_eq!(surface_for("m_chat_unknown"), "unknown");

        // Sessions rule-fall back when no messages link to them. No
        // message references `sess_cc` or `sess_cur`, so the provider
        // rule kicks in.
        let sess_cc_surface: String = conn
            .query_row(
                "SELECT surface FROM sessions WHERE id = 'sess_cc'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let sess_cur_surface: String = conn
            .query_row(
                "SELECT surface FROM sessions WHERE id = 'sess_cur'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sess_cc_surface, "terminal");
        assert_eq!(sess_cur_surface, "cursor");

        // Rollups should be rebuilt with surface partitioning.
        let surfaces_in_rollups: Vec<String> = conn
            .prepare("SELECT DISTINCT surface FROM message_rollups_daily ORDER BY surface")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        // We seeded rows with vscode, cursor, terminal, and unknown.
        assert!(
            surfaces_in_rollups.contains(&"vscode".to_string()),
            "daily rollups should partition on surface; got {:?}",
            surfaces_in_rollups
        );
        assert!(
            surfaces_in_rollups.contains(&"cursor".to_string()),
            "daily rollups should partition on surface; got {:?}",
            surfaces_in_rollups
        );
        assert!(
            surfaces_in_rollups.contains(&"terminal".to_string()),
            "daily rollups should partition on surface; got {:?}",
            surfaces_in_rollups
        );

        // Idempotency: a second repair should be a no-op.
        let second = repair(&conn).unwrap();
        assert!(
            !second.added_columns.iter().any(|c| c.contains("surface")),
            "second repair must not re-add surface columns; got {:?}",
            second.added_columns
        );
    }

    /// 8.1 → 8.2 upgrade: an existing v1 database that pre-dates the
    /// `tail_offsets` table must gain it through `reconcile_schema` without
    /// triggering a destructive migration. See #319 / ADR-0089.
    #[test]
    fn reconcile_adds_tail_offsets_to_existing_v1_db() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute_batch("DROP TABLE tail_offsets;").unwrap();
        assert!(!table_exists(&conn, "tail_offsets").unwrap());

        let report = repair(&conn).unwrap();

        assert_eq!(report.from_version, SCHEMA_VERSION);
        assert_eq!(report.to_version, SCHEMA_VERSION);
        assert!(!report.migrated, "additive repair should not bump version");
        assert!(
            report.added_columns.iter().any(|c| c == "tail_offsets"),
            "report should mention the new table; got {:?}",
            report.added_columns
        );
        assert!(table_exists(&conn, "tail_offsets").unwrap());
        assert!(report.removed_tables.is_empty());
    }

    /// 8.1 -> 8.2 upgrade: keep proxy-sourced `messages` rows but remove the
    /// orphaned `proxy_events` table now that the proxy runtime is gone.
    #[test]
    fn reconcile_drops_legacy_proxy_events_table_from_existing_v1_db() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute_batch(
            "
            CREATE TABLE proxy_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp TEXT NOT NULL
            );
            ",
        )
        .unwrap();
        assert!(table_exists(&conn, "proxy_events").unwrap());

        let report = repair(&conn).unwrap();

        assert_eq!(report.from_version, SCHEMA_VERSION);
        assert_eq!(report.to_version, SCHEMA_VERSION);
        assert!(!report.migrated, "cleanup should not bump schema version");
        assert!(
            report.removed_tables.iter().any(|t| t == "proxy_events"),
            "report should mention the removed table; got {:?}",
            report.removed_tables
        );
        assert!(!table_exists(&conn, "proxy_events").unwrap());
    }

    /// #442: an existing v1 DB may carry bare-folder-name `repo_id`
    /// values from pre-8.3 pipeline runs. `reconcile_schema` must rewrite
    /// every non-URL value to NULL while leaving real remote URLs
    /// untouched, and re-running must be a no-op.
    #[test]
    fn reconcile_scrubs_bare_folder_repo_ids_to_null() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        // Seed the messages table with one real URL and several pre-8.3
        // bare-folder-name rows drawn from the #442 repro table.
        conn.execute_batch(
            "INSERT INTO messages (id, role, timestamp, repo_id, cwd, provider)
             VALUES
                 ('m1', 'assistant', '2026-04-20T00:00:00Z', 'github.com/siropkin/budi', '/u/x/budi', 'claude_code'),
                 ('m2', 'assistant', '2026-04-20T00:00:00Z', 'Desktop',                    '/u/x/Desktop', 'claude_code'),
                 ('m3', 'assistant', '2026-04-20T00:00:00Z', 'ivan.seredkin',              '/u/x', 'claude_code'),
                 ('m4', 'assistant', '2026-04-20T00:00:00Z', '.cursor',                    '/u/x/.cursor', 'claude_code'),
                 ('m5', 'assistant', '2026-04-20T00:00:00Z', 'homebrew-budi',              '/u/x/h', 'claude_code'),
                 ('m6', 'assistant', '2026-04-20T00:00:00Z', 'gitlab.com/acme/web',        '/u/x/web', 'claude_code');",
        )
        .unwrap();

        let report = repair(&conn).unwrap();
        assert_eq!(report.from_version, SCHEMA_VERSION);
        assert_eq!(report.to_version, SCHEMA_VERSION);

        let real_url_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE repo_id IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            real_url_count, 2,
            "only the two github/gitlab rows should keep their repo_id"
        );

        let nulled_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE repo_id IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            nulled_count, 4,
            "Desktop / ivan.seredkin / .cursor / homebrew-budi must collapse to NULL"
        );

        // Second run: predicate is empty, so nothing changes.
        let before: Vec<(String, Option<String>)> = conn
            .prepare("SELECT id, repo_id FROM messages ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        let _ = repair(&conn).unwrap();
        let after: Vec<(String, Option<String>)> = conn
            .prepare("SELECT id, repo_id FROM messages ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(before, after, "backfill must be idempotent");
    }

    /// #730 / ADR-0094 §1: a pre-dual-cost v1 DB must gain
    /// `cost_cents_ingested` on `messages` and both rollup tables, rename
    /// the legacy `cost_cents` column to `cost_cents_effective`, and
    /// preserve every existing row's stored value verbatim in both new
    /// columns (since `_effective = _ingested` until the team-pricing
    /// worker (#731) ships). The rollup triggers must be recreated with
    /// the dual-column shape so subsequent message inserts populate both.
    #[test]
    fn reconcile_adds_dual_cost_columns_on_existing_v1_db() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        // Simulate a pre-#730 v1 DB by dropping the new columns and
        // creating a legacy `cost_cents` column on `messages` and the
        // two rollup tables. The trigger bodies and several indexes
        // reference the new columns; drop them so the legacy state is
        // internally consistent.
        conn.execute_batch(
            "DROP TRIGGER IF EXISTS trg_messages_rollup_insert;
             DROP TRIGGER IF EXISTS trg_messages_rollup_delete;
             DROP TRIGGER IF EXISTS trg_messages_rollup_update;
             DROP INDEX IF EXISTS idx_messages_ts_cost;
             DROP INDEX IF EXISTS idx_messages_role_ts_cost;
             DROP INDEX IF EXISTS idx_messages_role_branch_cost;
             DROP INDEX IF EXISTS idx_messages_session_role_cost;
             ALTER TABLE messages DROP COLUMN cost_cents_effective;
             ALTER TABLE messages DROP COLUMN cost_cents_ingested;
             ALTER TABLE messages ADD COLUMN cost_cents REAL;
             ALTER TABLE message_rollups_hourly DROP COLUMN cost_cents_effective;
             ALTER TABLE message_rollups_hourly DROP COLUMN cost_cents_ingested;
             ALTER TABLE message_rollups_hourly ADD COLUMN cost_cents REAL NOT NULL DEFAULT 0;
             ALTER TABLE message_rollups_daily DROP COLUMN cost_cents_effective;
             ALTER TABLE message_rollups_daily DROP COLUMN cost_cents_ingested;
             ALTER TABLE message_rollups_daily ADD COLUMN cost_cents REAL NOT NULL DEFAULT 0;
             CREATE INDEX idx_messages_ts_cost ON messages(timestamp, cost_cents);
             CREATE INDEX idx_messages_role_ts_cost ON messages(role, timestamp, cost_cents);
             CREATE INDEX idx_messages_role_branch_cost ON messages(role, git_branch, cost_cents);
             CREATE INDEX idx_messages_session_role_cost ON messages(session_id, role, cost_cents);",
        )
        .unwrap();
        assert!(has_column(&conn, "messages", "cost_cents").unwrap());
        assert!(!has_column(&conn, "messages", "cost_cents_effective").unwrap());
        assert!(!has_column(&conn, "messages", "cost_cents_ingested").unwrap());

        // Seed a row with a known cost so we can prove the value
        // survives the rename + add-column path.
        conn.execute(
            "INSERT INTO messages (id, role, timestamp, model, provider, cost_cents)
             VALUES ('m1', 'assistant', '2026-04-20T00:00:00Z', 'claude-opus-4-6',
                     'claude_code', 12.34)",
            [],
        )
        .unwrap();

        let report = repair(&conn).unwrap();
        assert!(
            report
                .added_columns
                .iter()
                .any(|c| c == "messages.cost_cents_ingested"),
            "report should mention messages.cost_cents_ingested; got {:?}",
            report.added_columns
        );
        assert!(
            report
                .added_columns
                .iter()
                .any(|c| c == "messages.cost_cents_effective"),
            "report should mention messages.cost_cents_effective; got {:?}",
            report.added_columns
        );

        assert!(has_column(&conn, "messages", "cost_cents_effective").unwrap());
        assert!(has_column(&conn, "messages", "cost_cents_ingested").unwrap());
        assert!(!has_column(&conn, "messages", "cost_cents").unwrap());
        assert!(has_column(&conn, "message_rollups_hourly", "cost_cents_effective").unwrap());
        assert!(has_column(&conn, "message_rollups_hourly", "cost_cents_ingested").unwrap());
        assert!(has_column(&conn, "message_rollups_daily", "cost_cents_effective").unwrap());
        assert!(has_column(&conn, "message_rollups_daily", "cost_cents_ingested").unwrap());

        // ADR-0094 §1: `_effective` defaults to `_ingested` until a team
        // price list rewrites it. The migration must copy the legacy
        // value into both columns.
        let (ingested, effective): (f64, f64) = conn
            .query_row(
                "SELECT cost_cents_ingested, cost_cents_effective FROM messages WHERE id = 'm1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(
            (ingested - 12.34).abs() < 1e-9,
            "cost_cents_ingested preserves the legacy stored value; got {ingested}"
        );
        assert!(
            (effective - 12.34).abs() < 1e-9,
            "cost_cents_effective defaults to _ingested until team pricing ships (#731); got {effective}"
        );

        // Rollup triggers must be recreated with the dual-column shape.
        // A fresh INSERT exercises them — both rollup columns must
        // increment on the new row.
        assert!(trigger_exists(&conn, "trg_messages_rollup_insert").unwrap());
        conn.execute(
            "INSERT INTO messages
                (id, role, timestamp, model, provider,
                 input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
                 cost_cents_ingested, cost_cents_effective)
             VALUES ('m2', 'assistant', '2026-04-21T00:00:00Z', 'claude-opus-4-6',
                     'claude_code', 0, 0, 0, 0, 4.5, 4.5)",
            [],
        )
        .unwrap();
        let (rh_i, rh_e): (f64, f64) = conn
            .query_row(
                "SELECT SUM(cost_cents_ingested), SUM(cost_cents_effective)
                 FROM message_rollups_daily WHERE bucket_day = '2026-04-21'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!((rh_i - 4.5).abs() < 1e-9);
        assert!((rh_e - 4.5).abs() < 1e-9);

        // Idempotency: a second repair must not re-add the columns or
        // rewrite the rollup contents.
        let second = repair(&conn).unwrap();
        assert!(
            !second
                .added_columns
                .iter()
                .any(|c| c.contains("cost_cents_")),
            "second repair must not re-add dual cost columns; got {:?}",
            second.added_columns
        );
    }

    /// #730: `db check` must surface a missing `cost_cents_ingested` /
    /// `cost_cents_effective` column on an otherwise-current v1 DB as
    /// drift so `budi db check --fix` heals it without bumping the
    /// schema version.
    #[test]
    fn check_detects_missing_dual_cost_columns() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        // Drop only `cost_cents_ingested` to simulate an incomplete
        // partial migration. The triggers reference it, so drop them
        // too to keep the database internally consistent.
        conn.execute_batch(
            "DROP TRIGGER IF EXISTS trg_messages_rollup_insert;
             DROP TRIGGER IF EXISTS trg_messages_rollup_delete;
             DROP TRIGGER IF EXISTS trg_messages_rollup_update;
             ALTER TABLE message_rollups_hourly DROP COLUMN cost_cents_ingested;
             ALTER TABLE message_rollups_daily DROP COLUMN cost_cents_ingested;
             ALTER TABLE messages DROP COLUMN cost_cents_ingested;",
        )
        .unwrap();

        let report = check(&conn).unwrap();
        assert!(
            !report.migrated,
            "additive drift must not request a destructive migration"
        );
        assert!(
            report
                .added_columns
                .iter()
                .any(|c| c == "messages.cost_cents_ingested"),
            "check should report missing messages.cost_cents_ingested; got {:?}",
            report.added_columns
        );
        assert!(
            has_column(&conn, "messages", "cost_cents_effective").unwrap(),
            "check must not modify the database"
        );
    }
}
