//! JetBrains AI Assistant provider — tails the chat transcripts the
//! JetBrains-published `com.intellij.ml.llm` plugin (a.k.a. "AI
//! Assistant") writes under each JetBrains IDE configuration directory.
//!
//! This is **not** the GitHub Copilot for JetBrains plugin (ADR-0093,
//! provider id `copilot_chat`). AI Assistant is JetBrains' own
//! Anthropic-backed product; billing flows through the user's JetBrains
//! AI subscription, not GitHub's. We emit rows with
//! `provider = "jetbrains_ai_assistant"` and `surface = "jetbrains"` so
//! both JetBrains-hosted assistants blend correctly under the same
//! surface bucket.
//!
//! Wire format is Anthropic-style JSONL — one event per line, with
//! `message_start` and `message_stop` events bracketing each assistant
//! turn. The parser keys off `message_stop` for the finalized token
//! counts. See `fixtures/synthetic_session_v1.shape.md` for the full
//! shape documentation and open questions for a real-world capture.

use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::jsonl::ParsedMessage;
use crate::provider::{DiscoveredFile, Provider};

/// Canonical provider id. Threaded through `ParsedMessage::provider` and
/// used by `surface::default_for_provider` to coalesce to `jetbrains`.
pub const PROVIDER_ID: &str = "jetbrains_ai_assistant";

/// The JetBrains AI Assistant provider.
pub struct JetBrainsAiAssistantProvider;

impl Provider for JetBrainsAiAssistantProvider {
    fn name(&self) -> &'static str {
        PROVIDER_ID
    }

    fn display_name(&self) -> &'static str {
        "JetBrains AI Assistant"
    }

    fn is_available(&self) -> bool {
        !discover_chat_dirs(&jetbrains_config_roots()).is_empty()
    }

    fn discover_files(&self) -> Result<Vec<DiscoveredFile>> {
        let chat_dirs = discover_chat_dirs(&jetbrains_config_roots());
        let mut files = Vec::new();
        for dir in &chat_dirs {
            collect_jsonl_files(dir, &mut files);
        }

        files.sort_by(|a, b| {
            let mtime = |p: &PathBuf| {
                p.metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
            };
            mtime(&b.path).cmp(&mtime(&a.path))
        });

        Ok(files)
    }

    fn parse_file(
        &self,
        path: &Path,
        content: &str,
        offset: usize,
    ) -> Result<(Vec<ParsedMessage>, usize)> {
        let fallback_session_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string());
        Ok(parse_ai_assistant_transcript(
            content,
            offset,
            fallback_session_id.as_deref(),
        ))
    }

    fn watch_roots(&self) -> Vec<PathBuf> {
        discover_chat_dirs(&jetbrains_config_roots())
    }
}

// ---------------------------------------------------------------------------
// Path discovery
// ---------------------------------------------------------------------------

/// Platform-specific JetBrains configuration roots that contain
/// `<Product><Year>/aiAssistant/chats/` subdirectories.
///
/// JetBrains' configuration root is **not** the GitHub-Copilot XDG path
/// pinned in ADR-0093 — it is the per-IDE config directory the JetBrains
/// installer maintains itself. We return one root per platform; the
/// caller enumerates `<Product><Year>` subdirs underneath.
fn jetbrains_config_roots() -> Vec<PathBuf> {
    let Ok(home) = crate::config::home_dir() else {
        return Vec::new();
    };
    #[cfg(target_os = "macos")]
    {
        vec![home.join("Library/Application Support/JetBrains")]
    }
    #[cfg(target_os = "linux")]
    {
        vec![home.join(".config/JetBrains")]
    }
    #[cfg(target_os = "windows")]
    {
        let mut roots = Vec::new();
        if let Ok(appdata) = std::env::var("APPDATA") {
            roots.push(PathBuf::from(appdata).join("JetBrains"));
        } else {
            roots.push(home.join("AppData/Roaming/JetBrains"));
        }
        roots
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = home;
        Vec::new()
    }
}

/// Enumerate every `<Product><Year>/aiAssistant/chats/` directory that
/// exists under the given JetBrains configuration roots. The product set
/// is open by design — we do not hardcode a closed allowlist of IDE
/// slugs. Returns absolute paths only.
fn discover_chat_dirs(config_roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut chats_dirs = Vec::new();
    for root in config_roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let product_dir = entry.path();
            if !product_dir.is_dir() {
                continue;
            }
            let chats = product_dir.join("aiAssistant").join("chats");
            if chats.is_dir() {
                chats_dirs.push(chats);
            }
        }
    }
    chats_dirs.sort();
    chats_dirs.dedup();
    chats_dirs
}

fn collect_jsonl_files(dir: &Path, out: &mut Vec<DiscoveredFile>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && path.extension().is_some_and(|ext| ext == "jsonl") {
            out.push(DiscoveredFile { path });
        }
    }
}

// ---------------------------------------------------------------------------
// JSONL parsing
// ---------------------------------------------------------------------------

/// Parse a JetBrains AI Assistant transcript (Anthropic-style JSONL) into
/// `ParsedMessage` records starting at `start_offset`. Emits one row per
/// `message_stop` event (the final token counts for that assistant turn).
pub fn parse_ai_assistant_transcript(
    content: &str,
    start_offset: usize,
    fallback_session_id: Option<&str>,
) -> (Vec<ParsedMessage>, usize) {
    let mut messages = Vec::new();
    let mut offset = start_offset;

    let remaining = &content[start_offset..];
    let mut pos = 0;
    let mut current_model: Option<String> = None;

    for line in remaining.lines() {
        let line_end = pos + line.len();
        let has_newline = line_end < remaining.len() && remaining.as_bytes()[line_end] == b'\n';
        if !has_newline && line_end == remaining.len() {
            // Trailing partial line; advance offset only past completed lines.
            break;
        }
        pos = line_end + usize::from(has_newline);
        offset = start_offset + pos;

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };

        let event_type = value.get("type").and_then(Value::as_str).unwrap_or("");

        // Track the running model id so `message_stop` events that omit
        // `model` still inherit attribution from their `message_start`.
        if matches!(event_type, "message_start" | "message_stop")
            && let Some(model) = value
                .get("model")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        {
            current_model = Some(model.to_string());
        }

        if event_type != "message_stop" {
            continue;
        }

        if let Some(msg) = parse_message_stop(&value, current_model.as_deref(), fallback_session_id)
        {
            messages.push(msg);
        }
    }

    (messages, offset)
}

fn parse_message_stop(
    value: &Value,
    running_model: Option<&str>,
    fallback_session_id: Option<&str>,
) -> Option<ParsedMessage> {
    let usage = value.get("usage")?;
    let input_tokens = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_creation_tokens = usage
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_read_tokens = usage
        .get("cache_read_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    // Skip zero-token rows; they would never contribute cost and only
    // pollute the message stream.
    if input_tokens == 0
        && output_tokens == 0
        && cache_creation_tokens == 0
        && cache_read_tokens == 0
    {
        return None;
    }

    let timestamp = value
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<DateTime<Utc>>().ok())
        .unwrap_or_else(|| DateTime::from_timestamp(0, 0).expect("epoch is valid"));

    let session_id = value
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| fallback_session_id.map(String::from));

    let model = value
        .get("model")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| running_model.map(String::from));

    let id = value
        .get("id")
        .and_then(Value::as_str)
        .map(String::from)
        .unwrap_or_else(|| deterministic_uuid(session_id.as_deref(), timestamp));

    Some(ParsedMessage {
        uuid: id.clone(),
        session_id,
        timestamp,
        cwd: None,
        role: "assistant".to_string(),
        model,
        input_tokens,
        output_tokens,
        cache_creation_tokens,
        cache_read_tokens,
        git_branch: None,
        repo_id: None,
        provider: PROVIDER_ID.to_string(),
        cost_cents: None,
        session_title: None,
        parent_uuid: None,
        user_name: None,
        machine_name: None,
        cost_confidence: "estimated".to_string(),
        pricing_source: None,
        request_id: Some(id),
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
        cwd_source: None,
        surface: Some(crate::surface::JETBRAINS.to_string()),
    })
}

fn deterministic_uuid(session_id: Option<&str>, timestamp: DateTime<Utc>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(session_id.unwrap_or("").as_bytes());
    hasher.update(timestamp.timestamp_nanos_opt().unwrap_or(0).to_le_bytes());
    let hash = hasher.finalize();
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        u32::from_be_bytes([hash[0], hash[1], hash[2], hash[3]]),
        u16::from_be_bytes([hash[4], hash[5]]),
        u16::from_be_bytes([hash[6], hash[7]]),
        u16::from_be_bytes([hash[8], hash[9]]),
        u64::from_be_bytes([
            0, 0, hash[10], hash[11], hash[12], hash[13], hash[14], hash[15],
        ])
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SYNTHETIC_FIXTURE: &str =
        include_str!("jetbrains_ai_assistant/fixtures/synthetic_session_v1.jsonl");

    #[test]
    fn parses_synthetic_fixture_emits_one_row_per_message_stop() {
        let (messages, _) = parse_ai_assistant_transcript(SYNTHETIC_FIXTURE, 0, None);
        assert_eq!(messages.len(), 3, "expected 3 rows, one per message_stop");

        // Row 1: claude-sonnet, no cache.
        assert_eq!(messages[0].input_tokens, 1240);
        assert_eq!(messages[0].output_tokens, 612);
        assert_eq!(messages[0].cache_creation_tokens, 0);
        assert_eq!(messages[0].cache_read_tokens, 0);
        assert_eq!(
            messages[0].model.as_deref(),
            Some("claude-sonnet-4-20250514")
        );
        assert_eq!(messages[0].provider, PROVIDER_ID);
        assert_eq!(
            messages[0].surface.as_deref(),
            Some(crate::surface::JETBRAINS)
        );
        assert_eq!(messages[0].role, "assistant");
        assert_eq!(
            messages[0].session_id.as_deref(),
            Some("8c2e5d63-6f4a-4e21-8d11-2b9a3a4e5b6c")
        );

        // Row 2: claude-sonnet, with cache creation + read.
        assert_eq!(messages[1].input_tokens, 2150);
        assert_eq!(messages[1].output_tokens, 845);
        assert_eq!(messages[1].cache_creation_tokens, 1100);
        assert_eq!(messages[1].cache_read_tokens, 900);

        // Row 3: claude-haiku, cache read only.
        assert_eq!(messages[2].input_tokens, 540);
        assert_eq!(messages[2].output_tokens, 210);
        assert_eq!(messages[2].cache_read_tokens, 1800);
        assert_eq!(
            messages[2].model.as_deref(),
            Some("claude-haiku-4-5-20251001")
        );
    }

    #[test]
    fn cost_confidence_is_estimated() {
        let (messages, _) = parse_ai_assistant_transcript(SYNTHETIC_FIXTURE, 0, None);
        for msg in &messages {
            assert_eq!(msg.cost_confidence, "estimated");
            assert!(
                msg.cost_cents.is_none(),
                "cost is computed downstream by CostEnricher"
            );
        }
    }

    #[test]
    fn skips_message_start_and_zero_token_rows() {
        // message_start carries usage but output_tokens=1 (typical first
        // delta); message_stop with all-zero usage should be dropped.
        let content = concat!(
            r#"{"type":"message_start","id":"m1","timestamp":"2026-05-11T18:00:00Z","model":"claude-sonnet-4-20250514","usage":{"input_tokens":100,"output_tokens":1}}"#,
            "\n",
            r#"{"type":"message_stop","id":"m1","timestamp":"2026-05-11T18:00:01Z","model":"claude-sonnet-4-20250514","usage":{"input_tokens":0,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}"#,
            "\n",
        );
        let (messages, _) = parse_ai_assistant_transcript(content, 0, None);
        assert!(
            messages.is_empty(),
            "zero-token message_stop should be skipped"
        );
    }

    #[test]
    fn message_stop_inherits_model_from_running_message_start() {
        let content = concat!(
            r#"{"type":"message_start","id":"m1","timestamp":"2026-05-11T18:00:00Z","model":"claude-sonnet-4-20250514","usage":{"input_tokens":50,"output_tokens":1}}"#,
            "\n",
            r#"{"type":"message_stop","id":"m1","timestamp":"2026-05-11T18:00:05Z","usage":{"input_tokens":50,"output_tokens":100}}"#,
            "\n",
        );
        let (messages, _) = parse_ai_assistant_transcript(content, 0, None);
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].model.as_deref(),
            Some("claude-sonnet-4-20250514")
        );
    }

    #[test]
    fn fallback_session_id_used_when_event_omits_it() {
        let content = concat!(
            r#"{"type":"message_stop","id":"m1","timestamp":"2026-05-11T18:00:05Z","model":"claude-sonnet-4-20250514","usage":{"input_tokens":50,"output_tokens":100}}"#,
            "\n",
        );
        let (messages, _) = parse_ai_assistant_transcript(content, 0, Some("file-stem-sid"));
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].session_id.as_deref(), Some("file-stem-sid"));
    }

    #[test]
    fn incremental_offset_advances_past_completed_lines_only() {
        let line_a = r#"{"type":"message_stop","id":"m1","session_id":"s1","timestamp":"2026-05-11T18:00:05Z","model":"claude-sonnet-4-20250514","usage":{"input_tokens":10,"output_tokens":20}}"#;
        let line_b = r#"{"type":"message_stop","id":"m2","session_id":"s1","timestamp":"2026-05-11T18:01:05Z","model":"claude-sonnet-4-20250514","usage":{"input_tokens":30,"output_tokens":40}}"#;

        let content = format!("{line_a}\n{line_b}\n");

        // First pass: both lines complete.
        let (messages, offset) = parse_ai_assistant_transcript(&content, 0, None);
        assert_eq!(messages.len(), 2);
        assert_eq!(offset, content.len());

        // Second pass with same offset returns nothing.
        let (messages2, offset2) = parse_ai_assistant_transcript(&content, offset, None);
        assert!(messages2.is_empty());
        assert_eq!(offset2, offset);

        // Now simulate a partial trailing line that hasn't been flushed
        // yet — offset should stay at the end of the last completed line.
        let with_partial = format!("{line_a}\n{{\"type\":\"messa");
        let (messages3, offset3) = parse_ai_assistant_transcript(&with_partial, 0, None);
        assert_eq!(messages3.len(), 1);
        assert_eq!(
            offset3,
            line_a.len() + 1,
            "offset should stop at end of first newline"
        );
    }

    #[test]
    fn ignores_malformed_lines() {
        let content = concat!(
            "not json at all\n",
            r#"{"type":"message_stop","id":"m1","session_id":"s1","timestamp":"2026-05-11T18:00:05Z","model":"claude-sonnet-4-20250514","usage":{"input_tokens":10,"output_tokens":20}}"#,
            "\n",
            "{\"truncated\":\n",
        );
        let (messages, _) = parse_ai_assistant_transcript(content, 0, None);
        assert_eq!(messages.len(), 1, "malformed lines should be skipped");
        assert_eq!(messages[0].input_tokens, 10);
    }

    #[test]
    fn discovers_chat_dirs_under_each_jetbrains_product() {
        let tmp = std::env::temp_dir().join("budi-ai-assistant-discover");
        let _ = std::fs::remove_dir_all(&tmp);

        // Two product directories with chat subdirs; one without.
        std::fs::create_dir_all(tmp.join("IntelliJIdea2025.3/aiAssistant/chats")).unwrap();
        std::fs::create_dir_all(tmp.join("WebStorm2026.1/aiAssistant/chats")).unwrap();
        std::fs::create_dir_all(tmp.join("PyCharm2025.2/options")).unwrap();

        let chats = discover_chat_dirs(std::slice::from_ref(&tmp));
        assert_eq!(chats.len(), 2, "expected two chat dirs, got {chats:?}");
        assert!(chats.contains(&tmp.join("IntelliJIdea2025.3/aiAssistant/chats")));
        assert!(chats.contains(&tmp.join("WebStorm2026.1/aiAssistant/chats")));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn discover_chat_dirs_returns_empty_when_root_missing() {
        let chats = discover_chat_dirs(&[PathBuf::from("/nonexistent/jetbrains-root")]);
        assert!(chats.is_empty());
    }

    #[test]
    fn deterministic_uuid_is_stable() {
        let t = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let a = deterministic_uuid(Some("session-a"), t);
        let b = deterministic_uuid(Some("session-a"), t);
        assert_eq!(a, b);
        let c = deterministic_uuid(Some("session-b"), t);
        assert_ne!(a, c);
    }
}
