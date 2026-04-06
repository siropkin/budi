//! Hook event processing for Claude Code and Cursor.
//!
//! Both editors support lifecycle hooks that fire real-time events with metadata.
//! This module parses those events, inserts them into the `hook_events` table,
//! and upserts session records in the `sessions` table.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use serde_json::Value;

/// A parsed hook event, ready for insertion into `hook_events`.
#[derive(Debug)]
pub struct HookEvent {
    pub provider: String,
    pub event: String,
    pub session_id: Option<String>,
    pub timestamp: DateTime<Utc>,
    pub model: Option<String>,
    // Session lifecycle
    pub duration_ms: Option<i64>,
    pub composer_mode: Option<String>,
    pub permission_mode: Option<String>,
    pub workspace_root: Option<String>,
    pub user_email: Option<String>,
    pub end_reason: Option<String>,
    // Tool
    pub tool_name: Option<String>,
    pub tool_duration_ms: Option<i64>,
    // Agentic stats
    pub tool_call_count: Option<i64>,
    // Resolved from workspace
    pub repo_id: Option<String>,
    pub git_branch: Option<String>,
    // MCP
    pub mcp_server: Option<String>,
    pub message_id: Option<String>,
    pub message_request_id: Option<String>,
    pub tool_use_id: Option<String>,
    pub link_confidence: Option<String>,
    // Raw
    pub raw_json: String,
}

pub const HOOK_LINK_EXACT_REQUEST_ID: &str = "exact_request_id";
pub const HOOK_LINK_EXACT_TOOL_USE_ID: &str = "exact_tool_use_id";
pub const HOOK_LINK_UNLINKED: &str = "unlinked";

fn extract_string_path(json: &Value, path: &[&str]) -> Option<String> {
    let mut current = json;
    for key in path {
        current = current.get(*key)?;
    }
    current
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

pub fn extract_hook_message_request_id(json: &Value) -> Option<String> {
    let top_level_keys = [
        "message_request_id",
        "message_id",
        "request_id",
        "response_id",
        "id",
    ];
    for key in top_level_keys {
        if let Some(value) = json
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(value.to_string());
        }
    }

    let nested_paths = [
        ["message", "id"].as_slice(),
        ["request", "id"].as_slice(),
        ["response", "id"].as_slice(),
        ["payload", "id"].as_slice(),
        ["data", "id"].as_slice(),
    ];
    nested_paths
        .iter()
        .find_map(|path| extract_string_path(json, path))
}

pub fn extract_hook_tool_use_id(json: &Value) -> Option<String> {
    let top_level_keys = ["tool_use_id", "tool_call_id"];
    for key in top_level_keys {
        if let Some(value) = json
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(value.to_string());
        }
    }

    let nested_paths = [
        ["tool_use", "id"].as_slice(),
        ["tool_call", "id"].as_slice(),
        ["payload", "tool_use_id"].as_slice(),
        ["payload", "tool_call_id"].as_slice(),
    ];
    nested_paths
        .iter()
        .find_map(|path| extract_string_path(json, path))
}

/// Parse a hook event from raw JSON (stdin from hook command).
/// Auto-detects provider: if `cursor_version` is present → cursor, else → claude_code.
pub fn parse_hook_event(json: &Value) -> Result<HookEvent> {
    let is_cursor = json.get("cursor_version").is_some();
    let provider = if is_cursor { "cursor" } else { "claude_code" };

    // Normalize event name: CamelCase (CC) or camelCase (Cursor) → snake_case
    let raw_event = json
        .get("hook_event_name")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let event = normalize_event_name(raw_event);

    // Session ID: CC uses session_id, Cursor uses conversation_id
    let session_id = json
        .get("session_id")
        .or_else(|| json.get("conversation_id"))
        .and_then(|v| v.as_str())
        .map(crate::identity::normalize_session_id);

    let model = json
        .get("model")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // Session lifecycle fields
    let duration_ms = json.get("duration_ms").and_then(|v| v.as_i64());
    let composer_mode = json
        .get("composer_mode")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let permission_mode = json
        .get("permission_mode")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let user_email = json
        .get("user_email")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let end_reason = json
        .get("reason")
        .or_else(|| json.get("end_reason"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // Workspace root: CC uses cwd, Cursor uses workspace_roots[0]
    let workspace_root = json
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            json.get("workspace_roots")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        });

    // Tool fields
    let tool_name = json
        .get("tool_name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let tool_duration_ms = json
        .get("duration")
        .or_else(|| json.get("tool_duration_ms"))
        .and_then(|v| v.as_i64());

    // Agentic stats
    let tool_call_count = json.get("tool_call_count").and_then(|v| v.as_i64());

    // Resolve repo_id and git_branch from workspace root
    let (repo_id, git_branch) = workspace_root
        .as_deref()
        .map(|ws| {
            let path = std::path::Path::new(ws);
            let rid = crate::repo_id::resolve_repo_id(path);
            let branch = crate::providers::cursor::resolve_git_branch_from_head(ws);
            (Some(rid), branch)
        })
        .unwrap_or((None, None));

    // Extract MCP server name from tool_name (mcp__<server>__<tool>)
    let mcp_server = tool_name.as_deref().and_then(|name| {
        let parts: Vec<&str> = name.splitn(3, "__").collect();
        if parts.len() >= 2 && parts[0] == "mcp" {
            Some(parts[1].to_string())
        } else {
            None
        }
    });

    let raw_json = json.to_string();
    let message_request_id = extract_hook_message_request_id(json);
    let tool_use_id = extract_hook_tool_use_id(json);

    Ok(HookEvent {
        provider: provider.to_string(),
        event,
        session_id,
        timestamp: Utc::now(),
        model,
        duration_ms,
        composer_mode,
        permission_mode,
        workspace_root,
        user_email,
        end_reason,
        tool_name,
        tool_duration_ms,
        tool_call_count,
        repo_id,
        git_branch,
        mcp_server,
        message_id: None,
        message_request_id,
        tool_use_id,
        link_confidence: Some(HOOK_LINK_UNLINKED.to_string()),
        raw_json,
    })
}

pub fn resolve_hook_message_link(
    conn: &Connection,
    session_id: Option<&str>,
    message_request_id: Option<&str>,
    tool_use_id: Option<&str>,
) -> Result<(Option<String>, String)> {
    let Some(sid) = session_id.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok((None, HOOK_LINK_UNLINKED.to_string()));
    };

    if let Some(request_id) = message_request_id.map(str::trim).filter(|s| !s.is_empty()) {
        let linked_by_request: Option<String> = conn
            .query_row(
                "SELECT uuid
                 FROM messages
                 WHERE session_id = ?1 AND request_id = ?2
                 ORDER BY timestamp DESC
                 LIMIT 1",
                params![sid, request_id],
                |row| row.get(0),
            )
            .ok();
        if let Some(uuid) = linked_by_request {
            return Ok((Some(uuid), HOOK_LINK_EXACT_REQUEST_ID.to_string()));
        }
    }

    if let Some(tool_id) = tool_use_id.map(str::trim).filter(|s| !s.is_empty()) {
        let linked_by_tool: Option<String> = conn
            .query_row(
                "SELECT m.uuid
                 FROM messages m
                 JOIN tags t ON t.message_uuid = m.uuid
                 WHERE m.session_id = ?1
                   AND t.key = 'tool_use_id'
                   AND t.value = ?2
                 ORDER BY m.timestamp DESC
                 LIMIT 1",
                params![sid, tool_id],
                |row| row.get(0),
            )
            .ok();
        if let Some(uuid) = linked_by_tool {
            return Ok((Some(uuid), HOOK_LINK_EXACT_TOOL_USE_ID.to_string()));
        }
    }

    Ok((None, HOOK_LINK_UNLINKED.to_string()))
}

/// Insert a hook event into the `hook_events` table.
pub fn ingest_hook_event(conn: &Connection, event: &HookEvent) -> Result<()> {
    if let (Some(session_id), Some(tool_use_id)) =
        (event.session_id.as_deref(), event.tool_use_id.as_deref())
        && event.event == "post_tool_use"
    {
        let already_exists: bool = conn
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM hook_events
                    WHERE session_id = ?1
                      AND event = 'post_tool_use'
                      AND tool_use_id = ?2
                )",
                params![session_id, tool_use_id],
                |row| row.get(0),
            )
            .unwrap_or(false);
        if already_exists {
            return Ok(());
        }
    }

    let (linked_message_id, link_confidence) = resolve_hook_message_link(
        conn,
        event.session_id.as_deref(),
        event.message_request_id.as_deref(),
        event.tool_use_id.as_deref(),
    )?;
    let message_id = event
        .message_id
        .clone()
        .or(linked_message_id)
        .filter(|s| !s.trim().is_empty());
    let confidence = event
        .link_confidence
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(link_confidence);

    conn.execute(
        "INSERT INTO hook_events (
            provider, event, session_id, timestamp, model,
            tool_name, tool_duration_ms, tool_call_count,
            raw_json, mcp_server, message_id, message_request_id,
            tool_use_id, link_confidence
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            event.provider,
            event.event,
            event.session_id,
            event.timestamp.to_rfc3339(),
            event.model,
            event.tool_name,
            event.tool_duration_ms,
            event.tool_call_count,
            event.raw_json,
            event.mcp_server,
            message_id,
            event.message_request_id,
            event.tool_use_id,
            confidence,
        ],
    )
    .context("Failed to insert hook event")?;
    Ok(())
}

/// Upsert a session record based on a hook event.
/// Ensures a session row exists for every event (not just session_start),
/// since some providers (e.g. Cursor) may not send session_start at all.
/// Then applies event-specific updates on top.
pub fn upsert_session(conn: &Connection, event: &HookEvent) -> Result<()> {
    let Some(ref sid) = event.session_id else {
        return Ok(()); // No session_id → can't create session
    };

    // Ensure a session row exists regardless of which event arrives first.
    // Cursor often sends post_tool_use before (or instead of) session_start.
    conn.execute(
        "INSERT OR IGNORE INTO sessions (session_id, provider, started_at, workspace_root, repo_id, git_branch)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            sid,
            event.provider,
            event.timestamp.to_rfc3339(),
            event.workspace_root,
            event.repo_id,
            event.git_branch,
        ],
    )?;

    match event.event.as_str() {
        "session_start" => {
            conn.execute(
                "UPDATE sessions SET
                    started_at = ?2,
                    composer_mode = COALESCE(?3, composer_mode),
                    permission_mode = COALESCE(?4, permission_mode),
                    user_email = COALESCE(?5, NULLIF(user_email, '')),
                    workspace_root = COALESCE(?6, NULLIF(workspace_root, '')),
                    model = COALESCE(?7, model),
                    raw_json = ?8,
                    repo_id = COALESCE(?9, NULLIF(NULLIF(repo_id, ''), 'unknown')),
                    git_branch = COALESCE(?10, NULLIF(git_branch, ''))
                WHERE session_id = ?1",
                params![
                    sid,
                    event.timestamp.to_rfc3339(),
                    event.composer_mode,
                    event.permission_mode,
                    event.user_email,
                    event.workspace_root,
                    event.model,
                    event.raw_json,
                    event.repo_id,
                    event.git_branch,
                ],
            )?;
        }
        "session_end" => {
            conn.execute(
                "UPDATE sessions SET
                    ended_at = ?2,
                    duration_ms = ?3,
                    end_reason = ?4,
                    model = COALESCE(?5, model),
                    user_email = COALESCE(?6, NULLIF(user_email, '')),
                    repo_id = COALESCE(?7, NULLIF(NULLIF(repo_id, ''), 'unknown')),
                    git_branch = COALESCE(?8, NULLIF(git_branch, ''))
                WHERE session_id = ?1",
                params![
                    sid,
                    event.timestamp.to_rfc3339(),
                    event.duration_ms,
                    event.end_reason,
                    event.model,
                    event.user_email,
                    event.repo_id,
                    event.git_branch,
                ],
            )?;
        }
        _ => {
            conn.execute(
                "UPDATE sessions SET
                    started_at = MIN(started_at, ?2),
                    model = COALESCE(?3, model),
                    user_email = COALESCE(NULLIF(user_email, ''), ?4),
                    workspace_root = COALESCE(NULLIF(workspace_root, ''), ?5),
                    repo_id = COALESCE(NULLIF(NULLIF(repo_id, ''), 'unknown'), ?6),
                    git_branch = COALESCE(NULLIF(git_branch, ''), ?7)
                 WHERE session_id = ?1",
                params![
                    sid,
                    event.timestamp.to_rfc3339(),
                    event.model,
                    event.user_email,
                    event.workspace_root,
                    event.repo_id,
                    event.git_branch,
                ],
            )?;
        }
    }

    Ok(())
}

/// Update a session's prompt_category.
pub fn update_session_category(conn: &Connection, event: &HookEvent, category: &str) -> Result<()> {
    if let Some(ref sid) = event.session_id {
        conn.execute(
            "UPDATE sessions SET prompt_category = ?2
             WHERE session_id = ?1 AND prompt_category IS NULL",
            params![sid, category],
        )?;
    }
    Ok(())
}

/// Check if `text` contains `word` at a word boundary.
/// Handles both single words ("fix") and phrases ("clean up").
fn contains_word(text: &str, word: &str) -> bool {
    let mut start = 0;
    while let Some(pos) = text[start..].find(word) {
        let abs_pos = start + pos;
        let before_ok = abs_pos == 0 || !text.as_bytes()[abs_pos - 1].is_ascii_alphanumeric();
        let after_pos = abs_pos + word.len();
        let after_ok =
            after_pos >= text.len() || !text.as_bytes()[after_pos].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
        start = abs_pos + 1;
    }
    false
}

/// Classify a user prompt into a category using keyword heuristics.
/// Returns None if no category matches (system commands, very short, or ambiguous).
///
/// Categories: bugfix, refactor, testing, review, ops, question, writing, feature.
pub fn classify_prompt(text: &str) -> Option<String> {
    let lower = text.to_lowercase();

    // Skip system commands, tool results, and trivially short input
    if lower.starts_with('/') || lower.len() < 5 {
        return None;
    }
    // Skip XML/HTML tool output but not user text that merely contains angle brackets
    if lower.starts_with('<') && !lower.contains(' ') {
        return None;
    }

    // Check in priority order (most specific first).
    // Within each group the word-boundary check avoids false positives
    // (e.g. "testing" won't match the "test" in "contest").
    let bugfix_words = [
        "fix",
        "bug",
        "broken",
        "error",
        "crash",
        "issue",
        "debug",
        "failing",
        "fails",
        "wrong",
        "regression",
        "workaround",
        "patch",
        "hotfix",
        "not working",
        "doesn't work",
        "does not work",
        "isn't working",
        "stopped working",
    ];
    let refactor_words = [
        "refactor",
        "rename",
        "clean up",
        "extract",
        "reorganize",
        "simplify",
        "restructure",
        "move",
        "split",
        "consolidate",
        "deduplicate",
        "dedup",
        "inline",
        "remove",
        "delete",
        "deprecate",
        "replace",
        "convert",
        "rewrite",
        "tidy",
    ];
    let testing_words = [
        "test",
        "tests",
        "testing",
        "spec",
        "specs",
        "unit test",
        "integration test",
        "e2e",
        "coverage",
        "assert",
        "mock",
        "fixture",
        "snapshot",
    ];
    let review_words = [
        "review",
        "audit",
        "validate",
        "verify",
        "inspect",
        "look at",
        "take a look",
        "examine",
        "analyze",
        "analyse",
        "assess",
        "evaluate",
        "feedback",
        "critique",
    ];
    let ops_words = [
        "deploy",
        "release",
        "migrate",
        "upgrade",
        "bump",
        "publish",
        "install",
        "commit",
        "push",
        "merge",
        "rebase",
        "cherry-pick",
        "rollback",
        "revert",
        "configure",
        "provision",
        "ci",
        "cd",
        "docker",
        "k8s",
        "kubernetes",
        "terraform",
        "ansible",
    ];
    let question_words = [
        "why",
        "how does",
        "how do",
        "how can",
        "how to",
        "how much",
        "how often",
        "how many",
        "what is",
        "what does",
        "what are",
        "where is",
        "where does",
        "where are",
        "when does",
        "when is",
        "which",
        "can you tell",
        "can you explain",
        "explain",
        "understand",
        "show me",
        "discover",
        "research",
        "what happens",
        "is there",
        "are there",
        "do we",
        "does this",
        "could you",
        "tell me",
    ];
    let feature_words = [
        "add",
        "implement",
        "create",
        "build",
        "new feature",
        "integrate",
        "introduce",
        "design",
        "make",
        "change",
        "modify",
        "adjust",
        "tweak",
        "set up",
        "setup",
        "enable",
        "support",
        "extend",
        "enhance",
        "improve",
        "optimize",
        "update",
    ];
    let writing_words = [
        "write",
        "draft",
        "article",
        "post",
        "document",
        "blog",
        "readme",
        "changelog",
        "documentation",
    ];
    let plan_words = [
        "plan",
        "the plan",
        "implement the plan",
        "read and implement",
    ];

    if bugfix_words.iter().any(|w| contains_word(&lower, w)) {
        Some("bugfix".to_string())
    } else if refactor_words.iter().any(|w| contains_word(&lower, w)) {
        Some("refactor".to_string())
    } else if testing_words.iter().any(|w| contains_word(&lower, w)) {
        Some("testing".to_string())
    } else if plan_words.iter().any(|w| contains_word(&lower, w)) {
        Some("feature".to_string())
    } else if review_words.iter().any(|w| contains_word(&lower, w)) {
        Some("review".to_string())
    } else if ops_words.iter().any(|w| contains_word(&lower, w)) {
        Some("ops".to_string())
    } else if question_words.iter().any(|w| contains_word(&lower, w)) || lower.ends_with('?') {
        Some("question".to_string())
    } else if writing_words.iter().any(|w| contains_word(&lower, w)) {
        Some("writing".to_string())
    } else if feature_words.iter().any(|w| contains_word(&lower, w)) {
        Some("feature".to_string())
    } else {
        None
    }
}

/// Query aggregated session metadata for the HookEnricher.
/// Returns a map of session_id → SessionMeta.
/// When `max_age_days` is Some(N), only sessions started in the last N days are loaded.
pub fn load_session_meta(
    conn: &Connection,
    max_age_days: Option<u64>,
) -> Result<std::collections::HashMap<String, SessionMeta>> {
    let mut map = std::collections::HashMap::new();

    let (date_clause, date_param) = match max_age_days {
        Some(days) => (
            " AND started_at >= datetime('now', ?1)".to_string(),
            Some(format!("-{} days", days)),
        ),
        None => (String::new(), None),
    };
    let sql = format!(
        "SELECT session_id, composer_mode, permission_mode, prompt_category,
                user_email, duration_ms, model, repo_id, git_branch
         FROM sessions
         WHERE session_id IS NOT NULL{}",
        date_clause
    );
    let mut stmt = conn.prepare(&sql)?;

    let params: Vec<Box<dyn rusqlite::types::ToSql>> = match date_param {
        Some(p) => vec![Box::new(p)],
        None => vec![],
    };
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let mut rows = stmt.query(param_refs.as_slice())?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let meta = SessionMeta {
            composer_mode: row.get(1)?,
            permission_mode: row.get(2)?,
            prompt_category: row.get(3)?,
            user_email: row.get(4)?,
            duration_ms: row.get(5)?,
            model: row.get(6)?,
            repo_id: row.get(7)?,
            git_branch: row.get(8)?,
        };
        map.insert(id, meta);
    }

    Ok(map)
}

/// Aggregated session metadata for enrichment.
#[derive(Debug, Clone)]
pub struct SessionMeta {
    pub composer_mode: Option<String>,
    pub permission_mode: Option<String>,
    pub prompt_category: Option<String>,
    pub user_email: Option<String>,
    pub duration_ms: Option<i64>,
    pub model: Option<String>,
    pub repo_id: Option<String>,
    pub git_branch: Option<String>,
}

/// Query tool usage stats from hook_events.
pub fn query_tool_stats(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    limit: usize,
) -> Result<Vec<ToolStats>> {
    let mut conditions = vec![
        "event = 'post_tool_use'".to_string(),
        "tool_name IS NOT NULL".to_string(),
        "tool_name NOT LIKE 'mcp_%'".to_string(),
    ];
    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    let mut idx = 1;

    if let Some(s) = since {
        conditions.push(format!("timestamp >= ?{idx}"));
        param_values.push(Box::new(s.to_string()));
        idx += 1;
    }
    if let Some(u) = until {
        conditions.push(format!("timestamp < ?{idx}"));
        param_values.push(Box::new(u.to_string()));
        idx += 1;
    }

    let where_clause = format!("WHERE {}", conditions.join(" AND "));

    let sql = format!(
        "SELECT tool_name, provider, COUNT(*) as call_count,
                AVG(tool_duration_ms) as avg_duration_ms,
                SUM(tool_duration_ms) as total_duration_ms
         FROM hook_events
         {where_clause}
         GROUP BY tool_name, provider
         ORDER BY call_count DESC
         LIMIT ?{idx}",
    );
    param_values.push(Box::new(limit as i64));

    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        param_values.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(ToolStats {
                tool_name: row.get(0)?,
                provider: row.get(1)?,
                call_count: row.get(2)?,
                avg_duration_ms: row.get(3)?,
                total_duration_ms: row.get(4)?,
            })
        })?
        .filter_map(|r| {
            r.inspect_err(|e| tracing::warn!("Failed to map tool stats row: {e}"))
                .ok()
        })
        .collect();

    Ok(rows)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolStats {
    pub tool_name: String,
    pub provider: String,
    pub call_count: i64,
    pub avg_duration_ms: Option<f64>,
    pub total_duration_ms: Option<i64>,
}

/// Query MCP server usage stats from hook_events.
pub fn query_mcp_stats(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    limit: usize,
) -> Result<Vec<McpStats>> {
    let mut conditions = vec!["mcp_server IS NOT NULL".to_string()];
    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    let mut idx = 1;

    if let Some(s) = since {
        conditions.push(format!("timestamp >= ?{idx}"));
        param_values.push(Box::new(s.to_string()));
        idx += 1;
    }
    if let Some(u) = until {
        conditions.push(format!("timestamp < ?{idx}"));
        param_values.push(Box::new(u.to_string()));
        idx += 1;
    }

    let where_clause = format!("WHERE {}", conditions.join(" AND "));

    let sql = format!(
        "SELECT tool_name, mcp_server, COUNT(*) as call_count,
                AVG(tool_duration_ms) as avg_duration_ms,
                SUM(tool_duration_ms) as total_duration_ms
         FROM hook_events
         {where_clause}
         GROUP BY tool_name, mcp_server
         ORDER BY call_count DESC
         LIMIT ?{idx}",
    );
    param_values.push(Box::new(limit as i64));

    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        param_values.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(McpStats {
                tool_name: row.get(0)?,
                mcp_server: row.get(1)?,
                call_count: row.get(2)?,
                avg_duration_ms: row.get(3)?,
                total_duration_ms: row.get(4)?,
            })
        })?
        .filter_map(|r| {
            r.inspect_err(|e| tracing::warn!("Failed to map MCP stats row: {e}"))
                .ok()
        })
        .collect();

    Ok(rows)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct McpStats {
    pub tool_name: String,
    pub mcp_server: String,
    pub call_count: i64,
    pub avg_duration_ms: Option<f64>,
    pub total_duration_ms: Option<i64>,
}

// ---------------------------------------------------------------------------
// Event name normalization
// ---------------------------------------------------------------------------

/// Normalize event names from both CC (PascalCase) and Cursor (camelCase) to snake_case.
fn normalize_event_name(name: &str) -> String {
    match name {
        // Claude Code PascalCase
        "SessionStart" => "session_start",
        "SessionEnd" => "session_end",
        "PreToolUse" => "pre_tool_use",
        "PostToolUse" => "post_tool_use",
        "PostToolUseFailure" => "post_tool_use_failure",
        "SubagentStart" => "subagent_start",
        "SubagentStop" => "subagent_stop",
        "PreCompact" => "pre_compact",
        "Stop" => "stop",
        "UserPromptSubmit" => "user_prompt_submit",
        "Notification" => "notification",
        "PermissionRequest" => "permission_request",
        // Cursor camelCase
        "sessionStart" => "session_start",
        "sessionEnd" => "session_end",
        "preToolUse" => "pre_tool_use",
        "postToolUse" => "post_tool_use",
        "postToolUseFailure" => "post_tool_use_failure",
        "subagentStart" => "subagent_start",
        "subagentStop" => "subagent_stop",
        "preCompact" => "pre_compact",
        "stop" => "stop",
        "beforeSubmitPrompt" => "user_prompt_submit",
        "afterShellExecution" => "after_shell_execution",
        "afterFileEdit" => "after_file_edit",
        "afterAgentResponse" => "after_agent_response",
        "beforeShellExecution" => "before_shell_execution",
        _ => {
            tracing::debug!("Unknown hook event name: {}", name);
            name
        }
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_cc_events() {
        assert_eq!(normalize_event_name("SessionStart"), "session_start");
        assert_eq!(normalize_event_name("PostToolUse"), "post_tool_use");
        assert_eq!(
            normalize_event_name("UserPromptSubmit"),
            "user_prompt_submit"
        );
    }

    #[test]
    fn normalize_cursor_events() {
        assert_eq!(normalize_event_name("sessionStart"), "session_start");
        assert_eq!(normalize_event_name("postToolUse"), "post_tool_use");
        assert_eq!(
            normalize_event_name("beforeSubmitPrompt"),
            "user_prompt_submit"
        );
    }

    #[test]
    fn parse_claude_code_session_start() {
        let json: Value = serde_json::from_str(
            r#"{
            "session_id": "abc-123",
            "hook_event_name": "SessionStart",
            "cwd": "/Users/test/project",
            "permission_mode": "default",
            "model": "claude-opus-4-6"
        }"#,
        )
        .unwrap();

        let event = parse_hook_event(&json).unwrap();
        assert_eq!(event.provider, "claude_code");
        assert_eq!(event.event, "session_start");
        assert_eq!(event.session_id.as_deref(), Some("abc-123"));
        assert_eq!(event.permission_mode.as_deref(), Some("default"));
        assert_eq!(event.model.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(event.workspace_root.as_deref(), Some("/Users/test/project"));
    }

    #[test]
    fn parse_cursor_session_start() {
        let json: Value = serde_json::from_str(
            r#"{
            "conversation_id": "conv-456",
            "hook_event_name": "sessionStart",
            "cursor_version": "1.7.0",
            "workspace_roots": ["/Users/test/project"],
            "user_email": "test@example.com",
            "composer_mode": "agent",
            "model": "claude-3-5-sonnet"
        }"#,
        )
        .unwrap();

        let event = parse_hook_event(&json).unwrap();
        assert_eq!(event.provider, "cursor");
        assert_eq!(event.event, "session_start");
        assert_eq!(event.session_id.as_deref(), Some("conv-456"));
        assert_eq!(event.composer_mode.as_deref(), Some("agent"));
        assert_eq!(event.user_email.as_deref(), Some("test@example.com"));
        assert_eq!(event.workspace_root.as_deref(), Some("/Users/test/project"));
    }

    #[test]
    fn parse_cursor_session_id_strips_provider_prefix_for_uuid() {
        let json: Value = serde_json::from_str(
            r#"{
            "conversation_id": "cursor-d99dfe22-d05c-4c78-8698-015d06e5dabb",
            "hook_event_name": "sessionStart",
            "cursor_version": "1.7.0"
        }"#,
        )
        .unwrap();

        let event = parse_hook_event(&json).unwrap();
        assert_eq!(
            event.session_id.as_deref(),
            Some("d99dfe22-d05c-4c78-8698-015d06e5dabb")
        );
    }

    #[test]
    fn parse_post_tool_use() {
        let json: Value = serde_json::from_str(
            r#"{
            "session_id": "abc-123",
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "duration": 1500,
            "model": "claude-opus-4-6"
        }"#,
        )
        .unwrap();

        let event = parse_hook_event(&json).unwrap();
        assert_eq!(event.event, "post_tool_use");
        assert_eq!(event.tool_name.as_deref(), Some("Bash"));
        assert_eq!(event.tool_duration_ms, Some(1500));
    }

    #[test]
    fn parse_pre_compact() {
        let json: Value = serde_json::from_str(
            r#"{
            "session_id": "abc-123",
            "hook_event_name": "PreCompact",
            "context_tokens": 150000,
            "context_window_size": 200000,
            "context_usage_percent": 75.0,
            "message_count": 42
        }"#,
        )
        .unwrap();

        let event = parse_hook_event(&json).unwrap();
        assert_eq!(event.event, "pre_compact");
        // context_tokens, context_window_size, context_usage_pct, message_count
        // are no longer stored as fields — they remain in raw_json if needed
        assert_eq!(event.session_id.as_deref(), Some("abc-123"));
    }

    #[test]
    fn parse_session_end() {
        let json: Value = serde_json::from_str(
            r#"{
            "session_id": "abc-123",
            "hook_event_name": "SessionEnd",
            "reason": "completed",
            "duration_ms": 300000
        }"#,
        )
        .unwrap();

        let event = parse_hook_event(&json).unwrap();
        assert_eq!(event.event, "session_end");
        assert_eq!(event.end_reason.as_deref(), Some("completed"));
        assert_eq!(event.duration_ms, Some(300000));
    }

    #[test]
    fn parse_mcp_tool_extracts_server() {
        let json: Value = serde_json::from_str(
            r#"{
            "session_id": "abc-123",
            "hook_event_name": "PostToolUse",
            "tool_name": "mcp__memory__create_entities",
            "duration": 500
        }"#,
        )
        .unwrap();

        let event = parse_hook_event(&json).unwrap();
        assert_eq!(event.mcp_server.as_deref(), Some("memory"));
        assert_eq!(
            event.tool_name.as_deref(),
            Some("mcp__memory__create_entities")
        );
    }

    #[test]
    fn parse_non_mcp_tool_no_server() {
        let json: Value = serde_json::from_str(
            r#"{
            "session_id": "abc-123",
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "duration": 100
        }"#,
        )
        .unwrap();

        let event = parse_hook_event(&json).unwrap();
        assert!(event.mcp_server.is_none());
    }

    #[test]
    fn parse_hook_extracts_message_and_tool_use_ids() {
        let json: Value = serde_json::from_str(
            r#"{
            "session_id": "abc-123",
            "hook_event_name": "PostToolUse",
            "message_id": "msg_123",
            "tool_use": {"id": "toolu_456"},
            "tool_name": "Read"
        }"#,
        )
        .unwrap();

        let event = parse_hook_event(&json).unwrap();
        assert_eq!(event.message_request_id.as_deref(), Some("msg_123"));
        assert_eq!(event.tool_use_id.as_deref(), Some("toolu_456"));
        assert_eq!(event.link_confidence.as_deref(), Some(HOOK_LINK_UNLINKED));
    }

    #[test]
    fn resolve_hook_message_link_prefers_request_id() {
        let conn = Connection::open_in_memory().unwrap();
        crate::migration::migrate(&conn).unwrap();
        conn.execute(
            "INSERT INTO messages (uuid, session_id, role, timestamp, request_id, provider)
             VALUES ('m-req', 'sess-1', 'assistant', '2026-03-25T00:00:01Z', 'msg_123', 'claude_code')",
            [],
        )
        .unwrap();

        let (uuid, confidence) =
            resolve_hook_message_link(&conn, Some("sess-1"), Some("msg_123"), Some("toolu_unused"))
                .unwrap();
        assert_eq!(uuid.as_deref(), Some("m-req"));
        assert_eq!(confidence, HOOK_LINK_EXACT_REQUEST_ID);
    }

    #[test]
    fn resolve_hook_message_link_falls_back_to_tool_use_id() {
        let conn = Connection::open_in_memory().unwrap();
        crate::migration::migrate(&conn).unwrap();
        conn.execute(
            "INSERT INTO messages (uuid, session_id, role, timestamp, provider)
             VALUES ('m-tool', 'sess-2', 'assistant', '2026-03-25T00:00:02Z', 'claude_code')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (message_uuid, key, value)
             VALUES ('m-tool', 'tool_use_id', 'toolu_456')",
            [],
        )
        .unwrap();

        let (uuid, confidence) = resolve_hook_message_link(
            &conn,
            Some("sess-2"),
            Some("missing_req"),
            Some("toolu_456"),
        )
        .unwrap();
        assert_eq!(uuid.as_deref(), Some("m-tool"));
        assert_eq!(confidence, HOOK_LINK_EXACT_TOOL_USE_ID);
    }

    #[test]
    fn resolve_hook_message_link_unlinked_when_no_deterministic_match() {
        let conn = Connection::open_in_memory().unwrap();
        crate::migration::migrate(&conn).unwrap();
        let (uuid, confidence) =
            resolve_hook_message_link(&conn, Some("sess-3"), Some("req_x"), Some("tool_x"))
                .unwrap();
        assert!(uuid.is_none());
        assert_eq!(confidence, HOOK_LINK_UNLINKED);
    }

    #[test]
    fn classify_bugfix() {
        assert_eq!(
            classify_prompt("fix the login bug"),
            Some("bugfix".to_string())
        );
        assert_eq!(
            classify_prompt("debug this error"),
            Some("bugfix".to_string())
        );
        assert_eq!(
            classify_prompt("this is broken"),
            Some("bugfix".to_string())
        );
        assert_eq!(
            classify_prompt("the build doesn't work"),
            Some("bugfix".to_string())
        );
        assert_eq!(
            classify_prompt("there is a regression in the pipeline"),
            Some("bugfix".to_string())
        );
    }

    #[test]
    fn classify_feature() {
        assert_eq!(
            classify_prompt("add a new button to the dashboard"),
            Some("feature".to_string())
        );
        assert_eq!(
            classify_prompt("implement pagination"),
            Some("feature".to_string())
        );
        assert_eq!(
            classify_prompt("create a new endpoint"),
            Some("feature".to_string())
        );
        assert_eq!(
            classify_prompt("make the header sticky"),
            Some("feature".to_string())
        );
        assert_eq!(
            classify_prompt("change the background color"),
            Some("feature".to_string())
        );
        assert_eq!(
            classify_prompt("update the sidebar layout"),
            Some("feature".to_string())
        );
    }

    #[test]
    fn classify_refactor() {
        assert_eq!(
            classify_prompt("refactor the auth module"),
            Some("refactor".to_string())
        );
        assert_eq!(
            classify_prompt("rename this function"),
            Some("refactor".to_string())
        );
        assert_eq!(
            classify_prompt("extract this into a helper"),
            Some("refactor".to_string())
        );
        assert_eq!(
            classify_prompt("clean up the utils module"),
            Some("refactor".to_string())
        );
        assert_eq!(
            classify_prompt("remove the unused import"),
            Some("refactor".to_string())
        );
        assert_eq!(
            classify_prompt("move this to a separate file"),
            Some("refactor".to_string())
        );
        assert_eq!(
            classify_prompt("split the component into smaller parts"),
            Some("refactor".to_string())
        );
    }

    #[test]
    fn classify_testing() {
        assert_eq!(
            classify_prompt("add tests for the parser"),
            Some("testing".to_string())
        );
        assert_eq!(
            classify_prompt("write a unit test for this function"),
            Some("testing".to_string())
        );
        assert_eq!(
            classify_prompt("run the e2e tests"),
            Some("testing".to_string())
        );
        assert_eq!(
            classify_prompt("increase coverage for the hooks module"),
            Some("testing".to_string())
        );
    }

    #[test]
    fn classify_question() {
        assert_eq!(
            classify_prompt("how does this work?"),
            Some("question".to_string())
        );
        assert_eq!(
            classify_prompt("explain the pipeline"),
            Some("question".to_string())
        );
        assert_eq!(
            classify_prompt("is this correct?"),
            Some("question".to_string())
        );
        assert_eq!(
            classify_prompt("where is the config file?"),
            Some("question".to_string())
        );
        assert_eq!(
            classify_prompt("what happens when the session ends"),
            Some("question".to_string())
        );
        assert_eq!(
            classify_prompt("tell me about the architecture"),
            Some("question".to_string())
        );
    }

    #[test]
    fn classify_ops() {
        assert_eq!(
            classify_prompt("deploy to production"),
            Some("ops".to_string())
        );
        assert_eq!(
            classify_prompt("upgrade the dependency"),
            Some("ops".to_string())
        );
        assert_eq!(
            classify_prompt("set up the kubernetes cluster"),
            Some("ops".to_string())
        );
        assert_eq!(
            classify_prompt("revert the last commit"),
            Some("ops".to_string())
        );
    }

    #[test]
    fn classify_review() {
        assert_eq!(classify_prompt("review the PR"), Some("review".to_string()));
        assert_eq!(
            classify_prompt("audit the codebase"),
            Some("review".to_string())
        );
        assert_eq!(
            classify_prompt("validate the endpoint responses"),
            Some("review".to_string())
        );
        assert_eq!(
            classify_prompt("analyze the performance results"),
            Some("review".to_string())
        );
    }

    #[test]
    fn classify_writing() {
        assert_eq!(
            classify_prompt("draft the article"),
            Some("writing".to_string())
        );
        assert_eq!(
            classify_prompt("write documentation for the API"),
            Some("writing".to_string())
        );
    }

    #[test]
    fn classify_plan() {
        assert_eq!(
            classify_prompt("read and implement the plan ~/.claude/plans/foo.md"),
            Some("feature".to_string())
        );
    }

    #[test]
    fn classify_unknown() {
        assert_eq!(classify_prompt("hi"), None);
        assert_eq!(classify_prompt("thanks"), None);
        assert_eq!(classify_prompt("<command>/clear</command>"), None);
        assert_eq!(classify_prompt("/exit"), None);
        assert_eq!(classify_prompt("lgtm"), None);
        assert_eq!(classify_prompt("ok cool"), None);
    }

    #[test]
    fn upsert_session_roundtrip() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                session_id TEXT PRIMARY KEY,
                provider TEXT NOT NULL DEFAULT 'claude_code',
                started_at TEXT, ended_at TEXT, duration_ms INTEGER,
                composer_mode TEXT, permission_mode TEXT, user_email TEXT,
                workspace_root TEXT, end_reason TEXT, prompt_category TEXT,
                model TEXT, raw_json TEXT, repo_id TEXT, git_branch TEXT
            );",
        )
        .unwrap();

        // Session start
        let start_event = HookEvent {
            provider: "claude_code".to_string(),
            event: "session_start".to_string(),
            session_id: Some("sess-1".to_string()),
            timestamp: Utc::now(),
            model: Some("claude-opus-4-6".to_string()),
            permission_mode: Some("auto".to_string()),
            workspace_root: Some("/tmp".to_string()),
            duration_ms: None,
            composer_mode: None,
            user_email: None,
            end_reason: None,
            tool_name: None,
            tool_duration_ms: None,
            tool_call_count: None,
            repo_id: None,
            git_branch: None,
            mcp_server: None,
            message_id: None,
            message_request_id: None,
            tool_use_id: None,
            link_confidence: Some(HOOK_LINK_UNLINKED.to_string()),
            raw_json: "{}".to_string(),
        };
        upsert_session(&conn, &start_event).unwrap();

        // Verify session created
        let mode: String = conn
            .query_row(
                "SELECT permission_mode FROM sessions WHERE session_id = 'sess-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(mode, "auto");

        // Session end
        let end_event = HookEvent {
            event: "session_end".to_string(),
            session_id: Some("sess-1".to_string()),
            end_reason: Some("completed".to_string()),
            duration_ms: Some(60000),
            ..start_event
        };
        upsert_session(&conn, &end_event).unwrap();

        let (reason, dur): (String, i64) = conn
            .query_row(
                "SELECT end_reason, duration_ms FROM sessions WHERE session_id = 'sess-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(reason, "completed");
        assert_eq!(dur, 60000);
    }

    #[test]
    fn ingest_hook_event_dedups_duplicate_post_tool_use_by_tool_use_id() {
        let conn = Connection::open_in_memory().unwrap();
        crate::migration::migrate(&conn).unwrap();

        let event = HookEvent {
            provider: "claude_code".to_string(),
            event: "post_tool_use".to_string(),
            session_id: Some("sess-1".to_string()),
            timestamp: Utc::now(),
            model: Some("claude-sonnet-4-6".to_string()),
            duration_ms: None,
            composer_mode: None,
            permission_mode: None,
            workspace_root: None,
            user_email: None,
            end_reason: None,
            tool_name: Some("Read".to_string()),
            tool_duration_ms: Some(10),
            tool_call_count: None,
            repo_id: None,
            git_branch: None,
            mcp_server: None,
            message_id: None,
            message_request_id: None,
            tool_use_id: Some("toolu_dup_1".to_string()),
            link_confidence: Some(HOOK_LINK_UNLINKED.to_string()),
            raw_json: "{}".to_string(),
        };

        ingest_hook_event(&conn, &event).unwrap();
        ingest_hook_event(&conn, &event).unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM hook_events WHERE session_id='sess-1' AND tool_use_id='toolu_dup_1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }
}
