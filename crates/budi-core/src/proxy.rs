//! Proxy event types and analytics storage.
//!
//! Each proxied request produces a `ProxyEvent` record that is appended to the
//! `proxy_events` table in the analytics database. Attribution fields (repo,
//! branch, ticket, cost) make proxy traffic visible in existing analytics
//! surfaces via a unified insert into the `messages` table.

use anyhow::Result;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::pipeline::extract_ticket_id;

/// The fallback repo_id when attribution cannot be determined.
pub const UNASSIGNED_REPO: &str = "Unassigned";

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
            Self::Anthropic => "claude_code",
            Self::OpenAi => "openai",
        }
    }
}

impl std::fmt::Display for ProxyProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Attribution context resolved from request headers or git state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProxyAttribution {
    pub repo_id: String,
    pub git_branch: String,
    pub ticket_id: String,
}

impl ProxyAttribution {
    /// Build attribution from explicit values and/or cwd-based git resolution.
    ///
    /// Priority: explicit header values > git-resolved values > Unassigned fallback.
    pub fn resolve(repo: Option<&str>, branch: Option<&str>, cwd: Option<&str>) -> Self {
        let (resolved_repo, resolved_branch) = match cwd {
            Some(dir) => {
                let path = std::path::Path::new(dir);
                let r = repo
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .unwrap_or_else(|| crate::repo_id::resolve_repo_id(path));
                let b = branch
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .unwrap_or_else(|| resolve_git_branch(path));
                (r, b)
            }
            None => (
                repo.filter(|s| !s.is_empty())
                    .unwrap_or(UNASSIGNED_REPO)
                    .to_string(),
                branch
                    .filter(|s| !s.is_empty())
                    .unwrap_or_default()
                    .to_string(),
            ),
        };

        let repo_id = if resolved_repo.is_empty() {
            UNASSIGNED_REPO.to_string()
        } else {
            resolved_repo
        };

        let ticket_id = extract_ticket_id(&resolved_branch)
            .or_else(|| extract_numeric_ticket(&resolved_branch))
            .unwrap_or_else(|| "Unassigned".to_string());
        // ADR-0082 §9: main/master/develop are integration branches, not tickets
        let ticket_id = if matches!(
            resolved_branch.as_str(),
            "main" | "master" | "develop" | "HEAD" | ""
        ) {
            "Unassigned".to_string()
        } else {
            ticket_id
        };

        Self {
            repo_id,
            git_branch: resolved_branch,
            ticket_id,
        }
    }
}

/// Resolve the current git branch for a directory.
fn resolve_git_branch(cwd: &std::path::Path) -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// Extract a numeric-only ticket ID from a branch name per ADR-0082 §9.
/// Matches the first segment after `/` or at the start that is purely numeric
/// followed by `-` or end-of-string. E.g., `fix/1234-typo` → `"1234"`.
fn extract_numeric_ticket(branch: &str) -> Option<String> {
    // Take the segment after the last `/`, or the whole branch
    let segment = branch.rsplit('/').next().unwrap_or(branch);
    let bytes = segment.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_digit() {
        return None;
    }
    let end = bytes
        .iter()
        .position(|&b| !b.is_ascii_digit())
        .unwrap_or(bytes.len());
    if end == 0 {
        return None;
    }
    // Must be followed by '-' or end-of-string to be a ticket, not just any number
    if end < bytes.len() && bytes[end] != b'-' {
        return None;
    }
    Some(segment[..end].to_string())
}

/// Generate a best-effort session ID when the agent does not provide one.
/// Uses a UUID v4 prefixed with `proxy-session-` for easy identification.
pub fn generate_proxy_session_id() -> String {
    format!("proxy-session-{}", uuid::Uuid::new_v4())
}

/// Compute cost in cents for a proxy event using the provider pricing tables.
pub fn compute_proxy_cost_cents(
    provider: ProxyProvider,
    model: &str,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
) -> f64 {
    let pricing = crate::provider::pricing_for_model(model, provider.as_str());
    pricing.calculate_cost_cents(
        input_tokens.unwrap_or(0).max(0) as u64,
        output_tokens.unwrap_or(0).max(0) as u64,
        0,
        0,
        0,
        None,
        0,
    )
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
    #[serde(default)]
    pub repo_id: String,
    #[serde(default)]
    pub git_branch: String,
    #[serde(default)]
    pub ticket_id: String,
    #[serde(default)]
    pub cost_cents: f64,
    /// Session correlation ID. If the agent provides one via header, use it;
    /// otherwise generate a best-effort ID per ADR-0082 §8.
    #[serde(default)]
    pub session_id: String,
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
            repo_id       TEXT NOT NULL DEFAULT '',
            git_branch    TEXT NOT NULL DEFAULT '',
            ticket_id     TEXT NOT NULL DEFAULT '',
            cost_cents    REAL NOT NULL DEFAULT 0.0,
            session_id    TEXT NOT NULL DEFAULT '',
            created_at    TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE INDEX IF NOT EXISTS idx_proxy_events_timestamp
            ON proxy_events(timestamp);
        CREATE INDEX IF NOT EXISTS idx_proxy_events_provider
            ON proxy_events(provider);
        CREATE INDEX IF NOT EXISTS idx_proxy_events_repo
            ON proxy_events(repo_id);",
    )?;
    // Additive migration for existing databases missing the new columns.
    for (col, def) in [
        ("repo_id", "TEXT NOT NULL DEFAULT ''"),
        ("git_branch", "TEXT NOT NULL DEFAULT ''"),
        ("ticket_id", "TEXT NOT NULL DEFAULT ''"),
        ("cost_cents", "REAL NOT NULL DEFAULT 0.0"),
        ("session_id", "TEXT NOT NULL DEFAULT ''"),
    ] {
        let _ = conn.execute_batch(&format!("ALTER TABLE proxy_events ADD COLUMN {col} {def};"));
    }
    let _ = conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_proxy_events_repo ON proxy_events(repo_id);",
    );
    Ok(())
}

/// Insert a proxy event into the analytics database.
pub fn insert_proxy_event(conn: &Connection, event: &ProxyEvent) -> Result<i64> {
    conn.execute(
        "INSERT INTO proxy_events (
            timestamp, provider, model, input_tokens, output_tokens,
            duration_ms, status_code, is_streaming,
            repo_id, git_branch, ticket_id, cost_cents, session_id
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            event.timestamp,
            event.provider,
            event.model,
            event.input_tokens,
            event.output_tokens,
            event.duration_ms,
            event.status_code as i64,
            event.is_streaming as i64,
            event.repo_id,
            event.git_branch,
            event.ticket_id,
            event.cost_cents,
            event.session_id,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Insert a proxy event into the unified `messages` table so existing analytics
/// surfaces (dashboard, CLI, statusline) can query it without modification.
///
/// Returns the generated message UUID on success.
pub fn insert_proxy_message(conn: &Connection, event: &ProxyEvent) -> Result<String> {
    let uuid = format!("proxy-{}", uuid::Uuid::new_v4());
    let repo_id = if event.repo_id.is_empty() {
        None
    } else {
        Some(&event.repo_id)
    };
    let git_branch = if event.git_branch.is_empty() {
        None
    } else {
        Some(&event.git_branch)
    };

    conn.execute(
        "INSERT OR IGNORE INTO messages (
            id, role, timestamp, model, provider,
            input_tokens, output_tokens,
            cache_creation_tokens, cache_read_tokens,
            repo_id, git_branch, cost_cents, cost_confidence
        ) VALUES (?1, 'assistant', ?2, ?3, ?4, ?5, ?6, 0, 0, ?7, ?8, ?9, 'proxy_estimated')",
        params![
            uuid,
            event.timestamp,
            event.model,
            event.provider,
            event.input_tokens.unwrap_or(0),
            event.output_tokens.unwrap_or(0),
            repo_id,
            git_branch,
            event.cost_cents,
        ],
    )?;

    if !event.ticket_id.is_empty() {
        conn.execute(
            "INSERT OR IGNORE INTO tags (message_id, key, value) VALUES (?1, 'ticket_id', ?2)",
            params![uuid, event.ticket_id],
        )?;
        if let Some(dash) = event.ticket_id.find('-') {
            conn.execute(
                "INSERT OR IGNORE INTO tags (message_id, key, value) VALUES (?1, 'ticket_prefix', ?2)",
                params![uuid, &event.ticket_id[..dash]],
            )?;
        }
    }

    Ok(uuid)
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

    fn test_db_with_messages() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .unwrap();
        crate::migration::migrate(&conn).unwrap();
        ensure_proxy_schema(&conn).unwrap();
        conn
    }

    fn test_event() -> ProxyEvent {
        ProxyEvent {
            timestamp: "2026-04-10T12:00:00Z".to_string(),
            provider: "openai".to_string(),
            model: "gpt-4o".to_string(),
            input_tokens: Some(100),
            output_tokens: Some(50),
            duration_ms: 1200,
            status_code: 200,
            is_streaming: false,
            repo_id: String::new(),
            git_branch: String::new(),
            ticket_id: String::new(),
            cost_cents: 0.0,
            session_id: String::new(),
        }
    }

    #[test]
    fn proxy_event_round_trip() {
        let conn = test_db();
        let event = test_event();
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
    fn proxy_event_with_attribution() {
        let conn = test_db();
        let mut event = test_event();
        event.repo_id = "github.com/siropkin/budi".to_string();
        event.git_branch = "PAVA-2057-fix-auth".to_string();
        event.ticket_id = "PAVA-2057".to_string();
        event.cost_cents = 1.5;

        let id = insert_proxy_event(&conn, &event).unwrap();
        let (repo, branch, ticket, cost): (String, String, String, f64) = conn
            .query_row(
                "SELECT repo_id, git_branch, ticket_id, cost_cents FROM proxy_events WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(repo, "github.com/siropkin/budi");
        assert_eq!(branch, "PAVA-2057-fix-auth");
        assert_eq!(ticket, "PAVA-2057");
        assert!((cost - 1.5).abs() < f64::EPSILON);
    }

    #[test]
    fn proxy_event_with_null_tokens() {
        let conn = test_db();
        let mut event = test_event();
        event.provider = "claude_code".to_string();
        event.model = "claude-sonnet-4-6".to_string();
        event.input_tokens = None;
        event.output_tokens = None;
        event.is_streaming = true;
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
        assert_eq!(ProxyProvider::Anthropic.as_str(), "claude_code");
        assert_eq!(ProxyProvider::OpenAi.as_str(), "openai");
    }

    #[test]
    fn attribution_resolve_with_explicit_values() {
        let attr =
            ProxyAttribution::resolve(Some("github.com/test/repo"), Some("feat/PROJ-42-fix"), None);
        assert_eq!(attr.repo_id, "github.com/test/repo");
        assert_eq!(attr.git_branch, "feat/PROJ-42-fix");
        assert_eq!(attr.ticket_id, "PROJ-42");
    }

    #[test]
    fn attribution_resolve_empty_falls_back_to_unassigned() {
        let attr = ProxyAttribution::resolve(None, None, None);
        assert_eq!(attr.repo_id, UNASSIGNED_REPO);
        assert!(attr.git_branch.is_empty());
        assert_eq!(attr.ticket_id, "Unassigned");
    }

    #[test]
    fn attribution_resolve_empty_repo_with_branch() {
        let attr = ProxyAttribution::resolve(Some(""), Some("ABC-123-feat"), None);
        assert_eq!(attr.repo_id, UNASSIGNED_REPO);
        assert_eq!(attr.git_branch, "ABC-123-feat");
        assert_eq!(attr.ticket_id, "ABC-123");
    }

    #[test]
    fn attribution_resolve_poor_branch_no_ticket() {
        let attr = ProxyAttribution::resolve(Some("my-repo"), Some("main"), None);
        assert_eq!(attr.repo_id, "my-repo");
        assert_eq!(attr.git_branch, "main");
        assert_eq!(attr.ticket_id, "Unassigned");
    }

    #[test]
    fn attribution_resolve_numeric_only_ticket() {
        let attr = ProxyAttribution::resolve(Some("repo"), Some("fix/1234-typo"), None);
        assert_eq!(attr.ticket_id, "1234");
    }

    #[test]
    fn attribution_resolve_develop_branch_unassigned() {
        let attr = ProxyAttribution::resolve(Some("repo"), Some("develop"), None);
        assert_eq!(attr.ticket_id, "Unassigned");
    }

    #[test]
    fn attribution_resolve_master_branch_unassigned() {
        let attr = ProxyAttribution::resolve(Some("repo"), Some("master"), None);
        assert_eq!(attr.ticket_id, "Unassigned");
    }

    #[test]
    fn compute_proxy_cost_anthropic() {
        let cost = compute_proxy_cost_cents(
            ProxyProvider::Anthropic,
            "claude-sonnet-4-6",
            Some(100_000),
            Some(50_000),
        );
        assert!(cost > 0.0, "cost should be positive for non-zero tokens");
    }

    #[test]
    fn compute_proxy_cost_zero_tokens() {
        let cost = compute_proxy_cost_cents(ProxyProvider::OpenAi, "gpt-4o", None, None);
        assert!((cost - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn insert_proxy_message_creates_messages_row() {
        let conn = test_db_with_messages();
        let mut event = test_event();
        event.repo_id = "github.com/test/repo".to_string();
        event.git_branch = "PROJ-42-fix".to_string();
        event.ticket_id = "PROJ-42".to_string();
        event.cost_cents = 2.5;

        let uuid = insert_proxy_message(&conn, &event).unwrap();
        assert!(uuid.starts_with("proxy-"));

        let (role, provider, model, repo, branch, cost, confidence): (
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            f64,
            String,
        ) = conn
            .query_row(
                "SELECT role, provider, model, repo_id, git_branch, cost_cents, cost_confidence
                 FROM messages WHERE id = ?1",
                params![uuid],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(role, "assistant");
        assert_eq!(provider, "openai");
        assert_eq!(model, "gpt-4o");
        assert_eq!(repo.as_deref(), Some("github.com/test/repo"));
        assert_eq!(branch.as_deref(), Some("PROJ-42-fix"));
        assert!((cost - 2.5).abs() < f64::EPSILON);
        assert_eq!(confidence, "proxy_estimated");

        // Ticket tags should be present
        let ticket: String = conn
            .query_row(
                "SELECT value FROM tags WHERE message_id = ?1 AND key = 'ticket_id'",
                params![uuid],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(ticket, "PROJ-42");

        let prefix: String = conn
            .query_row(
                "SELECT value FROM tags WHERE message_id = ?1 AND key = 'ticket_prefix'",
                params![uuid],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(prefix, "PROJ");
    }

    #[test]
    fn insert_proxy_message_no_ticket_skips_tags() {
        let conn = test_db_with_messages();
        let event = test_event();
        let uuid = insert_proxy_message(&conn, &event).unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tags WHERE message_id = ?1",
                params![uuid],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }
}
