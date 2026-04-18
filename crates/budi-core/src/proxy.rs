//! Proxy event types and (deprecated) analytics storage.
//!
//! ## R1.4 (#320, ADR-0089) deprecation
//!
//! As of 8.2 R1.4 ([#320](https://github.com/siropkin/budi/issues/320)) the
//! proxy is no longer the live ingestion path. The JSONL tailer worker
//! (`crates/budi-daemon/src/workers/tailer.rs`, R1.3 #319) is the sole live
//! source per ADR-0089 §1.
//!
//! [`insert_proxy_event`] and [`insert_proxy_message`] are intentionally
//! short-circuited to no-ops (with a single per-process deprecation warning)
//! so the proxy can keep forwarding traffic during the soak window without
//! racing the tailer on the same messages. The
//! [`analytics/sync.rs`](../analytics/sync.rs.html) `proxy_cutoff` dedup rule
//! that papered over the dual-write race is removed in the same PR.
//!
//! The `proxy_events` table itself is preserved for read-only access to 8.1.x
//! history. R2.5 ([#326](https://github.com/siropkin/budi/issues/326))
//! decides the fate of those rows on upgrade. This whole module is deleted in
//! R2.1 ([#322](https://github.com/siropkin/budi/issues/322)).

use std::sync::Once;

use anyhow::Result;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::pipeline::extract_ticket_from_branch;

/// Logs the proxy-ingestion deprecation banner once per process, the first
/// time [`insert_proxy_event`] or [`insert_proxy_message`] is invoked.
fn log_proxy_ingestion_deprecated_once() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        tracing::warn!(
            target: "budi_core::proxy",
            "Proxy ingestion is a no-op as of 8.2 R1.4 (#320, ADR-0089 §1). \
             Live data flows through the JSONL tailer worker (#319). \
             The proxy still forwards traffic but no longer writes to `proxy_events` or `messages`. \
             The proxy crate is removed in 8.2 R2.1 (#322)."
        );
    });
}

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
///
/// `ticket_id` is empty (not a sentinel) when no ticket could be derived
/// from the branch — the live insert path treats empty as "do not tag"
/// so proxy and import paths agree on the `(untagged)` bucket. `ticket_source`
/// records which extractor produced the id (R1.3, #221) — one of
/// `pipeline::TICKET_SOURCE_BRANCH` / `pipeline::TICKET_SOURCE_BRANCH_NUMERIC`
/// when `ticket_id` is set, or empty otherwise.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProxyAttribution {
    pub repo_id: String,
    pub git_branch: String,
    pub ticket_id: String,
    #[serde(default)]
    pub ticket_source: String,
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

        // R1.3 (#221): unified ticket extraction — the pipeline helper
        // handles integration-branch filtering, alpha pattern, and the
        // ADR-0082 §9 numeric fallback in one place so the proxy and
        // `budi import` agree on what counts as a ticket. An empty
        // `ticket_id` here means "no ticket" and `insert_proxy_message`
        // will skip the ticket tag writes entirely — no more phantom
        // `Unassigned` ticket bucket sitting next to `(untagged)`.
        let (ticket_id, ticket_source) = match extract_ticket_from_branch(&resolved_branch) {
            Some((id, source)) => (id, source.to_string()),
            None => (String::new(), String::new()),
        };

        Self {
            repo_id,
            git_branch: resolved_branch,
            ticket_id,
            ticket_source,
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
    /// Source the ticket id was derived from (R1.3, #221). Mirrors
    /// `pipeline::TICKET_SOURCE_BRANCH` / `_BRANCH_NUMERIC`. Empty when
    /// `ticket_id` is empty.
    #[serde(default)]
    pub ticket_source: String,
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

/// **Deprecated (R1.4 #320, ADR-0089 §1) — no-op.**
///
/// The proxy is no longer the live ingestion path. This function logs the
/// per-process deprecation banner once and returns `Ok(0)` without touching
/// the database. The whole module is deleted in R2.1 (#322).
///
/// `_event` is kept in the signature so callers don't need to change shape
/// during the brief window between this PR and R2.1.
pub fn insert_proxy_event(_conn: &Connection, _event: &ProxyEvent) -> Result<i64> {
    log_proxy_ingestion_deprecated_once();
    Ok(0)
}

/// **Deprecated (R1.4 #320, ADR-0089 §1) — no-op.**
///
/// Live ingestion is the JSONL tailer worker (#319). This function logs the
/// per-process deprecation banner once and returns `Ok(String::new())`
/// without writing to the `messages` table. The session-level branch
/// propagation that used to live here (#303) is now handled by the pipeline
/// for tailer-ingested rows; the proxy never carried the agent JSONL's
/// `gitBranch` field anyway. The whole module is deleted in R2.1 (#322).
pub fn insert_proxy_message(_conn: &Connection, _event: &ProxyEvent) -> Result<String> {
    log_proxy_ingestion_deprecated_once();
    Ok(String::new())
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
            ticket_source: String::new(),
            cost_cents: 0.0,
            session_id: String::new(),
            activity: String::new(),
            activity_source: String::new(),
            activity_confidence: String::new(),
        }
    }

    #[test]
    fn ensure_schema_is_idempotent() {
        let conn = test_db();
        ensure_proxy_schema(&conn).unwrap();
        ensure_proxy_schema(&conn).unwrap();
    }

    // ---- R1.4 (#320, ADR-0089 §1) no-op contract for proxy ingestion ----
    //
    // The proxy crate is removed in R2.1 (#322). For the soak window between
    // this PR and R2.1 the ingestion functions stay in the API surface but
    // do not write — the JSONL tailer is the sole live ingestion path. The
    // tests below pin that contract so a regression cannot silently revive
    // dual-writes (which would re-introduce the `proxy_cutoff` dedup that
    // R1.4 deletes from `analytics::sync`).

    /// `insert_proxy_event` returns `Ok(0)` and writes nothing to
    /// `proxy_events`. The 0 stand-in is fine because no caller reads the
    /// returned rowid (it was only ever used for debug logs).
    #[test]
    fn insert_proxy_event_is_noop_after_r1_4() {
        let conn = test_db();
        let event = test_event();
        let id = insert_proxy_event(&conn, &event).unwrap();
        assert_eq!(id, 0, "no-op must return 0; rows were not inserted");

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM proxy_events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            count, 0,
            "proxy_events must not receive new writes from insert_proxy_event \
             (ADR-0089 §1; R1.4 #320). Live ingestion is the JSONL tailer only."
        );
    }

    /// `insert_proxy_message` returns `Ok(String::new())` and writes nothing
    /// to `messages` or `tags`. The empty UUID stand-in is fine because the
    /// only caller (`record_event_blocking` in the daemon proxy route) only
    /// looks at the `Result` discriminant.
    #[test]
    fn insert_proxy_message_is_noop_after_r1_4() {
        let conn = test_db_with_messages();
        let mut event = test_event();
        // Populate the same fields the pre-R1.4 test exercised so we know
        // the no-op holds even when full attribution / activity / cost
        // would otherwise have been written.
        event.session_id = "proxy-session-noop".to_string();
        event.repo_id = "github.com/test/repo".to_string();
        event.git_branch = "PROJ-42-fix".to_string();
        event.ticket_id = "PROJ-42".to_string();
        event.ticket_source = "branch".to_string();
        event.activity = "bugfix".to_string();
        event.activity_source = "rule".to_string();
        event.activity_confidence = "high".to_string();
        event.cost_cents = 2.5;

        let uuid = insert_proxy_message(&conn, &event).unwrap();
        assert!(
            uuid.is_empty(),
            "no-op must return an empty uuid stand-in; got {uuid:?}"
        );

        let messages_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            messages_count, 0,
            "no proxy-derived row may land in `messages` after R1.4 (#320). \
             The JSONL tailer is the sole live writer."
        );

        let tags_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM tags", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            tags_count, 0,
            "no proxy-derived tag may land in `tags` after R1.4 (#320)."
        );

        // Specifically: the pre-R1.4 path stamped `cost_confidence =
        // 'proxy_estimated'` on every successful insert, which the
        // `proxy_cutoff` dedup keyed on. That marker must no longer
        // appear from new ingestion (existing rows from 8.1.x stay
        // queryable; their disposition is R2.5 / #326).
        let proxy_estimated: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE cost_confidence = 'proxy_estimated'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            proxy_estimated, 0,
            "no `cost_confidence='proxy_estimated'` rows may be written after R1.4 (#320)"
        );
    }

    // ---- ProxyAttribution::resolve — pure helper still consumed by the
    //      proxy route's structured-log line. Stays valid until R2.1 (#322)
    //      deletes the route. ----

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
        assert_eq!(attr.ticket_source, "branch");
    }

    #[test]
    fn attribution_resolve_empty_returns_empty_ticket() {
        let attr = ProxyAttribution::resolve(None, None, None);
        assert_eq!(attr.repo_id, UNASSIGNED_REPO);
        assert!(attr.git_branch.is_empty());
        assert!(attr.ticket_id.is_empty());
        assert!(attr.ticket_source.is_empty());
    }

    #[test]
    fn attribution_resolve_empty_repo_with_branch() {
        let attr = ProxyAttribution::resolve(Some(""), Some("ABC-123-feat"), None);
        assert_eq!(attr.repo_id, UNASSIGNED_REPO);
        assert_eq!(attr.git_branch, "ABC-123-feat");
        assert_eq!(attr.ticket_id, "ABC-123");
        assert_eq!(attr.ticket_source, "branch");
    }

    #[test]
    fn attribution_resolve_poor_branch_no_ticket() {
        let attr = ProxyAttribution::resolve(Some("my-repo"), Some("main"), None);
        assert_eq!(attr.repo_id, "my-repo");
        assert_eq!(attr.git_branch, "main");
        assert!(attr.ticket_id.is_empty());
        assert!(attr.ticket_source.is_empty());
    }

    #[test]
    fn attribution_resolve_numeric_only_ticket() {
        let attr = ProxyAttribution::resolve(Some("repo"), Some("fix/1234-typo"), None);
        assert_eq!(attr.ticket_id, "1234");
        assert_eq!(attr.ticket_source, "branch_numeric");
    }

    #[test]
    fn attribution_resolve_develop_branch_no_ticket() {
        let attr = ProxyAttribution::resolve(Some("repo"), Some("develop"), None);
        assert!(attr.ticket_id.is_empty());
        assert!(attr.ticket_source.is_empty());
    }

    #[test]
    fn attribution_resolve_master_branch_no_ticket() {
        let attr = ProxyAttribution::resolve(Some("repo"), Some("master"), None);
        assert!(attr.ticket_id.is_empty());
        assert!(attr.ticket_source.is_empty());
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
        let cost_no_cache = compute_proxy_cost_cents(
            ProxyProvider::Anthropic,
            "claude-opus-4-6",
            Some(1_000),
            Some(500),
            None,
            None,
        );
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

    // ---- classify_request_body — pure prompt classifier, still consumed
    //      by the proxy route's `activity` / structured-log line. ----

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
}
