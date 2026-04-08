//! Durable ingest queue for realtime hook and OTEL payloads.
//!
//! Realtime endpoints append raw payloads here first (durable-first), then a
//! background worker drains the queue into the analytics database in bounded
//! batches with retry/backoff.

use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;

use anyhow::{Context, Result};
use chrono::{Duration, Utc};
use rusqlite::{Connection, params};
use serde_json::Value;

const QUEUE_DB_FILE: &str = "ingest-queue.db";
const MAX_ATTEMPTS: i64 = 5;
const MAX_ERROR_CHARS: usize = 600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestSource {
    Hook,
    Otel,
}

impl IngestSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Hook => "hook",
            Self::Otel => "otel",
        }
    }

    fn from_str(value: &str) -> Option<Self> {
        match value {
            "hook" => Some(Self::Hook),
            "otel" => Some(Self::Otel),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct QueueStats {
    /// Pending rows (includes rows waiting for retry time).
    pub pending: u64,
    /// Rows eligible to process right now (`available_at <= now`).
    pub ready: u64,
    /// Rows that exhausted retry attempts and are now dropped/dead-lettered.
    pub failed: u64,
}

#[derive(Debug, Clone, Default)]
pub struct DrainReport {
    pub processed: u64,
    pub retried: u64,
    pub failed: u64,
    pub remaining: u64,
}

#[derive(Debug, Clone)]
struct QueueRow {
    id: i64,
    source: String,
    payload_json: String,
    attempts: i64,
}

/// Resolve the queue database path (`<budi-home>/ingest-queue.db`).
pub fn queue_db_path() -> Result<PathBuf> {
    Ok(crate::config::budi_home_dir()?.join(QUEUE_DB_FILE))
}

/// Ensure queue database exists and schema is created.
pub fn initialize_queue_db() -> Result<()> {
    let path = queue_db_path()?;
    let conn = open_queue_db(&path)?;
    ensure_schema(&conn)
}

pub fn enqueue_hook_payload(payload: &Value) -> Result<i64> {
    enqueue_payload(IngestSource::Hook, payload)
}

pub fn enqueue_otel_payload(payload: &Value) -> Result<i64> {
    enqueue_payload(IngestSource::Otel, payload)
}

pub fn enqueue_payload(source: IngestSource, payload: &Value) -> Result<i64> {
    let queue_path = queue_db_path()?;
    enqueue_payload_at(&queue_path, source, payload)
}

pub fn queue_stats() -> Result<QueueStats> {
    let queue_path = queue_db_path()?;
    queue_stats_at(&queue_path)
}

/// Process one queue batch and return processing counters.
pub fn process_pending_batch(batch_size: usize) -> Result<DrainReport> {
    let queue_path = queue_db_path()?;
    let analytics_path = crate::analytics::db_path()?;
    process_pending_batch_at(&queue_path, &analytics_path, batch_size, 250)
}

/// Process up to `max_batches` batches. Stops early when queue becomes idle.
pub fn process_until_idle(max_batches: usize, batch_size: usize) -> Result<DrainReport> {
    let queue_path = queue_db_path()?;
    let analytics_path = crate::analytics::db_path()?;

    let mut total = DrainReport::default();
    for _ in 0..max_batches {
        let batch = process_pending_batch_at(&queue_path, &analytics_path, batch_size, 250)?;
        total.processed += batch.processed;
        total.retried += batch.retried;
        total.failed += batch.failed;
        total.remaining = batch.remaining;

        if batch.processed == 0 && batch.retried == 0 && batch.failed == 0 {
            break;
        }
        if batch.remaining == 0 {
            break;
        }
    }

    Ok(total)
}

fn enqueue_payload_at(queue_db_path: &Path, source: IngestSource, payload: &Value) -> Result<i64> {
    let conn = open_queue_db(queue_db_path)?;
    ensure_schema(&conn)?;

    let now = Utc::now().to_rfc3339();
    let payload_json =
        serde_json::to_string(payload).context("Failed to serialize queue payload")?;
    conn.execute(
        "INSERT INTO ingest_queue (
            source, payload_json, received_at, available_at, attempts
        ) VALUES (?1, ?2, ?3, ?4, 0)",
        params![source.as_str(), payload_json, now, now],
    )?;
    Ok(conn.last_insert_rowid())
}

fn queue_stats_at(queue_db_path: &Path) -> Result<QueueStats> {
    let conn = open_queue_db(queue_db_path)?;
    ensure_schema(&conn)?;
    queue_stats_from_conn(&conn)
}

fn process_pending_batch_at(
    queue_db_path: &Path,
    analytics_db_path: &Path,
    batch_size: usize,
    analytics_busy_timeout_ms: u64,
) -> Result<DrainReport> {
    let conn = open_queue_db(queue_db_path)?;
    ensure_schema(&conn)?;

    let rows = load_ready_rows(&conn, batch_size)?;
    if rows.is_empty() {
        let stats = queue_stats_from_conn(&conn)?;
        return Ok(DrainReport {
            remaining: stats.pending,
            ..DrainReport::default()
        });
    }

    let mut report = DrainReport::default();
    for row in rows {
        match process_row(analytics_db_path, analytics_busy_timeout_ms, &row) {
            Ok(()) => {
                mark_processed(&conn, row.id)?;
                report.processed += 1;
            }
            Err(err) => {
                let attempts = row.attempts + 1;
                let error_text = truncate_error(&err);
                if attempts >= MAX_ATTEMPTS {
                    mark_failed(&conn, row.id, attempts, &error_text)?;
                    report.failed += 1;
                } else {
                    mark_retry(&conn, row.id, attempts, &error_text)?;
                    report.retried += 1;
                }
            }
        }
    }

    let stats = queue_stats_from_conn(&conn)?;
    report.remaining = stats.pending;
    Ok(report)
}

fn process_row(
    analytics_db_path: &Path,
    analytics_busy_timeout_ms: u64,
    row: &QueueRow,
) -> Result<()> {
    let source = IngestSource::from_str(row.source.as_str())
        .ok_or_else(|| anyhow::anyhow!("Unknown ingest queue source: {}", row.source))?;
    let payload: Value = serde_json::from_str(&row.payload_json)
        .with_context(|| format!("Failed to parse queued payload id={}", row.id))?;

    let mut analytics_conn = crate::analytics::open_db(analytics_db_path)?;
    analytics_conn.busy_timeout(StdDuration::from_millis(analytics_busy_timeout_ms))?;

    match source {
        IngestSource::Hook => {
            crate::hooks::ingest_hook_payload(&mut analytics_conn, &payload)?;
        }
        IngestSource::Otel => {
            let _ = crate::otel::ingest_otel_payload(&mut analytics_conn, &payload)?;
        }
    }

    if let Err(e) = crate::privacy::enforce_retention(&analytics_conn) {
        tracing::warn!(
            "Privacy retention cleanup failed after queued {} ingest: {e}",
            source.as_str()
        );
    }

    Ok(())
}

fn load_ready_rows(conn: &Connection, batch_size: usize) -> Result<Vec<QueueRow>> {
    let now = Utc::now().to_rfc3339();
    let mut stmt = conn.prepare(
        "SELECT id, source, payload_json, attempts
         FROM ingest_queue
         WHERE processed_at IS NULL
           AND failed_at IS NULL
           AND available_at <= ?1
         ORDER BY id ASC
         LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(params![now, batch_size as i64], |row| {
            Ok(QueueRow {
                id: row.get(0)?,
                source: row.get(1)?,
                payload_json: row.get(2)?,
                attempts: row.get(3)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn mark_processed(conn: &Connection, id: i64) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE ingest_queue
         SET processed_at = ?2, last_error = NULL
         WHERE id = ?1",
        params![id, now],
    )?;
    Ok(())
}

fn mark_retry(conn: &Connection, id: i64, attempts: i64, error_text: &str) -> Result<()> {
    let retry_at = (Utc::now() + retry_backoff(attempts)).to_rfc3339();
    conn.execute(
        "UPDATE ingest_queue
         SET attempts = ?2, available_at = ?3, last_error = ?4
         WHERE id = ?1",
        params![id, attempts, retry_at, error_text],
    )?;
    Ok(())
}

fn mark_failed(conn: &Connection, id: i64, attempts: i64, error_text: &str) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE ingest_queue
         SET attempts = ?2, failed_at = ?3, last_error = ?4
         WHERE id = ?1",
        params![id, attempts, now, error_text],
    )?;
    Ok(())
}

fn retry_backoff(attempts: i64) -> Duration {
    match attempts {
        1 => Duration::seconds(1),
        2 => Duration::seconds(2),
        3 => Duration::seconds(5),
        4 => Duration::seconds(10),
        _ => Duration::seconds(30),
    }
}

fn truncate_error(err: &anyhow::Error) -> String {
    let text = format!("{err:#}");
    if text.chars().count() <= MAX_ERROR_CHARS {
        return text;
    }
    let mut out = String::with_capacity(MAX_ERROR_CHARS + 3);
    for ch in text.chars().take(MAX_ERROR_CHARS) {
        out.push(ch);
    }
    out.push_str("...");
    out
}

fn queue_stats_from_conn(conn: &Connection) -> Result<QueueStats> {
    let now = Utc::now().to_rfc3339();
    let pending: i64 = conn.query_row(
        "SELECT COUNT(*) FROM ingest_queue
         WHERE processed_at IS NULL AND failed_at IS NULL",
        [],
        |row| row.get(0),
    )?;
    let ready: i64 = conn.query_row(
        "SELECT COUNT(*) FROM ingest_queue
         WHERE processed_at IS NULL
           AND failed_at IS NULL
           AND available_at <= ?1",
        params![now],
        |row| row.get(0),
    )?;
    let failed: i64 = conn.query_row(
        "SELECT COUNT(*) FROM ingest_queue
         WHERE failed_at IS NOT NULL",
        [],
        |row| row.get(0),
    )?;
    Ok(QueueStats {
        pending: pending.max(0) as u64,
        ready: ready.max(0) as u64,
        failed: failed.max(0) as u64,
    })
}

fn open_queue_db(db_path: &Path) -> Result<Connection> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create dir {}", parent.display()))?;
    }

    let conn = Connection::open(db_path)?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA foreign_keys=ON;
         PRAGMA synchronous=NORMAL;
         PRAGMA temp_store=MEMORY;
         PRAGMA busy_timeout=5000;",
    )?;
    Ok(conn)
}

fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS ingest_queue (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            source       TEXT NOT NULL,
            payload_json TEXT NOT NULL,
            received_at  TEXT NOT NULL,
            available_at TEXT NOT NULL,
            attempts     INTEGER NOT NULL DEFAULT 0,
            last_error   TEXT,
            processed_at TEXT,
            failed_at    TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_ingest_queue_pending
            ON ingest_queue(available_at, id)
            WHERE processed_at IS NULL AND failed_at IS NULL;
        CREATE INDEX IF NOT EXISTS idx_ingest_queue_failed
            ON ingest_queue(failed_at)
            WHERE failed_at IS NOT NULL;
        ",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(1);

    fn temp_test_dir(suffix: &str) -> std::path::PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = Utc::now().timestamp_nanos_opt().unwrap_or_default();
        let dir = std::env::temp_dir().join(format!(
            "budi-ingest-queue-{suffix}-{}-{nanos}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn setup_analytics_db(path: &Path) {
        let _conn = crate::analytics::open_db_with_migration(path).unwrap();
    }

    fn hook_payload() -> Value {
        serde_json::json!({
            "hook_event_name": "PostToolUse",
            "session_id": "sess-queue-hook",
            "tool_name": "Read",
            "duration": 12,
            "cwd": "/tmp/repo"
        })
    }

    fn otel_payload() -> Value {
        serde_json::json!({
            "resourceLogs": [{
                "resource": {
                    "attributes": [
                        {"key": "session.id", "value": {"stringValue": "sess-queue-otel"}}
                    ]
                },
                "scopeLogs": [{
                    "logRecords": [{
                        "timeUnixNano": "1711500000000000000",
                        "body": {"stringValue": "claude_code.api_request"},
                        "attributes": [
                            {"key": "model", "value": {"stringValue": "claude-sonnet-4-6"}},
                            {"key": "cost_usd", "value": {"doubleValue": 0.01}},
                            {"key": "input_tokens", "value": {"intValue": "100"}},
                            {"key": "output_tokens", "value": {"intValue": "20"}},
                            {"key": "cache_read_tokens", "value": {"intValue": "0"}},
                            {"key": "cache_creation_tokens", "value": {"intValue": "0"}}
                        ]
                    }]
                }]
            }]
        })
    }

    #[test]
    fn processes_hook_and_otel_queue_items() {
        let dir = temp_test_dir("process");
        let queue_db = dir.join("ingest-queue.db");
        let analytics_db = dir.join("analytics.db");
        setup_analytics_db(&analytics_db);

        enqueue_payload_at(&queue_db, IngestSource::Hook, &hook_payload()).unwrap();
        enqueue_payload_at(&queue_db, IngestSource::Otel, &otel_payload()).unwrap();

        let report = process_pending_batch_at(&queue_db, &analytics_db, 50, 50).unwrap();
        assert_eq!(report.processed, 2);
        assert_eq!(report.failed, 0);
        assert_eq!(report.retried, 0);

        let analytics_conn = crate::analytics::open_db(&analytics_db).unwrap();
        let hook_events: i64 = analytics_conn
            .query_row("SELECT COUNT(*) FROM hook_events", [], |row| row.get(0))
            .unwrap();
        let otel_events: i64 = analytics_conn
            .query_row("SELECT COUNT(*) FROM otel_events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(hook_events, 1);
        assert_eq!(otel_events, 1);

        let stats = queue_stats_at(&queue_db).unwrap();
        assert_eq!(stats.pending, 0);
        assert_eq!(stats.failed, 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retries_when_analytics_database_is_locked() {
        let dir = temp_test_dir("retry");
        let queue_db = dir.join("ingest-queue.db");
        let analytics_db = dir.join("analytics.db");
        setup_analytics_db(&analytics_db);

        enqueue_payload_at(&queue_db, IngestSource::Hook, &hook_payload()).unwrap();

        let mut lock_conn = crate::analytics::open_db(&analytics_db).unwrap();
        let tx = lock_conn.transaction().unwrap();
        tx.execute(
            "INSERT OR IGNORE INTO sessions (id, provider) VALUES ('lock-session', 'claude_code')",
            [],
        )
        .unwrap();

        let first = process_pending_batch_at(&queue_db, &analytics_db, 10, 1).unwrap();
        assert_eq!(first.processed, 0);
        assert_eq!(first.retried, 1);

        drop(tx);
        let queue_conn = open_queue_db(&queue_db).unwrap();
        queue_conn
            .execute(
                "UPDATE ingest_queue SET available_at = ?1 WHERE processed_at IS NULL AND failed_at IS NULL",
                params![Utc::now().to_rfc3339()],
            )
            .unwrap();

        let second = process_pending_batch_at(&queue_db, &analytics_db, 10, 50).unwrap();
        assert_eq!(second.processed, 1);
        assert_eq!(second.failed, 0);

        let stats = queue_stats_at(&queue_db).unwrap();
        assert_eq!(stats.pending, 0);
        assert_eq!(stats.failed, 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_payload_eventually_moves_to_failed_bucket() {
        let dir = temp_test_dir("failed");
        let queue_db = dir.join("ingest-queue.db");
        let analytics_db = dir.join("analytics.db");
        setup_analytics_db(&analytics_db);

        let queue_conn = open_queue_db(&queue_db).unwrap();
        ensure_schema(&queue_conn).unwrap();
        queue_conn
            .execute(
                "INSERT INTO ingest_queue (source, payload_json, received_at, available_at, attempts)
                 VALUES ('hook', '{bad-json', ?1, ?1, 0)",
                params![Utc::now().to_rfc3339()],
            )
            .unwrap();

        for _ in 0..MAX_ATTEMPTS {
            let _ = process_pending_batch_at(&queue_db, &analytics_db, 1, 50).unwrap();
            let conn = open_queue_db(&queue_db).unwrap();
            conn.execute(
                "UPDATE ingest_queue
                 SET available_at = ?1
                 WHERE processed_at IS NULL AND failed_at IS NULL",
                params![Utc::now().to_rfc3339()],
            )
            .unwrap();
        }

        let stats = queue_stats_at(&queue_db).unwrap();
        assert_eq!(stats.pending, 0);
        assert_eq!(stats.failed, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
