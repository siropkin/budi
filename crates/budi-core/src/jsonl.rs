//! Parser for Claude Code JSONL transcript files.
//!
//! Each line in a transcript is a JSON object with a `type` field.
//! We extract `user` and `assistant` messages for analytics.

use chrono::{DateTime, Utc};
use serde::Deserialize;

/// Top-level entry from a Claude Code JSONL transcript line.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub(crate) enum TranscriptEntry {
    #[serde(rename = "user")]
    User(UserEntry),
    #[serde(rename = "assistant")]
    Assistant(AssistantEntry),
    /// All other line types we don't need for analytics.
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UserEntry {
    pub uuid: String,
    pub session_id: Option<String>,
    pub timestamp: DateTime<Utc>,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub parent_uuid: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AssistantEntry {
    pub uuid: String,
    pub session_id: Option<String>,
    pub timestamp: DateTime<Utc>,
    pub cwd: Option<String>,
    pub message: AssistantMessage,
    pub parent_uuid: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AssistantMessage {
    pub model: Option<String>,
    pub usage: Option<TokenUsage>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TokenUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
}

/// Parsed analytics-relevant data from a single assistant message.
#[derive(Debug)]
pub struct ParsedMessage {
    pub uuid: String,
    pub session_id: Option<String>,
    pub timestamp: DateTime<Utc>,
    pub cwd: Option<String>,
    pub role: String,
    pub model: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub git_branch: Option<String>,
    /// Canonical repository identity, resolved from cwd during sync.
    pub repo_id: Option<String>,
    /// Which provider produced this message (e.g. "claude_code", "cursor").
    pub provider: String,
    /// Provider-reported cost in cents (ground truth from Cursor, None for Claude Code).
    pub cost_cents: Option<f64>,
    /// Human-readable session title (used by enrichers to produce tags, not stored as column).
    pub session_title: Option<String>,
    /// Parent message UUID (for subagent messages).
    pub parent_uuid: Option<String>,
    /// User name (used by enrichers to produce tags, not stored as column).
    pub user_name: Option<String>,
    /// Machine name (used by enrichers to produce tags, not stored as column).
    pub machine_name: Option<String>,
    /// Confidence level: "exact" (tokens from source), "exact_cost" (cost from API, tokens estimated),
    /// "estimated" (both calculated from heuristics).
    pub cost_confidence: String,
}

/// Parse a single JSONL line into a `ParsedMessage`, if relevant.
fn parse_line(line: &str) -> Option<ParsedMessage> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let entry: TranscriptEntry = serde_json::from_str(line).ok()?;
    match entry {
        TranscriptEntry::User(u) => Some(ParsedMessage {
            uuid: u.uuid,
            session_id: u.session_id,
            timestamp: u.timestamp,
            cwd: u.cwd,
            role: "user".to_string(),
            model: None,
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            git_branch: u.git_branch,
            repo_id: None,
            provider: "claude_code".to_string(),
            cost_cents: None,
            session_title: None,
            parent_uuid: u.parent_uuid,
            user_name: None,
            machine_name: None,
            cost_confidence: "estimated".to_string(),
        }),
        TranscriptEntry::Assistant(a) => {
            let usage = a.message.usage.as_ref();
            Some(ParsedMessage {
                uuid: a.uuid,
                session_id: a.session_id,
                timestamp: a.timestamp,
                cwd: a.cwd,
                role: "assistant".to_string(),
                model: a.message.model,
                input_tokens: usage.and_then(|u| u.input_tokens).unwrap_or(0),
                output_tokens: usage.and_then(|u| u.output_tokens).unwrap_or(0),
                cache_creation_tokens: usage
                    .and_then(|u| u.cache_creation_input_tokens)
                    .unwrap_or(0),
                cache_read_tokens: usage.and_then(|u| u.cache_read_input_tokens).unwrap_or(0),
                git_branch: None,
                repo_id: None,
                provider: "claude_code".to_string(),
                cost_cents: None, // Calculated during ingest from tokens × pricing
                session_title: None,
                parent_uuid: a.parent_uuid,
                user_name: None,
                machine_name: None,
                cost_confidence: "estimated".to_string(),
            })
        }
        TranscriptEntry::Other => None,
    }
}

/// Parse all lines from a JSONL string, returning parsed messages and the byte
/// offset of the end of the last successfully parsed line.
pub fn parse_transcript(content: &str, start_offset: usize) -> (Vec<ParsedMessage>, usize) {
    let mut messages = Vec::new();
    let mut offset = start_offset;

    let remaining = &content[start_offset..];
    let mut pos = 0;
    for line in remaining.lines() {
        pos += line.len();
        // Only count the newline if it actually exists (handles files without trailing newline)
        if pos < remaining.len() && remaining.as_bytes()[pos] == b'\n' {
            pos += 1;
        }
        if let Some(msg) = parse_line(line) {
            messages.push(msg);
        }
        offset = start_offset + pos;
    }

    (messages, offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_user_message() {
        let line = r#"{"parentUuid":null,"isSidechain":false,"type":"user","message":{"role":"user","content":"hello world"},"uuid":"abc-123","timestamp":"2026-03-14T18:13:42.614Z","sessionId":"sess-1","cwd":"/tmp","version":"2.1.76","gitBranch":"main"}"#;
        let msg = parse_line(line).unwrap();
        assert_eq!(msg.role, "user");
        assert_eq!(msg.uuid, "abc-123");
        assert_eq!(msg.session_id.as_deref(), Some("sess-1"));
        assert_eq!(msg.git_branch.as_deref(), Some("main"));
    }

    #[test]
    fn parse_assistant_with_usage() {
        let line = r#"{"parentUuid":"abc","isSidechain":false,"type":"assistant","message":{"model":"claude-opus-4-6","id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"Hello!"},{"type":"tool_use","id":"t1","name":"Read","input":{}}],"stop_reason":"tool_use","usage":{"input_tokens":100,"output_tokens":50,"cache_creation_input_tokens":200,"cache_read_input_tokens":300}},"uuid":"def-456","timestamp":"2026-03-14T18:14:10.603Z","sessionId":"sess-1","cwd":"/tmp"}"#;
        let msg = parse_line(line).unwrap();
        assert_eq!(msg.role, "assistant");
        assert_eq!(msg.input_tokens, 100);
        assert_eq!(msg.output_tokens, 50);
        assert_eq!(msg.cache_creation_tokens, 200);
        assert_eq!(msg.cache_read_tokens, 300);
        assert_eq!(msg.model.as_deref(), Some("claude-opus-4-6"));
    }

    #[test]
    fn skip_non_message_types() {
        let line = r#"{"type":"file-history-snapshot","messageId":"abc","snapshot":{}}"#;
        assert!(parse_line(line).is_none());
    }

    #[test]
    fn skip_empty_lines() {
        assert!(parse_line("").is_none());
        assert!(parse_line("  ").is_none());
    }

    #[test]
    fn parse_transcript_incremental() {
        let content = concat!(
            r#"{"type":"file-history-snapshot","messageId":"x","snapshot":{}}"#,
            "\n",
            r#"{"parentUuid":null,"isSidechain":false,"type":"user","message":{"role":"user","content":"hi"},"uuid":"u1","timestamp":"2026-03-14T18:13:42.614Z","sessionId":"s1"}"#,
            "\n",
            r#"{"parentUuid":"u1","isSidechain":false,"type":"assistant","message":{"model":"claude-opus-4-6","id":"m1","type":"message","role":"assistant","content":[{"type":"text","text":"hey"}],"stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":5}},"uuid":"a1","timestamp":"2026-03-14T18:14:00.000Z","sessionId":"s1"}"#,
            "\n",
        );

        let (msgs, offset) = parse_transcript(content, 0);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[1].role, "assistant");

        // Incremental: no new lines from end offset
        let (msgs2, _) = parse_transcript(content, offset);
        assert!(msgs2.is_empty());
    }
}
