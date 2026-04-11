//! Proxy event types and analytics storage.
//!
//! Each proxied request produces a `ProxyEvent` record that is appended to the
//! `proxy_events` table in the analytics database. This is append-only and
//! compatible with the existing analytics pipeline.

use anyhow::Result;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

/// Provider determined by path-based routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxyProvider {
    Anthropic,
    OpenAi,
}

impl ProxyProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
        }
    }
}

impl std::fmt::Display for ProxyProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A single proxy event record captured from a proxied request/response cycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyEvent {
    pub timestamp: String,
    pub provider: String,
    pub model: String,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub duration_ms: i64,
    pub status_code: u16,
    pub is_streaming: bool,
}

/// Ensure the `proxy_events` table exists in the analytics database.
pub fn ensure_proxy_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS proxy_events (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp     TEXT NOT NULL,
            provider      TEXT NOT NULL,
            model         TEXT NOT NULL DEFAULT '',
            input_tokens  INTEGER,
            output_tokens INTEGER,
            duration_ms   INTEGER NOT NULL DEFAULT 0,
            status_code   INTEGER NOT NULL DEFAULT 0,
            is_streaming  INTEGER NOT NULL DEFAULT 0,
            created_at    TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE INDEX IF NOT EXISTS idx_proxy_events_timestamp
            ON proxy_events(timestamp);
        CREATE INDEX IF NOT EXISTS idx_proxy_events_provider
            ON proxy_events(provider);",
    )?;
    Ok(())
}

/// Insert a proxy event into the analytics database.
pub fn insert_proxy_event(conn: &Connection, event: &ProxyEvent) -> Result<i64> {
    conn.execute(
        "INSERT INTO proxy_events (
            timestamp, provider, model, input_tokens, output_tokens,
            duration_ms, status_code, is_streaming
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            event.timestamp,
            event.provider,
            event.model,
            event.input_tokens,
            event.output_tokens,
            event.duration_ms,
            event.status_code as i64,
            event.is_streaming as i64,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .unwrap();
        ensure_proxy_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn proxy_event_round_trip() {
        let conn = test_db();
        let event = ProxyEvent {
            timestamp: "2026-04-10T12:00:00Z".to_string(),
            provider: "openai".to_string(),
            model: "gpt-4o".to_string(),
            input_tokens: Some(100),
            output_tokens: Some(50),
            duration_ms: 1200,
            status_code: 200,
            is_streaming: false,
        };
        let id = insert_proxy_event(&conn, &event).unwrap();
        assert!(id > 0);

        let (provider, model, status): (String, String, i64) = conn
            .query_row(
                "SELECT provider, model, status_code FROM proxy_events WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(provider, "openai");
        assert_eq!(model, "gpt-4o");
        assert_eq!(status, 200);
    }

    #[test]
    fn proxy_event_with_null_tokens() {
        let conn = test_db();
        let event = ProxyEvent {
            timestamp: "2026-04-10T12:00:00Z".to_string(),
            provider: "anthropic".to_string(),
            model: "claude-sonnet-4-6".to_string(),
            input_tokens: None,
            output_tokens: None,
            duration_ms: 500,
            status_code: 200,
            is_streaming: true,
        };
        let id = insert_proxy_event(&conn, &event).unwrap();
        assert!(id > 0);
    }

    #[test]
    fn ensure_schema_is_idempotent() {
        let conn = test_db();
        ensure_proxy_schema(&conn).unwrap();
        ensure_proxy_schema(&conn).unwrap();
    }

    #[test]
    fn proxy_provider_display() {
        assert_eq!(ProxyProvider::Anthropic.as_str(), "anthropic");
        assert_eq!(ProxyProvider::OpenAi.as_str(), "openai");
    }
}
