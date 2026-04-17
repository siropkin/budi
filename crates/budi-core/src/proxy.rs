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
///
/// Returns an empty string if `cwd` is not a git repo OR the repo is in
/// detached-HEAD state (`git rev-parse --abbrev-ref HEAD` returns the literal
/// `"HEAD"` in that case, which would pollute the branch attribution bucket).
/// Callers treat empty as "no branch" and fall through to `propagate_session_context`
/// or `Unassigned`. See #303 hypothesis #3.
fn resolve_git_branch(cwd: &std::path::Path) -> String {
    let raw = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    // Detached HEAD: git reports the literal "HEAD". Drop it so we never show
    // a bogus `HEAD` branch bucket in `budi stats --branches`.
    if raw == "HEAD" { String::new() } else { raw }
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
    cache_creation_input_tokens: Option<i64>,
    cache_read_input_tokens: Option<i64>,
) -> f64 {
    let pricing = crate::provider::pricing_for_model(model, provider.as_str());
    pricing.calculate_cost_cents(
        input_tokens.unwrap_or(0).max(0) as u64,
        output_tokens.unwrap_or(0).max(0) as u64,
        cache_creation_input_tokens.unwrap_or(0).max(0) as u64,
        cache_read_input_tokens.unwrap_or(0).max(0) as u64,
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
    /// Cache tokens written (Anthropic `cache_creation_input_tokens`).
    pub cache_creation_input_tokens: Option<i64>,
    /// Cache tokens read (Anthropic `cache_read_input_tokens`).
    pub cache_read_input_tokens: Option<i64>,
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
    /// Activity label derived from the last user prompt in the request body
    /// (R1.2, #222). Empty when the body contained no user text or the
    /// prompt did not match any classifier rule. ADR-0083 privacy:
    /// classification runs in-memory — no prompt content is persisted.
    #[serde(default)]
    pub activity: String,
    /// Classifier source, e.g. `"rule"`. Stable label set defined in
    /// `crate::hooks`. Empty when `activity` is empty.
    #[serde(default)]
    pub activity_source: String,
    /// Classifier confidence, one of `"high"`, `"medium"`, `"low"`. Empty
    /// when `activity` is empty.
    #[serde(default)]
    pub activity_confidence: String,
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
        ("cache_creation_input_tokens", "INTEGER"),
        ("cache_read_input_tokens", "INTEGER"),
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
            cache_creation_input_tokens, cache_read_input_tokens,
            duration_ms, status_code, is_streaming,
            repo_id, git_branch, ticket_id, cost_cents, session_id
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            event.timestamp,
            event.provider,
            event.model,
            event.input_tokens,
            event.output_tokens,
            event.cache_creation_input_tokens,
            event.cache_read_input_tokens,
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
///
/// ## Session-level branch propagation (#303)
///
/// The live proxy ingest path does **not** go through `Pipeline::process` (that
/// is used by batch `budi import`), so `propagate_session_context` never ran on
/// proxied rows. When the first turns of a session landed without a branch (the
/// very common case — agents do not set `X-Budi-*` headers, client cwd is not
/// yet known, etc.) every later assistant reply in the same session also got
/// `git_branch = NULL`, and `budi stats --branches` collapsed them into the
/// `(untagged)` bucket.
///
/// This function mirrors the pipeline's per-session carry-forward directly in
/// SQL so live ingest matches batch ingest:
///
/// - If the incoming event has an empty `git_branch` but another message in
///   the same session already has one, inherit it.
/// - If the incoming event has a `git_branch` but earlier same-session rows
///   are empty, backfill them. This covers the "first message lacked context,
///   later one resolved it" race described in the ticket.
/// - Same pattern for `repo_id`.
///
/// The propagation runs inside a single connection scope so a concurrent
/// writer on another session is unaffected.
pub fn insert_proxy_message(conn: &Connection, event: &ProxyEvent) -> Result<String> {
    let uuid = format!("proxy-{}", uuid::Uuid::new_v4());

    // Attribution: session_id must be written alongside other message columns.
    // `session_list_with_filters` filters out rows with NULL/empty `session_id`,
    // so a missing value here would make every proxied message invisible to
    // `budi sessions`. The proxy route always supplies a non-empty id (either
    // from `X-Budi-Session` or `generate_proxy_session_id`) — we still treat an
    // empty string defensively as NULL so queries stay consistent with the
    // documented `messages.session_id` contract (see SOUL.md).
    let session_id = if event.session_id.is_empty() {
        None
    } else {
        Some(event.session_id.as_str())
    };

    // Session-level propagation: if this row lacks branch/repo but a prior row
    // in the same session has one, adopt it before inserting. See fn doc above.
    let (repo_id, git_branch) = resolve_session_attribution(conn, session_id, event);

    let repo_id_param = repo_id.as_deref();
    let git_branch_param = git_branch.as_deref();

    conn.execute(
        "INSERT OR IGNORE INTO messages (
            id, session_id, role, timestamp, model, provider,
            input_tokens, output_tokens,
            cache_creation_tokens, cache_read_tokens,
            repo_id, git_branch, cost_cents, cost_confidence
        ) VALUES (?1, ?2, 'assistant', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 'proxy_estimated')",
        params![
            uuid,
            session_id,
            event.timestamp,
            event.model,
            event.provider,
            event.input_tokens.unwrap_or(0),
            event.output_tokens.unwrap_or(0),
            event.cache_creation_input_tokens.unwrap_or(0),
            event.cache_read_input_tokens.unwrap_or(0),
            repo_id_param,
            git_branch_param,
            event.cost_cents,
        ],
    )?;

    // Backfill earlier same-session rows whose branch/repo was NULL at write
    // time. This catches the exact race in #303: the first few proxy turns of
    // a session land before any attribution is resolved, then a later turn
    // arrives with a cwd-derived branch. Without backfill, the early turns
    // stay in `(untagged)` forever.
    if let Some(sid) = session_id {
        if let Some(ref branch) = git_branch {
            conn.execute(
                "UPDATE messages SET git_branch = ?1
                 WHERE session_id = ?2
                   AND (git_branch IS NULL OR git_branch = '')
                   AND id != ?3",
                params![branch, sid, uuid],
            )?;
        }
        if let Some(ref repo) = repo_id {
            conn.execute(
                "UPDATE messages SET repo_id = ?1
                 WHERE session_id = ?2
                   AND (repo_id IS NULL OR repo_id = '' OR repo_id = 'Unassigned')
                   AND id != ?3",
                params![repo, sid, uuid],
            )?;
        }
    }

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

    // R1.2 (#222): write activity classification tags alongside the row.
    // The classifier runs on the prompt in-memory at the proxy route; no
    // prompt content is stored — we only persist the derived label.
    if !event.activity.is_empty() {
        conn.execute(
            "INSERT OR IGNORE INTO tags (message_id, key, value) VALUES (?1, ?2, ?3)",
            params![uuid, crate::tag_keys::ACTIVITY, event.activity],
        )?;
        if !event.activity_source.is_empty() {
            conn.execute(
                "INSERT OR IGNORE INTO tags (message_id, key, value) VALUES (?1, ?2, ?3)",
                params![
                    uuid,
                    crate::tag_keys::ACTIVITY_SOURCE,
                    event.activity_source
                ],
            )?;
        }
        if !event.activity_confidence.is_empty() {
            conn.execute(
                "INSERT OR IGNORE INTO tags (message_id, key, value) VALUES (?1, ?2, ?3)",
                params![
                    uuid,
                    crate::tag_keys::ACTIVITY_CONFIDENCE,
                    event.activity_confidence
                ],
            )?;
        }
    }

    Ok(uuid)
}

/// Classify the last user prompt in an Anthropic or OpenAI request body.
///
/// The classifier runs in-memory. No prompt content is stored — only the
/// derived `(category, source, confidence)` triple is returned so the
/// proxy route can attach it to the recorded `ProxyEvent`.
///
/// Returns `None` when the body is not valid JSON, has no user message, or
/// the last user message text does not match any rule in the classifier.
/// Large bodies short-circuit at `MAX_CLASSIFY_BYTES` so we never pay to
/// deserialize a full multi-megabyte prompt; truncated prefix still produces
/// the same label in the common case.
pub fn classify_request_body(body: &[u8]) -> Option<(String, String, String)> {
    const MAX_CLASSIFY_BYTES: usize = 128 * 1024;
    let slice = if body.len() > MAX_CLASSIFY_BYTES {
        &body[..MAX_CLASSIFY_BYTES]
    } else {
        body
    };
    let value: serde_json::Value = serde_json::from_slice(slice).ok()?;
    let messages = value.get("messages")?.as_array()?;
    // Walk in reverse to pick the most recent user turn.
    for msg in messages.iter().rev() {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
        if role != "user" {
            continue;
        }
        let text = extract_user_text(msg.get("content")?)?;
        if let Some(c) = crate::hooks::classify_prompt_detailed(&text) {
            return Some((c.category, c.source.to_string(), c.confidence.to_string()));
        }
        // First user message found but unclassifiable — stop, don't fall
        // back to a prior assistant turn.
        return None;
    }
    None
}

fn extract_user_text(content: &serde_json::Value) -> Option<String> {
    match content {
        serde_json::Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        serde_json::Value::Array(blocks) => {
            let text = blocks
                .iter()
                .filter_map(|b| match b {
                    serde_json::Value::String(s) => Some(s.as_str()),
                    serde_json::Value::Object(_) => b.get("text").and_then(|v| v.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ");
            if text.trim().is_empty() {
                None
            } else {
                Some(text)
            }
        }
        _ => None,
    }
}

/// Merge the incoming event's attribution with whatever the rest of the
/// session already knows. Used by `insert_proxy_message` to propagate
/// `git_branch` / `repo_id` forward in a session (#303).
///
/// Returns `(repo_id, git_branch)` where each field is either:
/// - the event's value if non-empty, or
/// - the latest non-empty value observed on any prior message in the same
///   session, or
/// - `None` / `Unassigned` fallback if neither is known yet.
fn resolve_session_attribution(
    conn: &Connection,
    session_id: Option<&str>,
    event: &ProxyEvent,
) -> (Option<String>, Option<String>) {
    let event_repo = if event.repo_id.is_empty() || event.repo_id == UNASSIGNED_REPO {
        None
    } else {
        Some(event.repo_id.clone())
    };
    let event_branch = if event.git_branch.is_empty() {
        None
    } else {
        Some(event.git_branch.clone())
    };

    if event_repo.is_some() && event_branch.is_some() {
        return (event_repo, event_branch);
    }
    let Some(sid) = session_id else {
        return (event_repo, event_branch);
    };

    // Find the most recent message in this session that has the field we are
    // missing. We intentionally look across both directions (earlier and later
    // by timestamp) because a batch import or a restarted daemon may have
    // inserted the context row either way.
    let session_branch: Option<String> = event_branch.clone().or_else(|| {
        conn.query_row(
            "SELECT git_branch FROM messages
             WHERE session_id = ?1
               AND git_branch IS NOT NULL AND git_branch != ''
             ORDER BY timestamp DESC LIMIT 1",
            params![sid],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
    });
    let session_repo: Option<String> = event_repo.clone().or_else(|| {
        conn.query_row(
            "SELECT repo_id FROM messages
             WHERE session_id = ?1
               AND repo_id IS NOT NULL AND repo_id != '' AND repo_id != 'Unassigned'
             ORDER BY timestamp DESC LIMIT 1",
            params![sid],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
    });

    (session_repo, session_branch)
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
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            duration_ms: 1200,
            status_code: 200,
            is_streaming: false,
            repo_id: String::new(),
            git_branch: String::new(),
            ticket_id: String::new(),
            cost_cents: 0.0,
            session_id: String::new(),
            activity: String::new(),
            activity_source: String::new(),
            activity_confidence: String::new(),
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
            None,
            None,
        );
        assert!(cost > 0.0, "cost should be positive for non-zero tokens");
    }

    #[test]
    fn compute_proxy_cost_with_cache_tokens() {
        // Without cache tokens
        let cost_no_cache = compute_proxy_cost_cents(
            ProxyProvider::Anthropic,
            "claude-opus-4-6",
            Some(1_000),
            Some(500),
            None,
            None,
        );
        // With cache tokens
        let cost_with_cache = compute_proxy_cost_cents(
            ProxyProvider::Anthropic,
            "claude-opus-4-6",
            Some(1_000),
            Some(500),
            Some(50_000),
            Some(100_000),
        );
        assert!(
            cost_with_cache > cost_no_cache,
            "cost with cache tokens ({cost_with_cache}) should exceed cost without ({cost_no_cache})"
        );
    }

    #[test]
    fn compute_proxy_cost_zero_tokens() {
        let cost =
            compute_proxy_cost_cents(ProxyProvider::OpenAi, "gpt-4o", None, None, None, None);
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

    #[test]
    fn insert_proxy_message_writes_activity_tags() {
        let conn = test_db_with_messages();
        let mut event = test_event();
        event.activity = "bugfix".to_string();
        event.activity_source = "rule".to_string();
        event.activity_confidence = "high".to_string();
        let uuid = insert_proxy_message(&conn, &event).unwrap();

        let mut stmt = conn
            .prepare("SELECT key, value FROM tags WHERE message_id = ?1 ORDER BY key")
            .unwrap();
        let tags: Vec<(String, String)> = stmt
            .query_map(params![uuid], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            tags,
            vec![
                ("activity".to_string(), "bugfix".to_string()),
                ("activity_confidence".to_string(), "high".to_string()),
                ("activity_source".to_string(), "rule".to_string()),
            ]
        );
    }

    #[test]
    fn classify_request_body_openai_last_user_turn_wins() {
        let body = serde_json::json!({
            "messages": [
                {"role": "user", "content": "add a login button"},
                {"role": "assistant", "content": "done"},
                {"role": "user", "content": "fix the crash in the login flow please"}
            ]
        })
        .to_string();
        let (cat, source, confidence) = classify_request_body(body.as_bytes()).expect("classifies");
        assert_eq!(cat, "bugfix");
        assert_eq!(source, "rule");
        assert!(!confidence.is_empty());
    }

    #[test]
    fn classify_request_body_anthropic_content_blocks() {
        let body = serde_json::json!({
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "refactor the authentication module to be async"}
                ]}
            ]
        })
        .to_string();
        let (cat, _, _) = classify_request_body(body.as_bytes()).expect("classifies");
        assert_eq!(cat, "refactor");
    }

    #[test]
    fn classify_request_body_returns_none_for_non_json() {
        assert!(classify_request_body(b"not json").is_none());
    }

    #[test]
    fn classify_request_body_returns_none_without_user() {
        let body = serde_json::json!({
            "messages": [{"role": "assistant", "content": "hi"}]
        })
        .to_string();
        assert!(classify_request_body(body.as_bytes()).is_none());
    }

    #[test]
    fn proxy_event_stores_cache_tokens() {
        let conn = test_db();
        let mut event = test_event();
        event.cache_creation_input_tokens = Some(5000);
        event.cache_read_input_tokens = Some(80000);
        let id = insert_proxy_event(&conn, &event).unwrap();

        let (cache_create, cache_read): (Option<i64>, Option<i64>) = conn
            .query_row(
                "SELECT cache_creation_input_tokens, cache_read_input_tokens
                 FROM proxy_events WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(cache_create, Some(5000));
        assert_eq!(cache_read, Some(80000));
    }

    /// Regression test for #302 — `budi sessions` returned empty for periods
    /// that clearly had proxy activity because `insert_proxy_message` dropped
    /// `session_id`, making every proxied row invisible to the
    /// `AND m.session_id IS NOT NULL` filter in `session_list_with_filters`.
    #[test]
    fn proxy_message_persists_session_id_and_is_visible_in_session_list() {
        let conn = test_db_with_messages();
        let mut event = test_event();
        event.session_id = "proxy-session-abc".to_string();
        event.timestamp = chrono::Utc::now().to_rfc3339();

        let uuid = insert_proxy_message(&conn, &event).unwrap();

        let stored: Option<String> = conn
            .query_row(
                "SELECT session_id FROM messages WHERE id = ?1",
                params![uuid],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored.as_deref(), Some("proxy-session-abc"));

        // Window that includes the event — `budi sessions -p today` would use
        // an equivalent `since`. The session must be listed.
        let since = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let paginated = crate::analytics::session_list_with_filters(
            &conn,
            &crate::analytics::SessionListParams {
                since: Some(&since),
                until: None,
                search: None,
                sort_by: None,
                sort_asc: false,
                limit: 50,
                offset: 0,
                ticket: None,
                activity: None,
            },
            &crate::analytics::DimensionFilters::default(),
        )
        .unwrap();
        assert_eq!(paginated.total_count, 1);
        assert_eq!(paginated.sessions.len(), 1);
        assert_eq!(paginated.sessions[0].id, "proxy-session-abc");
    }

    /// Defensive: an empty `session_id` string is stored as NULL so it cannot
    /// quietly produce a `(empty)` session bucket or confuse downstream
    /// `session_id IS NOT NULL` checks. Such rows are intentionally invisible
    /// to `budi sessions` (there is no session to attribute them to).
    #[test]
    fn proxy_message_empty_session_id_is_stored_as_null() {
        let conn = test_db_with_messages();
        let event = test_event(); // session_id = ""

        let uuid = insert_proxy_message(&conn, &event).unwrap();

        let stored: Option<String> = conn
            .query_row(
                "SELECT session_id FROM messages WHERE id = ?1",
                params![uuid],
                |row| row.get(0),
            )
            .unwrap();
        assert!(stored.is_none(), "empty session_id must be stored as NULL");
    }

    // Regression tests for #303 — branch attribution on live proxy ingest.

    /// `ProxyAttribution::resolve` must populate the branch when the caller
    /// supplies a cwd that points at a git worktree, even without headers.
    /// This is the fallback path exercised by the new socket-PID lookup and by
    /// `budi launch` callers.
    /// Create a throwaway directory path for a single test. Caller is
    /// expected to create/remove the directory themselves — kept out of a
    /// shared helper so tests can be moved into dedicated integration files
    /// later without a cross-crate helper crate dependency.
    fn unique_test_dir(tag: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "budi-proxy-303-{tag}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4(),
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn git(repo: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr),
        );
    }

    #[test]
    fn attribution_resolve_populates_branch_from_cwd_git_repo() {
        let repo = unique_test_dir("branch-ok");
        // `git init -b` exists on 2.28+. Fall back to renaming main so we run
        // on older git too (older git in CI images still honors checkout -b).
        git(&repo, &["init", "-q"]);
        git(
            &repo,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ],
        );
        git(&repo, &["checkout", "-q", "-b", "PROJ-42-feature"]);

        let attr = ProxyAttribution::resolve(None, None, repo.to_str());
        assert_eq!(attr.git_branch, "PROJ-42-feature");
        assert_eq!(attr.ticket_id, "PROJ-42");
        assert_ne!(
            attr.repo_id, UNASSIGNED_REPO,
            "cwd-based git resolution must also populate repo_id"
        );

        let _ = std::fs::remove_dir_all(&repo);
    }

    /// Detached HEAD is explicitly treated as "no branch" — we must never emit
    /// the literal string `HEAD` as a branch, otherwise `budi stats --branches`
    /// accumulates a bogus `HEAD` bucket for worktrees, CI runs, and mid-rebase
    /// sessions.
    #[test]
    fn attribution_resolve_detached_head_yields_empty_branch() {
        let repo = unique_test_dir("detached");
        git(&repo, &["init", "-q"]);
        git(
            &repo,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "first",
            ],
        );
        let sha = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&repo)
            .output()
            .unwrap();
        let sha = String::from_utf8(sha.stdout).unwrap().trim().to_string();
        git(&repo, &["checkout", "-q", &sha]);

        let attr = ProxyAttribution::resolve(None, None, repo.to_str());
        assert!(
            attr.git_branch.is_empty(),
            "detached HEAD must not leak as a literal 'HEAD' branch, got: {:?}",
            attr.git_branch
        );
        assert_eq!(attr.ticket_id, "Unassigned");

        let _ = std::fs::remove_dir_all(&repo);
    }

    /// Later messages in a session inherit the branch set by an earlier
    /// message in the same session, matching the pipeline's
    /// `propagate_session_context` behavior but on the live ingest path.
    #[test]
    fn insert_proxy_message_inherits_branch_from_earlier_session_message() {
        let conn = test_db_with_messages();

        let mut first = test_event();
        first.session_id = "sess-propagate".to_string();
        first.timestamp = "2026-04-10T10:00:00Z".to_string();
        first.repo_id = "github.com/test/repo".to_string();
        first.git_branch = "PROJ-42-feature".to_string();
        insert_proxy_message(&conn, &first).unwrap();

        // Second turn in the same session arrives with no attribution — this
        // is the common case when headers are missing and cwd could not be
        // resolved for that particular request.
        let mut second = test_event();
        second.session_id = "sess-propagate".to_string();
        second.timestamp = "2026-04-10T10:00:05Z".to_string();
        let uuid = insert_proxy_message(&conn, &second).unwrap();

        let (branch, repo): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT git_branch, repo_id FROM messages WHERE id = ?1",
                params![uuid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(branch.as_deref(), Some("PROJ-42-feature"));
        assert_eq!(repo.as_deref(), Some("github.com/test/repo"));
    }

    /// The opposite direction: a late-arriving event that finally resolves a
    /// branch must retroactively fill earlier rows of the same session that
    /// went in without one. This is the race called out in #303.
    #[test]
    fn insert_proxy_message_backfills_earlier_session_rows_when_branch_appears() {
        let conn = test_db_with_messages();

        let mut first = test_event();
        first.session_id = "sess-backfill".to_string();
        first.timestamp = "2026-04-10T10:00:00Z".to_string();
        let first_uuid = insert_proxy_message(&conn, &first).unwrap();

        let mut second = test_event();
        second.session_id = "sess-backfill".to_string();
        second.timestamp = "2026-04-10T10:00:05Z".to_string();
        second.repo_id = "github.com/test/repo".to_string();
        second.git_branch = "PROJ-42-feature".to_string();
        insert_proxy_message(&conn, &second).unwrap();

        let (branch, repo): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT git_branch, repo_id FROM messages WHERE id = ?1",
                params![first_uuid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            branch.as_deref(),
            Some("PROJ-42-feature"),
            "earlier message must be backfilled with the later session branch"
        );
        assert_eq!(repo.as_deref(), Some("github.com/test/repo"));
    }

    /// Backfill must be scoped to the affected session — sibling sessions
    /// keep their own attribution (including their own missing state).
    #[test]
    fn insert_proxy_message_backfill_is_scoped_to_session() {
        let conn = test_db_with_messages();

        let mut a = test_event();
        a.session_id = "sess-a".to_string();
        a.timestamp = "2026-04-10T10:00:00Z".to_string();
        let a_uuid = insert_proxy_message(&conn, &a).unwrap();

        let mut b = test_event();
        b.session_id = "sess-b".to_string();
        b.timestamp = "2026-04-10T10:00:01Z".to_string();
        b.git_branch = "OTHER-9-fix".to_string();
        insert_proxy_message(&conn, &b).unwrap();

        let branch: Option<String> = conn
            .query_row(
                "SELECT git_branch FROM messages WHERE id = ?1",
                params![a_uuid],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            branch.is_none(),
            "session A must not inherit session B's branch, got: {branch:?}"
        );
    }

    #[test]
    fn proxy_message_stores_cache_tokens() {
        let conn = test_db_with_messages();
        let mut event = test_event();
        event.cache_creation_input_tokens = Some(3000);
        event.cache_read_input_tokens = Some(60000);
        let uuid = insert_proxy_message(&conn, &event).unwrap();

        let (cache_create, cache_read): (i64, i64) = conn
            .query_row(
                "SELECT cache_creation_tokens, cache_read_tokens
                 FROM messages WHERE id = ?1",
                params![uuid],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(cache_create, 3000);
        assert_eq!(cache_read, 60000);
    }
}
