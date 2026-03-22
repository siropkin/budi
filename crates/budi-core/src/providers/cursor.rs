//! Cursor provider — implements the Provider trait for Cursor AI editor.
//!
//! Cursor stores agent transcripts as JSONL files under
//! `~/.cursor/projects/*/agent-transcripts/`. Each line is a minimal JSON
//! object with `role` ("user" or "assistant") and `message.content` (structured
//! array of `{type: "text", text: "..."}` blocks). No timestamps, UUIDs,
//! model names, or token usage are recorded — we synthesize these from file
//! metadata and path structure.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use serde::Deserialize;
use serde_json::Value;

use crate::jsonl::ParsedMessage;
use crate::provider::{DiscoveredFile, ModelPricing, Provider};

/// The Cursor provider.
pub struct CursorProvider;

impl Provider for CursorProvider {
    fn name(&self) -> &'static str {
        "cursor"
    }

    fn display_name(&self) -> &'static str {
        "Cursor"
    }

    fn is_available(&self) -> bool {
        cursor_home().map(|p| p.exists()).unwrap_or(false)
    }

    fn discover_files(&self) -> Result<Vec<DiscoveredFile>> {
        let home = cursor_home()?;
        let projects_dir = home.join("projects");
        let mut files = Vec::new();
        collect_cursor_transcripts(&projects_dir, &mut files);
        files.sort();
        Ok(files.into_iter().map(|path| DiscoveredFile { path }).collect())
    }

    fn parse_file(
        &self,
        path: &Path,
        content: &str,
        offset: usize,
    ) -> Result<(Vec<ParsedMessage>, usize)> {
        // Derive session ID from the transcript path.
        // Paths look like: .../agent-transcripts/<uuid>.jsonl
        //              or: .../agent-transcripts/<uuid>/<uuid>.jsonl
        let session_id = session_id_from_path(path);

        // Derive project dir from the project slug in the path.
        // e.g. .../projects/Users-ivan-projects-myapp/... → try to recover original path
        let cwd = cwd_from_path(path);

        // Use file mtime as a fallback timestamp for all messages in the file.
        let file_ts = file_mtime(path);

        Ok(parse_cursor_transcript(
            content, offset, &session_id, cwd.as_deref(), file_ts,
        ))
    }

    fn pricing_for_model(&self, model: &str) -> ModelPricing {
        cursor_pricing_for_model(model)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn cursor_home() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".cursor"))
}

/// Walk `~/.cursor/projects/*/agent-transcripts/` for JSONL files.
/// Handles both flat (`*.jsonl`) and nested (`*/*.jsonl`) layouts.
fn collect_cursor_transcripts(projects_dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(projects_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let transcripts_dir = entry.path().join("agent-transcripts");
        if !transcripts_dir.is_dir() {
            continue;
        }
        let Ok(inner) = std::fs::read_dir(&transcripts_dir) else {
            continue;
        };
        for inner_entry in inner.flatten() {
            let path = inner_entry.path();
            if path.extension().is_some_and(|e| e == "jsonl") {
                files.push(path);
            } else if path.is_dir() {
                // Nested: agent-transcripts/<uuid>/<uuid>.jsonl
                let Ok(nested) = std::fs::read_dir(&path) else {
                    continue;
                };
                for nested_entry in nested.flatten() {
                    let nested_path = nested_entry.path();
                    if nested_path.extension().is_some_and(|e| e == "jsonl") {
                        files.push(nested_path);
                    }
                }
            }
        }
    }
}

/// Extract a session ID from the transcript file path.
fn session_id_from_path(path: &Path) -> String {
    // Try the file stem (without .jsonl extension) as the session UUID.
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(|s| format!("cursor-{}", s))
        .unwrap_or_else(|| "cursor-unknown".to_string())
}

/// Extract the Cursor project slug from the transcript path and use it as cwd.
/// The slug is a hyphenated encoding of the original path, but the encoding is
/// lossy (e.g. dots and hyphens in the original path become hyphens too), so we
/// don't try to reconstruct the original path. We just use the slug directly —
/// the repo_id resolver will handle it from there.
fn cwd_from_path(path: &Path) -> Option<String> {
    let mut current = path;
    loop {
        if let Some(parent) = current.parent() {
            if parent.file_name().is_some_and(|n| n == "agent-transcripts") {
                if let Some(project_dir) = parent.parent() {
                    return project_dir
                        .file_name()
                        .and_then(|n| n.to_str())
                        .map(|_| {
                            // Use the project dir's actual path as cwd so that
                            // the repo_id resolver can look for .git there.
                            project_dir.display().to_string()
                        });
                }
            }
            current = parent;
        } else {
            break;
        }
    }
    None
}

/// Get file modification time as a UTC DateTime.
fn file_mtime(path: &Path) -> DateTime<Utc> {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| {
            let dur = t.duration_since(std::time::UNIX_EPOCH).ok()?;
            Utc.timestamp_opt(dur.as_secs() as i64, 0).single()
        })
        .unwrap_or_else(Utc::now)
}

// ---------------------------------------------------------------------------
// Cursor JSONL parsing
// ---------------------------------------------------------------------------

/// A Cursor transcript entry — real format has only `role` and `message`.
#[derive(Debug, Deserialize)]
struct CursorEntry {
    role: Option<String>,
    message: Option<CursorMessage>,
    // Some entries may have extra fields in newer versions — accept them leniently.
    #[serde(rename = "type")]
    entry_type: Option<String>,
    model: Option<String>,
    timestamp: Option<String>,
    usage: Option<CursorUsage>,
    uuid: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    #[serde(rename = "requestId")]
    request_id: Option<String>,
    cwd: Option<String>,
    #[serde(rename = "toolCalls")]
    tool_calls: Option<Vec<CursorToolCall>>,
    #[serde(rename = "stopReason")]
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CursorMessage {
    content: Option<CursorContent>,
}

/// Content can be a string or structured array.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum CursorContent {
    Text(String),
    Structured(Vec<Value>),
}

impl CursorContent {
    fn text_length(&self) -> usize {
        match self {
            CursorContent::Text(s) => s.len(),
            CursorContent::Structured(parts) => parts
                .iter()
                .filter_map(|p| {
                    p.get("text")
                        .and_then(|t| t.as_str())
                        .map(|s| s.len())
                })
                .sum(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct CursorUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    #[serde(rename = "cacheCreationInputTokens")]
    cache_creation_input_tokens: Option<u64>,
    #[serde(rename = "cacheReadInputTokens")]
    cache_read_input_tokens: Option<u64>,
    #[serde(rename = "cache_creation_input_tokens")]
    cache_creation_input_tokens_alt: Option<u64>,
    #[serde(rename = "cache_read_input_tokens")]
    cache_read_input_tokens_alt: Option<u64>,
}

impl CursorUsage {
    fn cache_creation(&self) -> u64 {
        self.cache_creation_input_tokens
            .or(self.cache_creation_input_tokens_alt)
            .unwrap_or(0)
    }

    fn cache_read(&self) -> u64 {
        self.cache_read_input_tokens
            .or(self.cache_read_input_tokens_alt)
            .unwrap_or(0)
    }
}

#[derive(Debug, Deserialize)]
struct CursorToolCall {
    name: Option<String>,
    #[serde(rename = "type")]
    call_type: Option<String>,
}

/// Parse a single Cursor JSONL line into a `ParsedMessage`.
///
/// The `line_index` is used to generate a unique UUID when none is present.
/// `session_id`, `cwd`, and `fallback_ts` come from the file-level context.
fn parse_cursor_line(
    line: &str,
    line_index: usize,
    session_id: &str,
    cwd: Option<&str>,
    fallback_ts: DateTime<Utc>,
) -> Option<ParsedMessage> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let entry: CursorEntry = serde_json::from_str(line).ok()?;

    // Determine role from `role` field or `type` field.
    let role = entry
        .role
        .as_deref()
        .or(entry.entry_type.as_deref())?;

    // Content can be at top level or nested under `message`.
    let content_ref = entry
        .message
        .as_ref()
        .and_then(|m| m.content.as_ref());

    let text_length = content_ref.map(|c| c.text_length()).unwrap_or(0);

    // Timestamp: try entry-level, then fall back to file mtime.
    let timestamp = entry
        .timestamp
        .as_deref()
        .and_then(parse_timestamp)
        .unwrap_or(fallback_ts);

    // UUID: try entry-level, then synthesize from session + line index.
    let uuid = entry
        .uuid
        .or(entry.request_id)
        .unwrap_or_else(|| format!("{}-{}", session_id, line_index));

    // Session ID: prefer entry-level, fall back to file-derived.
    let msg_session_id = entry.session_id.unwrap_or_else(|| session_id.to_string());

    // CWD: prefer entry-level, fall back to file-derived.
    let msg_cwd = entry.cwd.or_else(|| cwd.map(|s| s.to_string()));

    match role {
        "user" | "human" => Some(ParsedMessage {
            uuid,
            session_id: Some(msg_session_id),
            timestamp,
            cwd: msg_cwd,
            role: "user".to_string(),
            model: None,
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            tool_names: vec![],
            has_thinking: false,
            stop_reason: None,
            text_length,
            version: None,
            git_branch: None,
            repo_id: None,
            provider: "cursor".to_string(),
        }),
        "assistant" | "ai" | "model" => {
            let usage = entry.usage.as_ref();
            let tool_names: Vec<String> = entry
                .tool_calls
                .unwrap_or_default()
                .into_iter()
                .filter_map(|tc| tc.name.or(tc.call_type))
                .collect();
            Some(ParsedMessage {
                uuid,
                session_id: Some(msg_session_id),
                timestamp,
                cwd: msg_cwd,
                role: "assistant".to_string(),
                model: entry.model,
                input_tokens: usage.and_then(|u| u.input_tokens).unwrap_or(0),
                output_tokens: usage.and_then(|u| u.output_tokens).unwrap_or(0),
                cache_creation_tokens: usage.map(|u| u.cache_creation()).unwrap_or(0),
                cache_read_tokens: usage.map(|u| u.cache_read()).unwrap_or(0),
                tool_names,
                has_thinking: false,
                stop_reason: entry.stop_reason,
                text_length,
                version: None,
                git_branch: None,
                repo_id: None,
                provider: "cursor".to_string(),
            })
        }
        _ => None,
    }
}

/// Parse all lines from a Cursor JSONL string with incremental offset support.
pub fn parse_cursor_transcript(
    content: &str,
    start_offset: usize,
    session_id: &str,
    cwd: Option<&str>,
    fallback_ts: DateTime<Utc>,
) -> (Vec<ParsedMessage>, usize) {
    let mut messages = Vec::new();
    let mut offset = start_offset;
    let mut line_index = 0usize;

    // Count lines before start_offset to get correct line_index.
    if start_offset > 0 {
        line_index = content[..start_offset].lines().count();
    }

    for line in content[start_offset..].lines() {
        let line_end = offset + line.len() + 1; // +1 for newline
        if let Some(msg) = parse_cursor_line(line, line_index, session_id, cwd, fallback_ts) {
            messages.push(msg);
        }
        offset = line_end;
        line_index += 1;
    }

    (messages, offset)
}

/// Try parsing a timestamp string — supports ISO 8601 and Unix millis.
fn parse_timestamp(ts: &str) -> Option<DateTime<Utc>> {
    // Try ISO 8601 first
    if let Ok(dt) = ts.parse::<DateTime<Utc>>() {
        return Some(dt);
    }
    // Try Unix milliseconds
    if let Ok(millis) = ts.parse::<i64>() {
        return DateTime::from_timestamp_millis(millis);
    }
    None
}

/// Cursor model pricing lookup.
pub fn cursor_pricing_for_model(model: &str) -> ModelPricing {
    let m = model.to_lowercase();
    if m.contains("gpt-4o-mini") {
        ModelPricing {
            input: 0.15,
            output: 0.60,
            cache_write: 0.15,
            cache_read: 0.075,
        }
    } else if m.contains("gpt-4o") || m.contains("gpt-4-turbo") {
        ModelPricing {
            input: 2.50,
            output: 10.0,
            cache_write: 2.50,
            cache_read: 1.25,
        }
    } else if m.contains("gpt-4") {
        ModelPricing {
            input: 30.0,
            output: 60.0,
            cache_write: 30.0,
            cache_read: 15.0,
        }
    } else if m.contains("o1-mini") || m.contains("o3-mini") {
        ModelPricing {
            input: 1.10,
            output: 4.40,
            cache_write: 1.10,
            cache_read: 0.55,
        }
    } else if m.contains("o1") || m.contains("o3") {
        ModelPricing {
            input: 10.0,
            output: 40.0,
            cache_write: 10.0,
            cache_read: 5.0,
        }
    } else if m.contains("opus") {
        ModelPricing {
            input: 5.0,
            output: 25.0,
            cache_write: 6.25,
            cache_read: 0.50,
        }
    } else if m.contains("sonnet") {
        ModelPricing {
            input: 3.0,
            output: 15.0,
            cache_write: 3.75,
            cache_read: 0.30,
        }
    } else if m.contains("haiku") {
        ModelPricing {
            input: 1.0,
            output: 5.0,
            cache_write: 1.25,
            cache_read: 0.10,
        }
    } else if m.contains("gemini") {
        ModelPricing {
            input: 1.25,
            output: 5.0,
            cache_write: 1.25,
            cache_read: 0.30,
        }
    } else {
        // Unknown model — use GPT-4o pricing as reasonable default for Cursor
        ModelPricing {
            input: 2.50,
            output: 10.0,
            cache_write: 2.50,
            cache_read: 1.25,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Real Cursor format tests (minimal: role + message.content only) ---

    #[test]
    fn parse_real_cursor_user_message() {
        let line = r#"{"role":"user","message":{"content":[{"type":"text","text":"fix the bug in main.rs"}]}}"#;
        let ts = Utc::now();
        let msg = parse_cursor_line(line, 0, "cursor-abc", Some("/proj"), ts).unwrap();
        assert_eq!(msg.role, "user");
        assert_eq!(msg.uuid, "cursor-abc-0");
        assert_eq!(msg.session_id.as_deref(), Some("cursor-abc"));
        assert_eq!(msg.cwd.as_deref(), Some("/proj"));
        assert_eq!(msg.provider, "cursor");
        assert_eq!(msg.text_length, 22);
        assert_eq!(msg.model, None);
        assert_eq!(msg.input_tokens, 0);
    }

    #[test]
    fn parse_real_cursor_assistant_message() {
        let line = r#"{"role":"assistant","message":{"content":[{"type":"text","text":"Here is the fix for main.rs"}]}}"#;
        let ts = Utc::now();
        let msg = parse_cursor_line(line, 1, "cursor-abc", Some("/proj"), ts).unwrap();
        assert_eq!(msg.role, "assistant");
        assert_eq!(msg.uuid, "cursor-abc-1");
        assert_eq!(msg.model, None); // Cursor doesn't log model
        assert_eq!(msg.input_tokens, 0); // Cursor doesn't log tokens
        assert_eq!(msg.text_length, 27);
    }

    #[test]
    fn parse_real_cursor_transcript() {
        let content = concat!(
            r#"{"role":"user","message":{"content":[{"type":"text","text":"hello"}]}}"#,
            "\n",
            r#"{"role":"assistant","message":{"content":[{"type":"text","text":"hi there"}]}}"#,
            "\n",
            r#"{"role":"assistant","message":{"content":[{"type":"text","text":"let me help"}]}}"#,
            "\n",
        );
        let ts = Utc::now();
        let (msgs, offset) = parse_cursor_transcript(content, 0, "cursor-s1", Some("/proj"), ts);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[1].role, "assistant");
        assert_eq!(msgs[2].role, "assistant");
        // All share the same session
        assert!(msgs.iter().all(|m| m.session_id.as_deref() == Some("cursor-s1")));
        // All are cursor provider
        assert!(msgs.iter().all(|m| m.provider == "cursor"));
        // UUIDs are unique
        assert_eq!(msgs[0].uuid, "cursor-s1-0");
        assert_eq!(msgs[1].uuid, "cursor-s1-1");
        assert_eq!(msgs[2].uuid, "cursor-s1-2");

        // Incremental: no new lines
        let (msgs2, _) = parse_cursor_transcript(content, offset, "cursor-s1", Some("/proj"), ts);
        assert!(msgs2.is_empty());
    }

    // --- Extended format tests (if Cursor adds more fields in future) ---

    #[test]
    fn parse_cursor_with_optional_fields() {
        // If Cursor ever adds timestamps, UUIDs, model, usage — we handle them.
        let line = r#"{"role":"assistant","model":"gpt-4o","message":{"content":[{"type":"text","text":"done"}]},"uuid":"ca-456","timestamp":"2026-03-20T10:01:00.000Z","sessionId":"cs-1","usage":{"input_tokens":500,"output_tokens":200},"toolCalls":[{"name":"edit_file"}],"stopReason":"end_turn"}"#;
        let ts = Utc::now();
        let msg = parse_cursor_line(line, 0, "fallback", None, ts).unwrap();
        assert_eq!(msg.uuid, "ca-456"); // Uses entry-level UUID
        assert_eq!(msg.session_id.as_deref(), Some("cs-1")); // Uses entry-level session
        assert_eq!(msg.model.as_deref(), Some("gpt-4o"));
        assert_eq!(msg.input_tokens, 500);
        assert_eq!(msg.output_tokens, 200);
        assert_eq!(msg.tool_names, vec!["edit_file"]);
    }

    #[test]
    fn skip_system_role() {
        let line = r#"{"role":"system","message":{"content":[{"type":"text","text":"You are helpful"}]}}"#;
        let ts = Utc::now();
        assert!(parse_cursor_line(line, 0, "s", None, ts).is_none());
    }

    #[test]
    fn skip_empty_and_whitespace() {
        let ts = Utc::now();
        assert!(parse_cursor_line("", 0, "s", None, ts).is_none());
        assert!(parse_cursor_line("  ", 0, "s", None, ts).is_none());
    }

    #[test]
    fn session_id_from_path_uuid() {
        let path = Path::new("/home/.cursor/projects/proj/agent-transcripts/abc-def-123/abc-def-123.jsonl");
        assert_eq!(session_id_from_path(path), "cursor-abc-def-123");
    }

    #[test]
    fn session_id_from_path_flat() {
        let path = Path::new("/home/.cursor/projects/proj/agent-transcripts/xyz.jsonl");
        assert_eq!(session_id_from_path(path), "cursor-xyz");
    }

    #[test]
    fn cursor_pricing_gpt4o() {
        let p = cursor_pricing_for_model("gpt-4o");
        assert_eq!(p.input, 2.50);
        assert_eq!(p.output, 10.0);
    }

    #[test]
    fn cursor_pricing_sonnet() {
        let p = cursor_pricing_for_model("claude-sonnet-4-6");
        assert_eq!(p.input, 3.0);
        assert_eq!(p.output, 15.0);
    }

    #[test]
    fn cursor_pricing_unknown_defaults_to_gpt4o() {
        let p = cursor_pricing_for_model("some-new-model");
        assert_eq!(p.input, 2.50);
    }
}
