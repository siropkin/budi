//! Parser for Claude Code JSONL transcript files.
//!
//! Each line in a transcript is a JSON object with a `type` field.
//! We extract `user` and `assistant` messages for analytics.

use chrono::{DateTime, Utc};
use serde::Deserialize;

/// Default timestamp for subagent entries that omit the field.
fn epoch() -> DateTime<Utc> {
    DateTime::from_timestamp(0, 0).expect("epoch is valid")
}

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

/// Subagent JSONL lines may use `"role"` instead of `"type"` as the
/// discriminator, and place `model`/`usage` directly at top-level rather
/// than inside a `message` wrapper. This struct handles that flat layout.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FlatAssistantEntry {
    pub uuid: Option<String>,
    pub role: Option<String>,
    pub model: Option<String>,
    pub session_id: Option<String>,
    #[serde(default = "epoch")]
    pub timestamp: DateTime<Utc>,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub parent_uuid: Option<String>,
    pub usage: Option<TokenUsage>,
    pub content: Option<Vec<serde_json::Value>>,
    pub id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UserEntry {
    pub uuid: String,
    pub session_id: Option<String>,
    /// Optional for subagent transcripts which may omit timestamp.
    #[serde(default = "epoch")]
    pub timestamp: DateTime<Utc>,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub parent_uuid: Option<String>,
    pub message: Option<UserMessage>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct UserMessage {
    pub content: Option<UserContent>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum UserContent {
    Text(String),
    Blocks(Vec<serde_json::Value>),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AssistantEntry {
    pub uuid: String,
    pub session_id: Option<String>,
    /// Optional for subagent transcripts which may omit timestamp.
    #[serde(default = "epoch")]
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
    pub content: Option<Vec<serde_json::Value>>,
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
    /// Classified activity type from user prompt text (e.g. "bugfix", "feature").
    /// Set during JSONL parsing or from hook events. Propagated user→assistant in pipeline.
    pub prompt_category: Option<String>,
    /// Classifier source for `prompt_category` (e.g. `"rule"`). See
    /// `crate::hooks::SOURCE_*` constants. Propagated alongside
    /// `prompt_category`. Added in R1.2 (#222).
    pub prompt_category_source: Option<String>,
    /// Classifier confidence for `prompt_category` (one of `"high"`,
    /// `"medium"`, `"low"`). See `crate::hooks::CONF_*` constants.
    /// Propagated alongside `prompt_category`. Added in R1.2 (#222).
    pub prompt_category_confidence: Option<String>,
    /// Tool names used by this assistant message (may contain multiple values).
    pub tool_names: Vec<String>,
    /// Tool-use block IDs emitted by assistant content blocks.
    pub tool_use_ids: Vec<String>,
    /// Raw file paths extracted from tool-call arguments (e.g. Read/Write/Edit
    /// `file_path`, Grep/Glob `path`/`pattern`). Normalized to repo-relative
    /// paths later by `FileEnricher` under ADR-0083 privacy rules; see
    /// `crate::file_attribution`. Added in R1.4 (#292).
    pub tool_files: Vec<String>,
    /// Tool outcomes extracted from provider `tool_result` blocks on this
    /// message (populated on **user** messages in Claude Code transcripts,
    /// which is where tool results land). The pipeline joins these to the
    /// originating assistant message by `tool_use_id` and emits
    /// `tool_outcome` tags there. Added in R1.5 (#293); see ADR-0088 §5.
    pub tool_outcomes: Vec<ToolOutcome>,
}

/// One provider-reported tool outcome. Content is never stored — we only
/// keep the classified label and the `tool_use_id` it binds to. See
/// ADR-0083 for the privacy contract.
#[derive(Debug, Clone)]
pub struct ToolOutcome {
    pub tool_use_id: String,
    /// Bounded label; one of `success`, `error`, `denied`. `retry` is
    /// produced later by a session-scoped heuristic in the pipeline, not
    /// by this extractor.
    pub outcome: String,
}

impl Default for ParsedMessage {
    fn default() -> Self {
        Self {
            uuid: String::new(),
            session_id: None,
            timestamp: DateTime::from_timestamp(0, 0).expect("epoch is valid"),
            cwd: None,
            role: String::new(),
            model: None,
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            git_branch: None,
            repo_id: None,
            provider: String::new(),
            cost_cents: None,
            session_title: None,
            parent_uuid: None,
            user_name: None,
            machine_name: None,
            cost_confidence: String::new(),
            request_id: None,
            speed: None,
            cache_creation_1h_tokens: 0,
            web_search_requests: 0,
            prompt_category: None,
            prompt_category_source: None,
            prompt_category_confidence: None,
            tool_names: Vec::new(),
            tool_use_ids: Vec::new(),
            tool_files: Vec::new(),
            tool_outcomes: Vec::new(),
        }
    }
}

/// Scan user-message content blocks for `tool_result` entries and return
/// the classified outcomes (R1.5, #293).
///
/// Claude Code transcripts deliver tool results as `user` messages with
/// content blocks of shape
/// `{"type":"tool_result","tool_use_id":"...","is_error":bool,"content":...}`.
/// We classify each one into a bounded label:
///
/// - `error` — `is_error: true`.
/// - `denied` — the content text matches a small set of
///   permission-denied sentinels used by Claude Code when the user
///   rejects a proposed action.
/// - `success` — otherwise.
///
/// The content text itself is inspected in-memory only — we never
/// persist it. See ADR-0083 §3.
pub(crate) fn extract_user_tool_outcomes(
    content: Option<&Vec<serde_json::Value>>,
) -> Vec<ToolOutcome> {
    let Some(blocks) = content else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for block in blocks {
        let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if block_type != "tool_result" {
            continue;
        }
        let tool_use_id = block
            .get("tool_use_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if tool_use_id.is_empty() {
            continue;
        }
        let is_error = block
            .get("is_error")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let text = tool_result_text(block.get("content"));
        let outcome = classify_tool_outcome(is_error, text.as_deref());
        out.push(ToolOutcome {
            tool_use_id,
            outcome: outcome.to_string(),
        });
    }
    out
}

/// Flatten a `tool_result.content` value into a plain text snippet for
/// sentinel matching. Content may be a string, a block array with
/// `{"type":"text","text":...}` entries, or missing. We truncate to keep
/// the classifier cheap; sentinels we care about all fit well within the
/// first kilobyte of content.
fn tool_result_text(content: Option<&serde_json::Value>) -> Option<String> {
    const MAX: usize = 2048;
    let v = content?;
    let mut s = match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| match b {
                serde_json::Value::String(s) => Some(s.as_str()),
                serde_json::Value::Object(_) => b.get("text").and_then(|v| v.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" "),
        _ => return None,
    };
    truncate_utf8_at_char_boundary(&mut s, MAX);
    if s.trim().is_empty() { None } else { Some(s) }
}

/// Truncate `s` to at most `max` bytes without splitting a UTF-8 scalar.
fn truncate_utf8_at_char_boundary(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut cut = max;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
}

/// Bounded label constants for `tool_outcome` tag values. Pinned here so
/// analytics, tests, live tailing, and `budi import` all agree.
pub const TOOL_OUTCOME_SUCCESS: &str = "success";
pub const TOOL_OUTCOME_ERROR: &str = "error";
pub const TOOL_OUTCOME_DENIED: &str = "denied";
pub const TOOL_OUTCOME_RETRY: &str = "retry";

/// Source label for outcomes extracted directly from a provider
/// `tool_result` block. Mirrors the `_SOURCE` convention used by R1.2
/// (#222) / R1.4 (#292).
pub const TOOL_OUTCOME_SOURCE_JSONL: &str = "jsonl_tool_result";
/// Source label for outcomes attributed by the session-scoped retry
/// heuristic in the pipeline.
pub const TOOL_OUTCOME_SOURCE_HEURISTIC: &str = "heuristic_retry";

/// Confidence labels used on `tool_outcome_confidence` tags. `high` for
/// explicit `tool_result` evidence, `medium` for heuristic retry.
pub const TOOL_OUTCOME_CONFIDENCE_HIGH: &str = "high";
pub const TOOL_OUTCOME_CONFIDENCE_MEDIUM: &str = "medium";

/// Sentinels that indicate the user rejected the tool call in Claude
/// Code / Cursor style permission flows. Matching is case-insensitive
/// and substring-based; we deliberately keep the list short and
/// debuggable. Anything not on this list with `is_error=true` is still
/// classified as `error` — we do not overfit.
const USER_DENIAL_SENTINELS: &[&str] = &[
    "the user doesn't want to take this action",
    "the user doesn't want to proceed",
    "user rejected",
    "user denied",
    "operation cancelled by the user",
    "operation canceled by the user",
    "permission denied by user",
];

/// Classify a single `tool_result` block given its `is_error` flag and
/// flattened content text. Exposed for tests and any parser/pipeline code
/// that needs the shared bounded outcome labels.
pub fn classify_tool_outcome(is_error: bool, text: Option<&str>) -> &'static str {
    if let Some(t) = text {
        let lower = t.to_ascii_lowercase();
        if USER_DENIAL_SENTINELS.iter().any(|s| lower.contains(s)) {
            return TOOL_OUTCOME_DENIED;
        }
    }
    if is_error {
        TOOL_OUTCOME_ERROR
    } else {
        TOOL_OUTCOME_SUCCESS
    }
}

fn extract_assistant_tool_metadata(
    content: Option<&Vec<serde_json::Value>>,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut names = std::collections::HashSet::new();
    let mut tool_use_ids = std::collections::HashSet::new();
    let mut files: Vec<String> = Vec::new();
    let Some(blocks) = content else {
        return (Vec::new(), Vec::new(), Vec::new());
    };

    for block in blocks {
        let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if block_type != "tool_use" {
            continue;
        }
        let tool_name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let trimmed = tool_name.trim();
        if !trimmed.is_empty() {
            names.insert(trimmed.to_string());
        }
        if let Some(id) = block.get("id").and_then(|v| v.as_str()) {
            let normalized = id.trim();
            if !normalized.is_empty() {
                tool_use_ids.insert(normalized.to_string());
            }
        }
        // R1.4 (#292): collect raw file paths from known tool args. The
        // actual repo-relative normalization + privacy filtering happens
        // later in `FileEnricher` once `cwd` / `repo_id` are resolved.
        if let Some(input) = block.get("input") {
            crate::file_attribution::collect_claude_tool_paths(trimmed, input, &mut files);
        }
    }

    let mut out: Vec<String> = names.into_iter().collect();
    out.sort();
    let mut out_ids: Vec<String> = tool_use_ids.into_iter().collect();
    out_ids.sort();
    (out, out_ids, files)
}

/// Parse a single JSONL line into a `ParsedMessage`, if relevant.
/// Tries the standard wrapper format first (`type` discriminator with nested
/// `message`), then falls back to a flat format used by subagent transcripts
/// (top-level `role`, `model`, `usage`).
fn parse_line(line: &str) -> Option<ParsedMessage> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let entry: TranscriptEntry = match serde_json::from_str(line) {
        Ok(e) => e,
        Err(_) => {
            return parse_flat_line(line);
        }
    };
    match entry {
        TranscriptEntry::User(u) => {
            let prompt_text: Option<String> = u.message.as_ref().and_then(|m| match &m.content {
                Some(UserContent::Text(s)) => Some(s.clone()),
                Some(UserContent::Blocks(blocks)) => {
                    let text: String = blocks
                        .iter()
                        .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
                        .collect::<Vec<_>>()
                        .join(" ");
                    if text.is_empty() { None } else { Some(text) }
                }
                None => None,
            });
            // R1.5 (#293): scan the same content blocks for tool_result
            // entries so the pipeline can bind outcomes back to their
            // originating assistant message by `tool_use_id`.
            let tool_outcomes = match u.message.as_ref().and_then(|m| m.content.as_ref()) {
                Some(UserContent::Blocks(blocks)) => extract_user_tool_outcomes(Some(blocks)),
                _ => Vec::new(),
            };
            let classification = prompt_text
                .as_deref()
                .and_then(crate::hooks::classify_prompt_detailed);
            let (prompt_category, prompt_category_source, prompt_category_confidence) =
                match classification {
                    Some(c) => (
                        Some(c.category),
                        Some(c.source.to_string()),
                        Some(c.confidence.to_string()),
                    ),
                    None => (None, None, None),
                };
            Some(ParsedMessage {
                uuid: u.uuid,
                session_id: crate::identity::normalize_optional_session_id(u.session_id.as_deref()),
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
                cost_confidence: "n/a".to_string(),
                request_id: None,
                speed: None,
                cache_creation_1h_tokens: 0,
                web_search_requests: 0,
                prompt_category,
                prompt_category_source,
                prompt_category_confidence,
                tool_names: Vec::new(),
                tool_use_ids: Vec::new(),
                tool_files: Vec::new(),
                tool_outcomes,
            })
        }
        TranscriptEntry::Assistant(a) => {
            if a.message.model.as_deref() == Some("<synthetic>") {
                return None;
            }
            let usage = a.message.usage.as_ref();
            let (tool_names, tool_use_ids, tool_files) =
                extract_assistant_tool_metadata(a.message.content.as_ref());
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
                session_id: crate::identity::normalize_optional_session_id(a.session_id.as_deref()),
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
                prompt_category: None,
                prompt_category_source: None,
                prompt_category_confidence: None,
                tool_names,
                tool_use_ids,
                tool_files,
                tool_outcomes: Vec::new(),
            })
        }
        TranscriptEntry::Other => None,
    }
}

/// Fallback parser for subagent JSONL lines that use a flat format with
/// `"role"` instead of `"type"` and top-level `model`/`usage` fields.
fn parse_flat_line(line: &str) -> Option<ParsedMessage> {
    let flat: FlatAssistantEntry = serde_json::from_str(line).ok()?;
    let role = flat.role.as_deref()?;
    let uuid = flat.uuid?;

    match role {
        "assistant" => {
            if flat.model.as_deref() == Some("<synthetic>") {
                return None;
            }
            let usage = flat.usage.as_ref();
            let (tool_names, tool_use_ids, tool_files) =
                extract_assistant_tool_metadata(flat.content.as_ref());
            let cache_1h = usage
                .and_then(|u| u.cache_creation.as_ref())
                .map(|cc| cc.ephemeral_1h_input_tokens)
                .unwrap_or(0);
            let web_searches = usage
                .and_then(|u| u.server_tool_use.as_ref())
                .map(|s| s.web_search_requests)
                .unwrap_or(0);
            Some(ParsedMessage {
                uuid,
                session_id: crate::identity::normalize_optional_session_id(
                    flat.session_id.as_deref(),
                ),
                timestamp: flat.timestamp,
                cwd: flat.cwd,
                role: "assistant".to_string(),
                model: flat.model,
                input_tokens: usage.and_then(|u| u.input_tokens).unwrap_or(0),
                output_tokens: usage.and_then(|u| u.output_tokens).unwrap_or(0),
                cache_creation_tokens: usage
                    .and_then(|u| u.cache_creation_input_tokens)
                    .unwrap_or(0),
                cache_read_tokens: usage.and_then(|u| u.cache_read_input_tokens).unwrap_or(0),
                git_branch: flat.git_branch,
                repo_id: None,
                provider: "claude_code".to_string(),
                cost_cents: None,
                session_title: None,
                parent_uuid: flat.parent_uuid,
                user_name: None,
                machine_name: None,
                cost_confidence: "estimated".to_string(),
                request_id: flat.id,
                speed: usage.and_then(|u| u.speed.clone()),
                cache_creation_1h_tokens: cache_1h,
                web_search_requests: web_searches,
                prompt_category: None,
                prompt_category_source: None,
                prompt_category_confidence: None,
                tool_names,
                tool_use_ids,
                tool_files,
                tool_outcomes: Vec::new(),
            })
        }
        "user" => Some(ParsedMessage {
            uuid,
            session_id: crate::identity::normalize_optional_session_id(flat.session_id.as_deref()),
            timestamp: flat.timestamp,
            cwd: flat.cwd,
            role: "user".to_string(),
            model: None,
            git_branch: flat.git_branch,
            parent_uuid: flat.parent_uuid,
            provider: "claude_code".to_string(),
            cost_confidence: "n/a".to_string(),
            ..Default::default()
        }),
        _ => None,
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

    let remove_set: std::collections::HashSet<usize> = to_remove.into_iter().collect();
    let mut i = 0;
    messages.retain(|_| {
        let keep = !remove_set.contains(&i);
        i += 1;
        keep
    });
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
        assert_eq!(msg.tool_names, vec!["Read".to_string()]);
        assert_eq!(msg.tool_use_ids, vec!["t1".to_string()]);
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

    // -----------------------------------------------------------------------
    // Subagent transcript format tests (#205)
    // Subagent JSONL uses a flat format: top-level `role`, `model`, `usage`.
    // -----------------------------------------------------------------------

    #[test]
    fn parse_subagent_assistant_flat_format() {
        let line = r#"{"role":"assistant","model":"claude-opus-4-6","id":"msg_sub1","uuid":"sub-a1","usage":{"input_tokens":3,"output_tokens":2,"cache_read_input_tokens":8577}}"#;
        let msg = parse_line(line).unwrap();
        assert_eq!(msg.role, "assistant");
        assert_eq!(msg.model.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(msg.input_tokens, 3);
        assert_eq!(msg.output_tokens, 2);
        assert_eq!(msg.cache_read_tokens, 8577);
        assert_eq!(msg.uuid, "sub-a1");
        assert_eq!(msg.request_id.as_deref(), Some("msg_sub1"));
    }

    #[test]
    fn parse_subagent_user_flat_format() {
        let line = r#"{"role":"user","uuid":"sub-u1","sessionId":"s1"}"#;
        let msg = parse_line(line).unwrap();
        assert_eq!(msg.role, "user");
        assert_eq!(msg.uuid, "sub-u1");
        assert_eq!(msg.session_id.as_deref(), Some("s1"));
        assert_eq!(msg.input_tokens, 0);
    }

    #[test]
    fn parse_subagent_without_timestamp_uses_epoch() {
        let line = r#"{"role":"assistant","model":"claude-haiku-4-5","uuid":"sub-a2","usage":{"input_tokens":10,"output_tokens":5}}"#;
        let msg = parse_line(line).unwrap();
        assert_eq!(msg.timestamp, epoch());
        assert_eq!(msg.input_tokens, 10);
    }

    #[test]
    fn parse_subagent_skips_synthetic_model() {
        let line = r#"{"role":"assistant","model":"<synthetic>","uuid":"sub-synth","usage":{"input_tokens":0,"output_tokens":0}}"#;
        assert!(parse_line(line).is_none());
    }

    #[test]
    fn parse_subagent_skips_unknown_role() {
        let line = r#"{"role":"system","uuid":"sub-sys","model":"x"}"#;
        assert!(parse_line(line).is_none());
    }

    #[test]
    fn parse_transcript_mixed_main_and_subagent() {
        let content = concat!(
            r#"{"parentUuid":null,"isSidechain":false,"type":"user","message":{"role":"user","content":"hi"},"uuid":"u1","timestamp":"2026-03-25T00:00:01.000Z","sessionId":"s1"}"#,
            "\n",
            r#"{"parentUuid":"u1","isSidechain":false,"type":"assistant","message":{"model":"claude-opus-4-6","id":"m1","type":"message","role":"assistant","content":[{"type":"text","text":"hey"}],"stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":5}},"uuid":"a1","timestamp":"2026-03-25T00:00:02.000Z","sessionId":"s1"}"#,
            "\n",
            r#"{"role":"assistant","model":"claude-haiku-4-5","id":"msg_sub","uuid":"sub-a1","usage":{"input_tokens":3,"output_tokens":2}}"#,
            "\n",
        );
        let (msgs, _) = parse_transcript(content, 0);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[1].role, "assistant");
        assert_eq!(msgs[1].model.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(msgs[2].role, "assistant");
        assert_eq!(msgs[2].model.as_deref(), Some("claude-haiku-4-5"));
    }

    /// Subagent messages with same UUID as main messages are deduped by
    /// request_id in parse_transcript.
    #[test]
    fn subagent_dedup_by_request_id_with_main() {
        let content = concat!(
            r#"{"parentUuid":"u1","isSidechain":false,"type":"assistant","message":{"model":"claude-opus-4-6","id":"shared_req","type":"message","role":"assistant","content":[],"usage":{"input_tokens":10,"output_tokens":50}},"uuid":"a1","timestamp":"2026-03-25T00:00:01.000Z","sessionId":"s1"}"#,
            "\n",
            r#"{"role":"assistant","model":"claude-opus-4-6","id":"shared_req","uuid":"sub-a1","usage":{"input_tokens":10,"output_tokens":50}}"#,
            "\n",
        );
        let (msgs, _) = parse_transcript(content, 0);
        assert_eq!(msgs.len(), 1, "duplicate request_id should be deduped");
    }

    // -----------------------------------------------------------------------
    // R1.5 (#293): tool_result → tool_outcome classification.
    // -----------------------------------------------------------------------

    #[test]
    fn classify_plain_success() {
        assert_eq!(classify_tool_outcome(false, None), TOOL_OUTCOME_SUCCESS);
        assert_eq!(
            classify_tool_outcome(false, Some("File contents redacted")),
            TOOL_OUTCOME_SUCCESS
        );
    }

    #[test]
    fn classify_error_flag_wins_when_no_denial() {
        assert_eq!(
            classify_tool_outcome(true, Some("network timeout")),
            TOOL_OUTCOME_ERROR
        );
        assert_eq!(classify_tool_outcome(true, None), TOOL_OUTCOME_ERROR);
    }

    #[test]
    fn classify_user_denial_sentinels() {
        let cases = [
            "The user doesn't want to take this action right now.",
            "User rejected the edit.",
            "Operation cancelled by the user",
            "permission denied by user",
        ];
        for c in cases {
            assert_eq!(
                classify_tool_outcome(true, Some(c)),
                TOOL_OUTCOME_DENIED,
                "{c} should classify as denied",
            );
        }
    }

    #[test]
    fn extract_user_tool_outcomes_parses_blocks() {
        let content = serde_json::json!([
            {"type": "tool_result", "tool_use_id": "t-1", "content": "ok"},
            {"type": "tool_result", "tool_use_id": "t-2", "is_error": true, "content": "boom"},
            {"type": "text", "text": "ignored"},
        ]);
        let blocks = content.as_array().cloned().unwrap();
        let outcomes = extract_user_tool_outcomes(Some(&blocks));
        assert_eq!(outcomes.len(), 2);
        assert_eq!(outcomes[0].tool_use_id, "t-1");
        assert_eq!(outcomes[0].outcome, TOOL_OUTCOME_SUCCESS);
        assert_eq!(outcomes[1].tool_use_id, "t-2");
        assert_eq!(outcomes[1].outcome, TOOL_OUTCOME_ERROR);
    }

    #[test]
    fn extract_user_tool_outcomes_skips_missing_id() {
        let content = serde_json::json!([
            {"type": "tool_result", "content": "no id"},
            {"type": "tool_result", "tool_use_id": "", "content": "empty id"},
        ]);
        let blocks = content.as_array().cloned().unwrap();
        assert!(extract_user_tool_outcomes(Some(&blocks)).is_empty());
    }

    #[test]
    fn parse_user_message_populates_tool_outcomes() {
        let line = r#"{"parentUuid":"a1","isSidechain":false,"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t-1","content":"ok"}]},"uuid":"u1","timestamp":"2026-03-14T18:13:42.614Z","sessionId":"s1"}"#;
        let msg = parse_line(line).unwrap();
        assert_eq!(msg.role, "user");
        assert_eq!(msg.tool_outcomes.len(), 1);
        assert_eq!(msg.tool_outcomes[0].tool_use_id, "t-1");
        assert_eq!(msg.tool_outcomes[0].outcome, TOOL_OUTCOME_SUCCESS);
    }

    #[test]
    fn tool_result_text_truncates_at_utf8_boundary_without_panicking() {
        // 2048 lands in the middle of the emoji's 4-byte UTF-8 encoding.
        let long = format!("{}🙂tail", "a".repeat(2047));
        let value = serde_json::Value::String(long);

        let out = tool_result_text(Some(&value)).expect("text should remain non-empty");

        assert_eq!(out.len(), 2047);
        assert!(out.chars().all(|c| c == 'a'));
    }
}
