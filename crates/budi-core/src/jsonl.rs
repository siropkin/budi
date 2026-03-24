//! Parser for Claude Code JSONL transcript files.
//!
//! Each line in a transcript is a JSON object with a `type` field.
//! We extract `user` and `assistant` messages for analytics.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

/// Top-level entry from a Claude Code JSONL transcript line.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum TranscriptEntry {
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
pub struct UserEntry {
    pub uuid: String,
    pub session_id: Option<String>,
    pub timestamp: DateTime<Utc>,
    pub cwd: Option<String>,
    pub message: UserMessage,
    pub version: Option<String>,
    pub git_branch: Option<String>,
    pub parent_uuid: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UserMessage {
    pub content: UserContent,
}

/// User content can be a plain string or structured array.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum UserContent {
    Text(String),
    Structured(Vec<Value>),
}

impl UserContent {
    pub fn text_length(&self) -> usize {
        match self {
            UserContent::Text(s) => s.len(),
            UserContent::Structured(parts) => parts
                .iter()
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .map(|s| s.len())
                .sum(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantEntry {
    pub uuid: String,
    pub session_id: Option<String>,
    pub timestamp: DateTime<Utc>,
    pub cwd: Option<String>,
    pub message: AssistantMessage,
    pub parent_uuid: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AssistantMessage {
    pub model: Option<String>,
    pub content: Option<Vec<ContentBlock>>,
    pub usage: Option<TokenUsage>,
    pub stop_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "thinking")]
    Thinking {},
    #[serde(rename = "tool_use")]
    ToolUse { name: String },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
}

impl TokenUsage {
    /// Total billable input tokens (direct + cache creation + cache read).
    pub fn total_input(&self) -> u64 {
        self.input_tokens.unwrap_or(0)
            + self.cache_creation_input_tokens.unwrap_or(0)
            + self.cache_read_input_tokens.unwrap_or(0)
    }
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
    pub tool_names: Vec<String>,
    pub has_thinking: bool,
    pub stop_reason: Option<String>,
    pub text_length: usize,
    pub version: Option<String>,
    pub git_branch: Option<String>,
    /// Canonical repository identity, resolved from cwd during sync.
    pub repo_id: Option<String>,
    /// Which provider produced this message (e.g. "claude_code", "cursor").
    pub provider: String,
    /// Provider-reported cost in cents (ground truth from Cursor, None for Claude Code).
    pub cost_cents: Option<f64>,
    /// Total context tokens used in this request.
    pub context_tokens_used: Option<u64>,
    /// Context window token limit for this request.
    pub context_token_limit: Option<u64>,
    /// Interaction mode: "agent", "chat", "composer", "tab".
    pub interaction_mode: Option<String>,
    /// Human-readable session title.
    pub session_title: Option<String>,
    /// Lines of code added in this session.
    pub lines_added: Option<u64>,
    /// Lines of code removed in this session.
    pub lines_removed: Option<u64>,
    /// Parent message UUID (for subagent messages).
    pub parent_uuid: Option<String>,
    /// User name (set by IdentityEnricher).
    pub user_name: Option<String>,
    /// Machine name (set by IdentityEnricher).
    pub machine_name: Option<String>,
}

/// Parse a single JSONL line into a `ParsedMessage`, if relevant.
pub fn parse_line(line: &str) -> Option<ParsedMessage> {
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
            tool_names: vec![],
            has_thinking: false,
            stop_reason: None,
            text_length: u.message.content.text_length(),
            version: u.version,
            git_branch: u.git_branch,
            repo_id: None,
            provider: "claude_code".to_string(),
            cost_cents: None,
            context_tokens_used: None,
            context_token_limit: None,
            interaction_mode: None,
            session_title: None,
            lines_added: None,
            lines_removed: None,
            parent_uuid: u.parent_uuid,
            user_name: None,
            machine_name: None,
        }),
        TranscriptEntry::Assistant(a) => {
            let usage = a.message.usage.as_ref();
            let blocks = a.message.content.as_deref().unwrap_or(&[]);
            let tool_names: Vec<String> = blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse { name } => Some(name.clone()),
                    _ => None,
                })
                .collect();
            let has_thinking = blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::Thinking { .. }));
            let text_length: usize = blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.len()),
                    _ => None,
                })
                .sum();
            // Context tokens used = sum of all input-side tokens
            let context_tokens_used = usage.map(|u| u.total_input());
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
                tool_names,
                has_thinking,
                stop_reason: a.message.stop_reason,
                text_length,
                version: None,
                git_branch: None,
                repo_id: None,
                provider: "claude_code".to_string(),
                cost_cents: None, // Calculated during ingest from tokens × pricing
                context_tokens_used,
                context_token_limit: None,
                interaction_mode: Some("agent".to_string()), // Claude Code is always agent mode
                session_title: None,
                lines_added: None,
                lines_removed: None,
                parent_uuid: a.parent_uuid,
                user_name: None,
                machine_name: None,
            })
        }
        TranscriptEntry::Other => None,
    }
}

/// A git commit hash extracted from a JSONL tool_result entry.
#[derive(Debug, Clone, PartialEq)]
pub struct JsonlGitCommit {
    /// Short hash (7-12 chars) from git commit output.
    pub short_hash: String,
}

/// Extract git commit short hashes from JSONL transcript content.
///
/// Scans for tool_result entries that contain git commit output in the format:
/// `[branch shorthash] commit message`
///
/// This gives definitive session→commit links: the AI agent created these commits.
pub fn extract_git_commit_hashes(content: &str) -> Vec<JsonlGitCommit> {
    let mut results = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Fast pre-filter: only parse lines that could contain tool results
        // User messages with structured content have tool_result blocks
        if !line.contains("tool_result") {
            continue;
        }

        let val: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Navigate to message.content array (user messages with structured content)
        let contents = match val.pointer("/message/content") {
            Some(c) => c,
            None => continue,
        };
        let arr = match contents.as_array() {
            Some(a) => a,
            None => continue,
        };

        for item in arr {
            if item.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
                continue;
            }

            let text = tool_result_text(item);
            for hash in extract_commit_hashes_from_text(&text) {
                if seen.insert(hash.clone()) {
                    results.push(JsonlGitCommit { short_hash: hash });
                }
            }
        }
    }

    results
}

/// Extract text from a tool_result content block.
/// Content can be a string or an array of content blocks.
fn tool_result_text(item: &Value) -> String {
    if let Some(content) = item.get("content") {
        if let Some(s) = content.as_str() {
            return s.to_string();
        }
        if let Some(arr) = content.as_array() {
            let mut text = String::new();
            for block in arr {
                if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                    text.push_str(t);
                    text.push('\n');
                }
            }
            return text;
        }
    }
    String::new()
}

/// Extract git commit short hashes from text that looks like git commit output.
///
/// Git commit output format: `[branch shorthash] commit message`
/// Examples:
///   `[main abc1234] Fix bug`
///   `[feature/auth a1b2c3d] Add login flow`
///   `[main abc1234] Fix bug\n 1 file changed, 2 insertions(+)`
fn extract_commit_hashes_from_text(text: &str) -> Vec<String> {
    let mut hashes = Vec::new();
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        if bytes[i] == b'[' {
            // Look for pattern: [<branch> <hex>] where hex is 7-12 chars
            if let Some(close) = text[i..].find(']') {
                let inner = &text[i + 1..i + close];
                if let Some(space_pos) = inner.rfind(' ') {
                    let candidate = &inner[space_pos + 1..];
                    if candidate.len() >= 7
                        && candidate.len() <= 12
                        && candidate.bytes().all(|b| b.is_ascii_hexdigit())
                        && space_pos > 0
                    {
                        hashes.push(candidate.to_string());
                    }
                }
                i += close + 1;
                continue;
            }
        }
        i += 1;
    }

    hashes
}

/// Parse all lines from a JSONL string, returning parsed messages and the byte
/// offset of the end of the last successfully parsed line.
pub fn parse_transcript(content: &str, start_offset: usize) -> (Vec<ParsedMessage>, usize) {
    let mut messages = Vec::new();
    let mut offset = start_offset;

    let remaining = &content[start_offset..];
    let mut pos = 0;
    for line in remaining.lines() {
        let line_start = pos;
        pos += line.len();
        // Only count the newline if it actually exists (handles files without trailing newline)
        if pos < remaining.len() && remaining.as_bytes()[pos] == b'\n' {
            pos += 1;
        }
        if let Some(msg) = parse_line(line) {
            messages.push(msg);
        }
        let _ = line_start; // suppress unused warning
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
        assert_eq!(msg.text_length, 11);
        assert_eq!(msg.version.as_deref(), Some("2.1.76"));
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
        assert_eq!(msg.tool_names, vec!["Read"]);
        assert_eq!(msg.stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(msg.text_length, 6);
        assert_eq!(msg.model.as_deref(), Some("claude-opus-4-6"));
    }

    #[test]
    fn parse_thinking_block() {
        let line = r#"{"parentUuid":"abc","isSidechain":false,"type":"assistant","message":{"model":"claude-opus-4-6","id":"msg_2","type":"message","role":"assistant","content":[{"type":"thinking","thinking":"hmm","signature":"sig"}],"stop_reason":null,"usage":{"input_tokens":10,"output_tokens":5}},"uuid":"ghi-789","timestamp":"2026-03-14T18:14:12.000Z","sessionId":"sess-1"}"#;
        let msg = parse_line(line).unwrap();
        assert!(msg.has_thinking);
        assert!(msg.tool_names.is_empty());
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
    fn extract_commit_hash_from_git_output() {
        let hashes = extract_commit_hashes_from_text(
            "[main abc1234] Fix bug\n 1 file changed, 2 insertions(+)",
        );
        assert_eq!(hashes, vec!["abc1234"]);
    }

    #[test]
    fn extract_commit_hash_feature_branch() {
        let hashes = extract_commit_hashes_from_text(
            "[feature/auth a1b2c3d] Add login flow",
        );
        assert_eq!(hashes, vec!["a1b2c3d"]);
    }

    #[test]
    fn extract_commit_hash_no_match() {
        // Not a git commit output
        let hashes = extract_commit_hashes_from_text("Hello [world]");
        assert!(hashes.is_empty());

        // Hash too short
        let hashes = extract_commit_hashes_from_text("[main abc12] msg");
        assert!(hashes.is_empty());

        // Hash has non-hex
        let hashes = extract_commit_hashes_from_text("[main zzzzzzz] msg");
        assert!(hashes.is_empty());
    }

    #[test]
    fn extract_commit_hash_amend() {
        // git commit --amend output
        let hashes = extract_commit_hashes_from_text(
            "[main fedcba9] Updated message\n 3 files changed",
        );
        assert_eq!(hashes, vec!["fedcba9"]);
    }

    #[test]
    fn extract_git_commits_from_jsonl() {
        // Simulate a JSONL file with a tool_result containing git commit output
        let jsonl = concat!(
            r#"{"type":"assistant","message":{"model":"claude-opus-4-6","content":[{"type":"tool_use","name":"Bash","id":"t1","input":{"command":"git commit -m 'Fix bug'"}}],"usage":{"input_tokens":10,"output_tokens":5}},"uuid":"a1","timestamp":"2026-03-14T18:14:00.000Z","sessionId":"s1"}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"[main abc1234] Fix bug\n 1 file changed, 2 insertions(+), 1 deletion(-)"}]},"uuid":"u2","timestamp":"2026-03-14T18:14:01.000Z","sessionId":"s1"}"#,
            "\n",
        );
        let commits = extract_git_commit_hashes(jsonl);
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].short_hash, "abc1234");
    }

    #[test]
    fn extract_git_commits_array_content() {
        // tool_result with array content
        let jsonl = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"[feature/foo deadbeef01] Add feature\n 2 files changed"}]}]},"uuid":"u2","timestamp":"2026-03-14T18:14:01.000Z","sessionId":"s1"}"#;
        let commits = extract_git_commit_hashes(jsonl);
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].short_hash, "deadbeef01");
    }

    #[test]
    fn extract_git_commits_dedup() {
        // Same hash appearing twice should be deduplicated
        let jsonl = concat!(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"[main abc1234] Fix bug"}]},"uuid":"u1","timestamp":"2026-03-14T18:14:01.000Z","sessionId":"s1"}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t2","content":"[main abc1234] Fix bug"}]},"uuid":"u2","timestamp":"2026-03-14T18:14:02.000Z","sessionId":"s1"}"#,
            "\n",
        );
        let commits = extract_git_commit_hashes(jsonl);
        assert_eq!(commits.len(), 1);
    }

    #[test]
    fn extract_git_commits_no_tool_results() {
        let jsonl = r#"{"type":"user","message":{"role":"user","content":"hello"},"uuid":"u1","timestamp":"2026-03-14T18:14:01.000Z","sessionId":"s1"}"#;
        let commits = extract_git_commit_hashes(jsonl);
        assert!(commits.is_empty());
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
