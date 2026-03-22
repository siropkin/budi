//! Cursor provider — implements the Provider trait for Cursor AI editor.
//!
//! Cursor stores agent transcripts as JSONL files under
//! `~/.cursor/projects/*/agent-transcripts/*.jsonl`. Each line is a JSON
//! object with a `role` field ("user" or "assistant") and associated metadata.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
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
        _path: &Path,
        content: &str,
        offset: usize,
    ) -> Result<(Vec<ParsedMessage>, usize)> {
        Ok(parse_cursor_transcript(content, offset))
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
fn collect_cursor_transcripts(projects_dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(projects_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let transcripts_dir = entry.path().join("agent-transcripts");
        if transcripts_dir.is_dir() {
            let Ok(inner) = std::fs::read_dir(&transcripts_dir) else {
                continue;
            };
            for inner_entry in inner.flatten() {
                let path = inner_entry.path();
                if path.extension().is_some_and(|e| e == "jsonl") {
                    files.push(path);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Cursor JSONL parsing
// ---------------------------------------------------------------------------

/// A Cursor transcript entry — lenient deserialization.
#[derive(Debug, Deserialize)]
struct CursorEntry {
    role: Option<String>,
    #[serde(rename = "type")]
    entry_type: Option<String>,
    model: Option<String>,
    timestamp: Option<String>,
    content: Option<CursorContent>,
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
    // Also accept snake_case variants
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

/// Parse a single Cursor JSONL line into a `ParsedMessage`, if relevant.
fn parse_cursor_line(line: &str) -> Option<ParsedMessage> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let entry: CursorEntry = serde_json::from_str(line).ok()?;

    // Determine role from the `role` field or `type` field.
    let role = entry
        .role
        .as_deref()
        .or(entry.entry_type.as_deref())?;

    match role {
        "user" | "human" => {
            let text_length = entry.content.as_ref().map(|c| c.text_length()).unwrap_or(0);
            let timestamp = parse_timestamp(entry.timestamp.as_deref())?;
            let uuid = entry
                .uuid
                .or(entry.request_id)
                .unwrap_or_else(|| format!("cursor-{}", timestamp.timestamp_millis()));
            Some(ParsedMessage {
                uuid,
                session_id: entry.session_id,
                timestamp,
                cwd: entry.cwd,
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
            })
        }
        "assistant" | "ai" | "model" => {
            let timestamp = parse_timestamp(entry.timestamp.as_deref())?;
            let uuid = entry
                .uuid
                .or(entry.request_id)
                .unwrap_or_else(|| format!("cursor-{}", timestamp.timestamp_millis()));
            let usage = entry.usage.as_ref();
            let tool_names: Vec<String> = entry
                .tool_calls
                .unwrap_or_default()
                .into_iter()
                .filter_map(|tc| tc.name.or(tc.call_type))
                .collect();
            let text_length = entry.content.as_ref().map(|c| c.text_length()).unwrap_or(0);
            Some(ParsedMessage {
                uuid,
                session_id: entry.session_id,
                timestamp,
                cwd: entry.cwd,
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
pub fn parse_cursor_transcript(content: &str, start_offset: usize) -> (Vec<ParsedMessage>, usize) {
    let mut messages = Vec::new();
    let mut offset = start_offset;

    for line in content[start_offset..].lines() {
        let line_end = offset + line.len() + 1; // +1 for newline
        if let Some(msg) = parse_cursor_line(line) {
            messages.push(msg);
        }
        offset = line_end;
    }

    (messages, offset)
}

/// Try parsing a timestamp string — supports ISO 8601 and Unix millis.
fn parse_timestamp(ts: Option<&str>) -> Option<DateTime<Utc>> {
    let ts = ts?;
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

    #[test]
    fn parse_cursor_user_message() {
        let line = r#"{"role":"user","content":"fix the bug","uuid":"cu-123","timestamp":"2026-03-20T10:00:00.000Z","sessionId":"cs-1","cwd":"/projects/myapp"}"#;
        let msg = parse_cursor_line(line).unwrap();
        assert_eq!(msg.role, "user");
        assert_eq!(msg.uuid, "cu-123");
        assert_eq!(msg.session_id.as_deref(), Some("cs-1"));
        assert_eq!(msg.provider, "cursor");
        assert_eq!(msg.text_length, 11);
    }

    #[test]
    fn parse_cursor_assistant_message() {
        let line = r#"{"role":"assistant","model":"gpt-4o","content":"Here's the fix","uuid":"ca-456","timestamp":"2026-03-20T10:01:00.000Z","sessionId":"cs-1","usage":{"input_tokens":500,"output_tokens":200},"toolCalls":[{"name":"edit_file"}],"stopReason":"end_turn"}"#;
        let msg = parse_cursor_line(line).unwrap();
        assert_eq!(msg.role, "assistant");
        assert_eq!(msg.model.as_deref(), Some("gpt-4o"));
        assert_eq!(msg.input_tokens, 500);
        assert_eq!(msg.output_tokens, 200);
        assert_eq!(msg.tool_names, vec!["edit_file"]);
        assert_eq!(msg.provider, "cursor");
    }

    #[test]
    fn parse_cursor_assistant_with_cache_tokens() {
        let line = r#"{"role":"assistant","model":"claude-sonnet-4-6","content":"Done","uuid":"ca-789","timestamp":"2026-03-20T10:02:00.000Z","sessionId":"cs-1","usage":{"input_tokens":100,"output_tokens":50,"cacheCreationInputTokens":200,"cacheReadInputTokens":300}}"#;
        let msg = parse_cursor_line(line).unwrap();
        assert_eq!(msg.cache_creation_tokens, 200);
        assert_eq!(msg.cache_read_tokens, 300);
    }

    #[test]
    fn parse_cursor_unix_timestamp() {
        let line = r#"{"role":"user","content":"hi","uuid":"cu-ts","timestamp":"1711000000000","sessionId":"cs-2"}"#;
        let msg = parse_cursor_line(line).unwrap();
        assert_eq!(msg.role, "user");
    }

    #[test]
    fn skip_unknown_roles() {
        let line = r#"{"role":"system","content":"You are helpful","timestamp":"2026-03-20T10:00:00.000Z"}"#;
        assert!(parse_cursor_line(line).is_none());
    }

    #[test]
    fn skip_empty_and_whitespace() {
        assert!(parse_cursor_line("").is_none());
        assert!(parse_cursor_line("  ").is_none());
    }

    #[test]
    fn parse_cursor_transcript_incremental() {
        let content = concat!(
            r#"{"role":"user","content":"hello","uuid":"u1","timestamp":"2026-03-20T10:00:00.000Z","sessionId":"s1"}"#,
            "\n",
            r#"{"role":"assistant","model":"gpt-4o","content":"hi","uuid":"a1","timestamp":"2026-03-20T10:01:00.000Z","sessionId":"s1","usage":{"input_tokens":10,"output_tokens":5}}"#,
            "\n",
        );

        let (msgs, offset) = parse_cursor_transcript(content, 0);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[0].provider, "cursor");
        assert_eq!(msgs[1].role, "assistant");
        assert_eq!(msgs[1].provider, "cursor");

        // Incremental: no new lines from end offset
        let (msgs2, _) = parse_cursor_transcript(content, offset);
        assert!(msgs2.is_empty());
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
