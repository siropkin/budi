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
    pub git_branch: Option<String>,
    pub message: AssistantMessage,
    pub parent_uuid: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AssistantMessage {
    /// The API request ID (e.g. "msg_01QXt5..."). Multiple JSONL entries can
    /// share the same `id` when a single API response contains multiple content
    /// blocks (thinking, text, tool_use). We deduplicate by this field.
    pub id: Option<String>,
    pub model: Option<String>,
    pub usage: Option<TokenUsage>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TokenUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    /// Breakdown of cache creation tokens by tier.
    pub cache_creation: Option<CacheCreationBreakdown>,
    /// "standard" or "fast" (fast = 6x pricing).
    pub speed: Option<String>,
    /// Server-side tool usage (web search, code execution).
    pub server_tool_use: Option<ServerToolUse>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CacheCreationBreakdown {
    /// Not directly used — 5m tokens are derived as `cache_creation_tokens - ephemeral_1h_input_tokens`.
    /// Kept for deserialization completeness and debugging.
    #[serde(default)]
    #[allow(dead_code)]
    pub ephemeral_5m_input_tokens: u64,
    #[serde(default)]
    pub ephemeral_1h_input_tokens: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ServerToolUse {
    #[serde(default)]
    pub web_search_requests: u64,
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
    /// API request ID (message.id from JSONL). Used for deduplication of
    /// multi-content-block responses.
    pub request_id: Option<String>,
    /// "standard" or "fast". Fast mode = 6x pricing.
    pub speed: Option<String>,
    /// Cache creation tokens using the 1-hour tier (2x input rate, vs 1.25x for 5-min).
    pub cache_creation_1h_tokens: u64,
    /// Number of web search requests (billed separately).
    pub web_search_requests: u64,
}

/// Parse a single JSONL line into a `ParsedMessage`, if relevant.
fn parse_line(line: &str) -> Option<ParsedMessage> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let entry: TranscriptEntry = match serde_json::from_str(line) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("JSONL parse error (skipping line): {e}");
            return None;
        }
    };
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
            // User messages have no cost — use "n/a" to distinguish from
            // assistant messages whose confidence hasn't been set yet (empty string).
            cost_confidence: "n/a".to_string(),
            request_id: None,
            speed: None,
            cache_creation_1h_tokens: 0,
            web_search_requests: 0,
        }),
        TranscriptEntry::Assistant(a) => {
            let usage = a.message.usage.as_ref();
            // Extract 1-hour cache tier tokens from cache_creation breakdown
            let cache_1h = usage
                .and_then(|u| u.cache_creation.as_ref())
                .map(|cc| cc.ephemeral_1h_input_tokens)
                .unwrap_or(0);
            let web_searches = usage
                .and_then(|u| u.server_tool_use.as_ref())
                .map(|s| s.web_search_requests)
                .unwrap_or(0);
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
                git_branch: a.git_branch,
                repo_id: None,
                provider: "claude_code".to_string(),
                cost_cents: None,
                session_title: None,
                parent_uuid: a.parent_uuid,
                user_name: None,
                machine_name: None,
                cost_confidence: "estimated".to_string(),
                request_id: a.message.id,
                speed: usage.and_then(|u| u.speed.clone()),
                cache_creation_1h_tokens: cache_1h,
                web_search_requests: web_searches,
            })
        }
        TranscriptEntry::Other => None,
    }
}

/// Parse all lines from a JSONL string, returning parsed messages and the byte
/// offset of the end of the last successfully parsed line.
///
/// Claude Code logs each content block of a single API response as a separate
/// JSONL entry (sharing the same `message.id`). Intermediate entries have
/// identical input/cache tokens but partial output_tokens; only the final entry
/// (highest output_tokens) is authoritative. We deduplicate by `message.id`,
/// keeping the entry with the most output_tokens to avoid inflating costs.
pub fn parse_transcript(content: &str, start_offset: usize) -> (Vec<ParsedMessage>, usize) {
    let mut messages = Vec::new();
    let mut offset = start_offset;

    let remaining = &content[start_offset..];
    let mut pos = 0;
    for line in remaining.lines() {
        let line_end = pos + line.len();
        // Only count the newline if it actually exists
        let has_newline = line_end < remaining.len() && remaining.as_bytes()[line_end] == b'\n';
        if !has_newline && line_end == remaining.len() {
            // Incomplete final line (no trailing newline) — don't advance offset past it.
            // It may be a truncated write that will be completed later.
            break;
        }
        pos = line_end + if has_newline { 1 } else { 0 };
        if let Some(msg) = parse_line(line) {
            messages.push(msg);
        }
        offset = start_offset + pos;
    }

    dedup_by_request_id(&mut messages);

    (messages, offset)
}

/// Deduplicate assistant messages that share the same API request ID.
/// When a single API call produces multiple content blocks (thinking, text,
/// tool_use), Claude Code writes each as a separate JSONL entry with the same
/// `message.id`. All entries have identical input/cache tokens, but only the
/// last one has the final output_tokens count. We keep the entry with the
/// highest output_tokens and discard the rest.
fn dedup_by_request_id(messages: &mut Vec<ParsedMessage>) {
    use std::collections::HashMap;

    // Map request_id → index of the best (highest output_tokens) message so far
    let mut best: HashMap<String, usize> = HashMap::new();
    let mut to_remove = Vec::new();

    for i in 0..messages.len() {
        let Some(ref request_id) = messages[i].request_id else {
            continue; // no request_id (user messages, etc.) — keep as-is
        };
        if let Some(&prev_idx) = best.get(request_id) {
            // Keep the one with higher output_tokens
            if messages[i].output_tokens > messages[prev_idx].output_tokens {
                to_remove.push(prev_idx);
                best.insert(request_id.clone(), i);
            } else {
                to_remove.push(i);
            }
        } else {
            best.insert(request_id.clone(), i);
        }
    }

    if to_remove.is_empty() {
        return;
    }

    // Sort descending so we can remove from the end without invalidating indices
    to_remove.sort_unstable_by(|a, b| b.cmp(a));
    to_remove.dedup();
    for idx in to_remove {
        messages.remove(idx);
    }
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

    /// Verify that extra usage fields (service_tier, cache_creation sub-object)
    /// don't break parsing. Real JSONL includes these fields.
    #[test]
    fn parse_assistant_with_extended_usage_fields() {
        let line = r#"{"parentUuid":"abc","isSidechain":false,"type":"assistant","message":{"model":"claude-opus-4-6","id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":2,"output_tokens":10,"cache_creation_input_tokens":14873,"cache_read_input_tokens":0,"cache_creation":{"ephemeral_5m_input_tokens":14873,"ephemeral_1h_input_tokens":0},"service_tier":"standard","inference_geo":"global"}},"uuid":"ext-1","timestamp":"2026-03-25T00:00:00.000Z","sessionId":"s1","cwd":"/tmp"}"#;
        let msg = parse_line(line).unwrap();
        assert_eq!(msg.input_tokens, 2);
        assert_eq!(msg.output_tokens, 10);
        assert_eq!(msg.cache_creation_tokens, 14873);
        assert_eq!(msg.cache_read_tokens, 0);
    }

    /// Verify user messages have zero tokens (no cost).
    #[test]
    fn user_messages_have_zero_tokens() {
        let line = r#"{"parentUuid":null,"isSidechain":false,"type":"user","message":{"role":"user","content":"test"},"uuid":"u1","timestamp":"2026-03-25T00:00:00.000Z","sessionId":"s1"}"#;
        let msg = parse_line(line).unwrap();
        assert_eq!(msg.input_tokens, 0);
        assert_eq!(msg.output_tokens, 0);
        assert_eq!(msg.cache_creation_tokens, 0);
        assert_eq!(msg.cache_read_tokens, 0);
        assert!(msg.cost_cents.is_none());
    }

    /// Verify assistant message without usage (edge case) gets zero tokens.
    #[test]
    fn assistant_without_usage_gets_zero_tokens() {
        let line = r#"{"parentUuid":"abc","isSidechain":false,"type":"assistant","message":{"model":"claude-opus-4-6","id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn"},"uuid":"no-usage","timestamp":"2026-03-25T00:00:00.000Z","sessionId":"s1"}"#;
        let msg = parse_line(line).unwrap();
        assert_eq!(msg.input_tokens, 0);
        assert_eq!(msg.output_tokens, 0);
    }

    /// When a single API call produces multiple content blocks (thinking, text,
    /// tool_use), Claude Code writes each as a separate JSONL entry sharing the
    /// same message.id. Verify that parse_transcript deduplicates these, keeping
    /// only the entry with the highest output_tokens.
    #[test]
    fn dedup_multi_content_block_entries() {
        // Three entries with same message.id "req1":
        //   1) intermediate: stop_reason=null, output=10
        //   2) intermediate: stop_reason=null, output=10
        //   3) final: stop_reason=tool_use, output=425
        // All share identical input/cache tokens.
        let content = concat!(
            r#"{"parentUuid":"u1","isSidechain":false,"type":"assistant","message":{"model":"claude-opus-4-6","id":"req1","type":"message","role":"assistant","content":[{"type":"thinking","thinking":"hmm"}],"stop_reason":null,"usage":{"input_tokens":3,"output_tokens":10,"cache_creation_input_tokens":21559,"cache_read_input_tokens":0}},"uuid":"a1","timestamp":"2026-03-25T00:00:01.000Z","sessionId":"s1"}"#,
            "\n",
            r#"{"parentUuid":"u1","isSidechain":false,"type":"assistant","message":{"model":"claude-opus-4-6","id":"req1","type":"message","role":"assistant","content":[{"type":"text","text":"hello"}],"stop_reason":null,"usage":{"input_tokens":3,"output_tokens":10,"cache_creation_input_tokens":21559,"cache_read_input_tokens":0}},"uuid":"a2","timestamp":"2026-03-25T00:00:02.000Z","sessionId":"s1"}"#,
            "\n",
            r#"{"parentUuid":"u1","isSidechain":false,"type":"assistant","message":{"model":"claude-opus-4-6","id":"req1","type":"message","role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Read","input":{}}],"stop_reason":"tool_use","usage":{"input_tokens":3,"output_tokens":425,"cache_creation_input_tokens":21559,"cache_read_input_tokens":0}},"uuid":"a3","timestamp":"2026-03-25T00:00:03.000Z","sessionId":"s1"}"#,
            "\n",
        );

        let (msgs, _) = parse_transcript(content, 0);
        // Should have 1 assistant message (deduped from 3)
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].output_tokens, 425);
        assert_eq!(msgs[0].input_tokens, 3);
        assert_eq!(msgs[0].cache_creation_tokens, 21559);
    }

    /// Different request IDs should not be deduped.
    #[test]
    fn no_dedup_across_request_ids() {
        let content = concat!(
            r#"{"parentUuid":"u1","isSidechain":false,"type":"assistant","message":{"model":"claude-opus-4-6","id":"req1","type":"message","role":"assistant","content":[{"type":"text","text":"a"}],"stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":50,"cache_creation_input_tokens":100,"cache_read_input_tokens":200}},"uuid":"a1","timestamp":"2026-03-25T00:00:01.000Z","sessionId":"s1"}"#,
            "\n",
            r#"{"parentUuid":"u1","isSidechain":false,"type":"assistant","message":{"model":"claude-opus-4-6","id":"req2","type":"message","role":"assistant","content":[{"type":"text","text":"b"}],"stop_reason":"end_turn","usage":{"input_tokens":20,"output_tokens":60,"cache_creation_input_tokens":300,"cache_read_input_tokens":400}},"uuid":"a2","timestamp":"2026-03-25T00:00:02.000Z","sessionId":"s1"}"#,
            "\n",
        );

        let (msgs, _) = parse_transcript(content, 0);
        assert_eq!(msgs.len(), 2);
    }

    /// User messages (no request_id) are never deduped.
    #[test]
    fn user_messages_not_deduped() {
        let content = concat!(
            r#"{"parentUuid":null,"isSidechain":false,"type":"user","message":{"role":"user","content":"hi"},"uuid":"u1","timestamp":"2026-03-25T00:00:01.000Z","sessionId":"s1"}"#,
            "\n",
            r#"{"parentUuid":null,"isSidechain":false,"type":"user","message":{"role":"user","content":"hello"},"uuid":"u2","timestamp":"2026-03-25T00:00:02.000Z","sessionId":"s1"}"#,
            "\n",
        );

        let (msgs, _) = parse_transcript(content, 0);
        assert_eq!(msgs.len(), 2);
    }
}
