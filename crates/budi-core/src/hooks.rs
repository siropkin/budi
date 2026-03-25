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
    pub conversation_id: Option<String>,
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
    // Context pressure
    pub context_tokens: Option<i64>,
    pub context_window_size: Option<i64>,
    pub context_usage_pct: Option<f64>,
    pub message_count: Option<i64>,
    // Subagent
    pub subagent_type: Option<String>,
    pub tool_call_count: Option<i64>,
    pub loop_count: Option<i64>,
    // Files
    pub files_json: Option<String>,
    // Resolved from workspace
    pub repo_id: Option<String>,
    pub git_branch: Option<String>,
    // MCP
    pub mcp_server: Option<String>,
    // Raw
    pub raw_json: String,
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

    // Conversation ID: CC uses session_id, Cursor uses conversation_id
    let conversation_id = json
        .get("session_id")
        .or_else(|| json.get("conversation_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let model = json.get("model").and_then(|v| v.as_str()).map(|s| s.to_string());

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

    // Context pressure (from preCompact)
    let context_tokens = json.get("context_tokens").and_then(|v| v.as_i64());
    let context_window_size = json.get("context_window_size").and_then(|v| v.as_i64());
    let context_usage_pct = json
        .get("context_usage_percent")
        .or_else(|| json.get("context_usage_pct"))
        .and_then(|v| v.as_f64());
    let message_count = json.get("message_count").and_then(|v| v.as_i64());

    // Subagent fields
    let subagent_type = json
        .get("subagent_type")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let tool_call_count = json.get("tool_call_count").and_then(|v| v.as_i64());
    let loop_count = json.get("loop_count").and_then(|v| v.as_i64());

    // Files modified
    let files_json = json
        .get("modified_files")
        .and_then(|v| {
            if v.is_array() {
                Some(v.to_string())
            } else {
                None
            }
        })
        .or_else(|| {
            // afterFileEdit: extract file_path
            json.get("file_path")
                .and_then(|v| v.as_str())
                .map(|p| format!("[\"{p}\"]"))
        });

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

    Ok(HookEvent {
        provider: provider.to_string(),
        event,
        conversation_id,
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
        context_tokens,
        context_window_size,
        context_usage_pct,
        message_count,
        subagent_type,
        tool_call_count,
        loop_count,
        files_json,
        repo_id,
        git_branch,
        mcp_server,
        raw_json,
    })
}

/// Insert a hook event into the `hook_events` table.
pub fn ingest_hook_event(conn: &Connection, event: &HookEvent) -> Result<()> {
    conn.execute(
        "INSERT INTO hook_events (
            provider, event, conversation_id, timestamp, model,
            tool_name, tool_duration_ms,
            context_tokens, context_window_size, context_usage_pct, message_count,
            subagent_type, tool_call_count, loop_count,
            files_json, raw_json, mcp_server
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
        params![
            event.provider,
            event.event,
            event.conversation_id,
            event.timestamp.to_rfc3339(),
            event.model,
            event.tool_name,
            event.tool_duration_ms,
            event.context_tokens,
            event.context_window_size,
            event.context_usage_pct,
            event.message_count,
            event.subagent_type,
            event.tool_call_count,
            event.loop_count,
            event.files_json,
            event.raw_json,
            event.mcp_server,
        ],
    )
    .context("Failed to insert hook event")?;
    Ok(())
}

/// Upsert a session record based on a hook event.
/// - On session_start: INSERT new session with metadata.
/// - On session_end: UPDATE ended_at, duration_ms, end_reason.
/// - On other events: UPDATE model if present and not already set.
pub fn upsert_session(conn: &Connection, event: &HookEvent) -> Result<()> {
    let Some(ref conv_id) = event.conversation_id else {
        return Ok(()); // No conversation_id → can't create session
    };

    match event.event.as_str() {
        "session_start" => {
            conn.execute(
                "INSERT OR IGNORE INTO sessions (
                    conversation_id, provider, started_at, composer_mode,
                    permission_mode, user_email, workspace_root, model, raw_json,
                    repo_id, git_branch
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    conv_id,
                    event.provider,
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
                    end_reason = ?4
                WHERE conversation_id = ?1",
                params![
                    conv_id,
                    event.timestamp.to_rfc3339(),
                    event.duration_ms,
                    event.end_reason,
                ],
            )?;
        }
        _ => {
            // Always update model to latest seen (user may switch models mid-session)
            if let Some(ref model) = event.model {
                conn.execute(
                    "UPDATE sessions SET model = ?2
                     WHERE conversation_id = ?1",
                    params![conv_id, model],
                )?;
            }
            // Update user_email if provided and not yet set
            if let Some(ref email) = event.user_email {
                conn.execute(
                    "UPDATE sessions SET user_email = ?2
                     WHERE conversation_id = ?1 AND user_email IS NULL",
                    params![conv_id, email],
                )?;
            }
            // Update repo_id/git_branch if not yet set
            if let Some(ref rid) = event.repo_id {
                conn.execute(
                    "UPDATE sessions SET repo_id = ?2
                     WHERE conversation_id = ?1 AND repo_id IS NULL",
                    params![conv_id, rid],
                )?;
            }
            if let Some(ref branch) = event.git_branch {
                conn.execute(
                    "UPDATE sessions SET git_branch = ?2
                     WHERE conversation_id = ?1 AND git_branch IS NULL",
                    params![conv_id, branch],
                )?;
            }
        }
    }

    Ok(())
}

/// Update a session's prompt_category.
pub fn update_session_category(
    conn: &Connection,
    event: &HookEvent,
    category: &str,
) -> Result<()> {
    if let Some(ref conv_id) = event.conversation_id {
        conn.execute(
            "UPDATE sessions SET prompt_category = ?2
             WHERE conversation_id = ?1 AND prompt_category IS NULL",
            params![conv_id, category],
        )?;
    }
    Ok(())
}

/// Classify a user prompt into a category using keyword heuristics.
/// Returns None if no category matches.
pub fn classify_prompt(text: &str) -> Option<String> {
    let lower = text.to_lowercase();

    // Check in priority order (most specific first)
    let bugfix_words = ["fix", "bug", "broken", "error", "crash", "issue", "debug", "failing", "wrong"];
    let feature_words = ["add", "implement", "create", "build", "new feature", "integrate"];
    let refactor_words = ["refactor", "rename", "move", "clean", "extract", "reorganize", "simplify"];
    let question_words = ["why", "how does", "what is", "explain", "understand"];
    let ops_words = ["deploy", "release", "migrate", "upgrade"];

    if bugfix_words.iter().any(|w| lower.contains(w)) {
        Some("bugfix".to_string())
    } else if refactor_words.iter().any(|w| lower.contains(w)) {
        Some("refactor".to_string())
    } else if ops_words.iter().any(|w| lower.contains(w)) {
        Some("ops".to_string())
    } else if question_words.iter().any(|w| lower.contains(w)) || lower.ends_with('?') {
        Some("question".to_string())
    } else if feature_words.iter().any(|w| lower.contains(w)) {
        Some("feature".to_string())
    } else {
        None
    }
}

/// Query aggregated session metadata for the HookEnricher.
/// Returns a map of conversation_id → SessionMeta.
pub fn load_session_meta(conn: &Connection) -> Result<std::collections::HashMap<String, SessionMeta>> {
    let mut map = std::collections::HashMap::new();

    // Load sessions
    let mut stmt = conn.prepare(
        "SELECT conversation_id, composer_mode, permission_mode, prompt_category,
                user_email, duration_ms, model
         FROM sessions
         WHERE conversation_id IS NOT NULL",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            SessionMeta {
                composer_mode: row.get(1)?,
                permission_mode: row.get(2)?,
                prompt_category: row.get(3)?,
                user_email: row.get(4)?,
                duration_ms: row.get(5)?,
                model: row.get(6)?,
                dominant_tool: None,
            },
        ))
    })?;

    for row in rows {
        if let Ok((id, meta)) = row {
            map.insert(id, meta);
        }
    }

    // Load dominant tool per conversation from hook_events
    let mut tool_stmt = conn.prepare(
        "SELECT conversation_id, tool_name, COUNT(*) as cnt
         FROM hook_events
         WHERE event = 'post_tool_use' AND tool_name IS NOT NULL AND conversation_id IS NOT NULL
         GROUP BY conversation_id, tool_name
         ORDER BY conversation_id, cnt DESC",
    )?;

    let tool_rows = tool_stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
        ))
    })?;

    // For each conversation, the first row (highest count) is the dominant tool
    let mut seen_conversations = std::collections::HashSet::new();
    for row in tool_rows {
        if let Ok((conv_id, tool)) = row {
            if seen_conversations.insert(conv_id.clone()) {
                if let Some(meta) = map.get_mut(&conv_id) {
                    meta.dominant_tool = Some(tool);
                }
            }
        }
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
    pub dominant_tool: Option<String>,
}

/// Query session stats for the /analytics/sessions endpoint.
pub fn query_sessions(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    limit: usize,
) -> Result<Vec<SessionStats>> {
    let mut conditions = Vec::new();
    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    let mut idx = 1;

    if let Some(s) = since {
        conditions.push(format!("s.started_at >= ?{idx}"));
        param_values.push(Box::new(s.to_string()));
        idx += 1;
    }
    if let Some(u) = until {
        conditions.push(format!("s.started_at < ?{idx}"));
        param_values.push(Box::new(u.to_string()));
        idx += 1;
    }

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };

    let sql = format!(
        "SELECT s.conversation_id, s.provider, s.started_at, s.ended_at,
                s.duration_ms, s.composer_mode, s.permission_mode, s.user_email,
                s.end_reason, s.prompt_category, s.model,
                COALESCE(m.msg_count, 0),
                COALESCE(m.total_cost, 0.0)
         FROM sessions s
         LEFT JOIN (
             SELECT session_id, COUNT(*) as msg_count, COALESCE(SUM(cost_cents), 0.0) as total_cost
             FROM messages
             GROUP BY session_id
         ) m ON m.session_id = s.conversation_id
         {where_clause}
         ORDER BY s.started_at DESC
         LIMIT ?{idx}",
    );
    param_values.push(Box::new(limit as i64));

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(SessionStats {
                conversation_id: row.get(0)?,
                provider: row.get(1)?,
                started_at: row.get(2)?,
                ended_at: row.get(3)?,
                duration_ms: row.get(4)?,
                composer_mode: row.get(5)?,
                permission_mode: row.get(6)?,
                user_email: row.get(7)?,
                end_reason: row.get(8)?,
                prompt_category: row.get(9)?,
                model: row.get(10)?,
                message_count: row.get(11)?,
                cost_cents: row.get(12)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(rows)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionStats {
    pub conversation_id: String,
    pub provider: String,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub duration_ms: Option<i64>,
    pub composer_mode: Option<String>,
    pub permission_mode: Option<String>,
    pub user_email: Option<String>,
    pub end_reason: Option<String>,
    pub prompt_category: Option<String>,
    pub model: Option<String>,
    pub message_count: i64,
    pub cost_cents: f64,
}

/// Query tool usage stats from hook_events.
pub fn query_tool_stats(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    limit: usize,
) -> Result<Vec<ToolStats>> {
    let mut conditions = vec!["event = 'post_tool_use'".to_string(), "tool_name IS NOT NULL".to_string()];
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

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values.iter().map(|p| p.as_ref()).collect();
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
        .filter_map(|r| r.ok())
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
        "SELECT mcp_server, COUNT(*) as call_count,
                AVG(tool_duration_ms) as avg_duration_ms,
                SUM(tool_duration_ms) as total_duration_ms
         FROM hook_events
         {where_clause}
         GROUP BY mcp_server
         ORDER BY call_count DESC
         LIMIT ?{idx}",
    );
    param_values.push(Box::new(limit as i64));

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(McpStats {
                mcp_server: row.get(0)?,
                call_count: row.get(1)?,
                avg_duration_ms: row.get(2)?,
                total_duration_ms: row.get(3)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(rows)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct McpStats {
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
        _ => name,
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
        assert_eq!(normalize_event_name("UserPromptSubmit"), "user_prompt_submit");
    }

    #[test]
    fn normalize_cursor_events() {
        assert_eq!(normalize_event_name("sessionStart"), "session_start");
        assert_eq!(normalize_event_name("postToolUse"), "post_tool_use");
        assert_eq!(normalize_event_name("beforeSubmitPrompt"), "user_prompt_submit");
    }

    #[test]
    fn parse_claude_code_session_start() {
        let json: Value = serde_json::from_str(r#"{
            "session_id": "abc-123",
            "hook_event_name": "SessionStart",
            "cwd": "/Users/test/project",
            "permission_mode": "default",
            "model": "claude-opus-4-6"
        }"#).unwrap();

        let event = parse_hook_event(&json).unwrap();
        assert_eq!(event.provider, "claude_code");
        assert_eq!(event.event, "session_start");
        assert_eq!(event.conversation_id.as_deref(), Some("abc-123"));
        assert_eq!(event.permission_mode.as_deref(), Some("default"));
        assert_eq!(event.model.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(event.workspace_root.as_deref(), Some("/Users/test/project"));
    }

    #[test]
    fn parse_cursor_session_start() {
        let json: Value = serde_json::from_str(r#"{
            "conversation_id": "conv-456",
            "hook_event_name": "sessionStart",
            "cursor_version": "1.7.0",
            "workspace_roots": ["/Users/test/project"],
            "user_email": "test@example.com",
            "composer_mode": "agent",
            "model": "claude-3-5-sonnet"
        }"#).unwrap();

        let event = parse_hook_event(&json).unwrap();
        assert_eq!(event.provider, "cursor");
        assert_eq!(event.event, "session_start");
        assert_eq!(event.conversation_id.as_deref(), Some("conv-456"));
        assert_eq!(event.composer_mode.as_deref(), Some("agent"));
        assert_eq!(event.user_email.as_deref(), Some("test@example.com"));
        assert_eq!(event.workspace_root.as_deref(), Some("/Users/test/project"));
    }

    #[test]
    fn parse_post_tool_use() {
        let json: Value = serde_json::from_str(r#"{
            "session_id": "abc-123",
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "duration": 1500,
            "model": "claude-opus-4-6"
        }"#).unwrap();

        let event = parse_hook_event(&json).unwrap();
        assert_eq!(event.event, "post_tool_use");
        assert_eq!(event.tool_name.as_deref(), Some("Bash"));
        assert_eq!(event.tool_duration_ms, Some(1500));
    }

    #[test]
    fn parse_pre_compact() {
        let json: Value = serde_json::from_str(r#"{
            "session_id": "abc-123",
            "hook_event_name": "PreCompact",
            "context_tokens": 150000,
            "context_window_size": 200000,
            "context_usage_percent": 75.0,
            "message_count": 42
        }"#).unwrap();

        let event = parse_hook_event(&json).unwrap();
        assert_eq!(event.event, "pre_compact");
        assert_eq!(event.context_tokens, Some(150000));
        assert_eq!(event.context_window_size, Some(200000));
        assert_eq!(event.context_usage_pct, Some(75.0));
        assert_eq!(event.message_count, Some(42));
    }

    #[test]
    fn parse_session_end() {
        let json: Value = serde_json::from_str(r#"{
            "session_id": "abc-123",
            "hook_event_name": "SessionEnd",
            "reason": "completed",
            "duration_ms": 300000
        }"#).unwrap();

        let event = parse_hook_event(&json).unwrap();
        assert_eq!(event.event, "session_end");
        assert_eq!(event.end_reason.as_deref(), Some("completed"));
        assert_eq!(event.duration_ms, Some(300000));
    }

    #[test]
    fn parse_mcp_tool_extracts_server() {
        let json: Value = serde_json::from_str(r#"{
            "session_id": "abc-123",
            "hook_event_name": "PostToolUse",
            "tool_name": "mcp__memory__create_entities",
            "duration": 500
        }"#).unwrap();

        let event = parse_hook_event(&json).unwrap();
        assert_eq!(event.mcp_server.as_deref(), Some("memory"));
        assert_eq!(event.tool_name.as_deref(), Some("mcp__memory__create_entities"));
    }

    #[test]
    fn parse_non_mcp_tool_no_server() {
        let json: Value = serde_json::from_str(r#"{
            "session_id": "abc-123",
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "duration": 100
        }"#).unwrap();

        let event = parse_hook_event(&json).unwrap();
        assert!(event.mcp_server.is_none());
    }

    #[test]
    fn classify_bugfix() {
        assert_eq!(classify_prompt("fix the login bug"), Some("bugfix".to_string()));
        assert_eq!(classify_prompt("debug this error"), Some("bugfix".to_string()));
        assert_eq!(classify_prompt("this is broken"), Some("bugfix".to_string()));
    }

    #[test]
    fn classify_feature() {
        assert_eq!(classify_prompt("add a new button to the dashboard"), Some("feature".to_string()));
        assert_eq!(classify_prompt("implement pagination"), Some("feature".to_string()));
        assert_eq!(classify_prompt("create a new endpoint"), Some("feature".to_string()));
    }

    #[test]
    fn classify_refactor() {
        assert_eq!(classify_prompt("refactor the auth module"), Some("refactor".to_string()));
        assert_eq!(classify_prompt("rename this function"), Some("refactor".to_string()));
        assert_eq!(classify_prompt("extract this into a helper"), Some("refactor".to_string()));
    }

    #[test]
    fn classify_question() {
        assert_eq!(classify_prompt("how does this work?"), Some("question".to_string()));
        assert_eq!(classify_prompt("explain the pipeline"), Some("question".to_string()));
        assert_eq!(classify_prompt("is this correct?"), Some("question".to_string()));
    }

    #[test]
    fn classify_ops() {
        assert_eq!(classify_prompt("deploy to production"), Some("ops".to_string()));
        assert_eq!(classify_prompt("upgrade the dependency"), Some("ops".to_string()));
    }

    #[test]
    fn classify_unknown() {
        assert_eq!(classify_prompt("hello"), None);
        assert_eq!(classify_prompt("thanks"), None);
    }

    #[test]
    fn upsert_session_roundtrip() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                conversation_id TEXT PRIMARY KEY,
                provider TEXT NOT NULL DEFAULT 'claude_code',
                started_at TEXT, ended_at TEXT, duration_ms INTEGER,
                composer_mode TEXT, permission_mode TEXT, user_email TEXT,
                workspace_root TEXT, end_reason TEXT, prompt_category TEXT,
                model TEXT, raw_json TEXT, repo_id TEXT, git_branch TEXT
            );",
        ).unwrap();

        // Session start
        let start_event = HookEvent {
            provider: "claude_code".to_string(),
            event: "session_start".to_string(),
            conversation_id: Some("sess-1".to_string()),
            timestamp: Utc::now(),
            model: Some("claude-opus-4-6".to_string()),
            permission_mode: Some("auto".to_string()),
            workspace_root: Some("/tmp".to_string()),
            duration_ms: None, composer_mode: None, user_email: None,
            end_reason: None, tool_name: None, tool_duration_ms: None,
            context_tokens: None, context_window_size: None,
            context_usage_pct: None, message_count: None,
            subagent_type: None, tool_call_count: None, loop_count: None,
            files_json: None, repo_id: None, git_branch: None,
            mcp_server: None, raw_json: "{}".to_string(),
        };
        upsert_session(&conn, &start_event).unwrap();

        // Verify session created
        let mode: String = conn
            .query_row("SELECT permission_mode FROM sessions WHERE conversation_id = 'sess-1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "auto");

        // Session end
        let end_event = HookEvent {
            event: "session_end".to_string(),
            conversation_id: Some("sess-1".to_string()),
            end_reason: Some("completed".to_string()),
            duration_ms: Some(60000),
            ..start_event
        };
        upsert_session(&conn, &end_event).unwrap();

        let (reason, dur): (String, i64) = conn
            .query_row(
                "SELECT end_reason, duration_ms FROM sessions WHERE conversation_id = 'sess-1'",
                [], |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(reason, "completed");
        assert_eq!(dur, 60000);
    }
}
