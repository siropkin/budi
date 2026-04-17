//! Codex provider — imports historical sessions from OpenAI Codex Desktop and
//! Codex CLI transcripts stored at `~/.codex/sessions/` and
//! `~/.codex/archived_sessions/`.
//!
//! Session files are JSONL with event types: `session_meta`, `turn_context`,
//! `event_msg`, `response_item`, etc. We extract token usage from `token_count`
//! events and model info from `turn_context` events.

use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use crate::jsonl::ParsedMessage;
use crate::provider::{DiscoveredFile, ModelPricing, Provider};

/// The Codex provider (covers both Codex Desktop and Codex CLI).
pub struct CodexProvider;

impl Provider for CodexProvider {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn display_name(&self) -> &'static str {
        "Codex"
    }

    fn is_available(&self) -> bool {
        codex_home().map(|p| p.exists()).unwrap_or(false)
    }

    fn discover_files(&self) -> Result<Vec<DiscoveredFile>> {
        let home = codex_home()?;
        let mut files = Vec::new();

        // Active sessions: ~/.codex/sessions/YYYY/MM/DD/*.jsonl
        let sessions_dir = home.join("sessions");
        collect_jsonl_recursive(&sessions_dir, &mut files, 0);

        // Archived sessions: ~/.codex/archived_sessions/*.jsonl
        let archived_dir = home.join("archived_sessions");
        collect_jsonl_recursive(&archived_dir, &mut files, 0);

        // Sort by modification time descending (newest first) for progressive sync.
        files.sort_by(|a, b| {
            let mtime = |p: &PathBuf| {
                p.metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
            };
            mtime(b).cmp(&mtime(a))
        });

        Ok(files
            .into_iter()
            .map(|path| DiscoveredFile { path })
            .collect())
    }

    fn parse_file(
        &self,
        _path: &Path,
        content: &str,
        offset: usize,
    ) -> Result<(Vec<ParsedMessage>, usize)> {
        Ok(parse_codex_transcript(content, offset))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn codex_home() -> Result<PathBuf> {
    Ok(crate::config::home_dir()?.join(".codex"))
}

fn collect_jsonl_recursive(dir: &Path, files: &mut Vec<PathBuf>, depth: u32) {
    if depth > 5 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl_recursive(&path, files, depth + 1);
        } else if path.extension().is_some_and(|e| e == "jsonl") {
            files.push(path);
        }
    }
}

/// Generate a deterministic UUID from a session ID and line index.
fn deterministic_uuid(session_id: &str, line_index: usize) -> String {
    let mut hasher = Sha256::new();
    hasher.update(session_id.as_bytes());
    hasher.update(line_index.to_le_bytes());
    let hash = hasher.finalize();
    // Format as UUID v5-style: 8-4-4-4-12
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        u32::from_be_bytes([hash[0], hash[1], hash[2], hash[3]]),
        u16::from_be_bytes([hash[4], hash[5]]),
        u16::from_be_bytes([hash[6], hash[7]]),
        u16::from_be_bytes([hash[8], hash[9]]),
        // 6 bytes for the last segment
        u64::from_be_bytes([
            0, 0, hash[10], hash[11], hash[12], hash[13], hash[14], hash[15]
        ])
    )
}

// ---------------------------------------------------------------------------
// JSONL parsing
// ---------------------------------------------------------------------------

/// Session-level metadata extracted from `session_meta` events.
#[derive(Debug, Default)]
struct SessionContext {
    session_id: Option<String>,
    cwd: Option<String>,
    git_branch: Option<String>,
}

/// Parse a Codex session JSONL file into `ParsedMessage` records.
///
/// Each `token_count` event with `last_token_usage` data produces one message.
/// The model is tracked from the most recent `turn_context` event.
fn parse_codex_transcript(content: &str, start_offset: usize) -> (Vec<ParsedMessage>, usize) {
    let mut messages = Vec::new();
    let mut offset = start_offset;

    let mut ctx = SessionContext::default();
    let mut current_model: Option<String> = None;
    let mut line_index: usize = 0;

    let remaining = &content[start_offset..];
    let mut pos = 0;

    for line in remaining.lines() {
        let line_end = pos + line.len();
        let has_newline = line_end < remaining.len() && remaining.as_bytes()[line_end] == b'\n';
        if !has_newline && line_end == remaining.len() {
            break;
        }
        pos = line_end + if has_newline { 1 } else { 0 };
        offset = start_offset + pos;
        line_index += 1;

        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };

        let event_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");

        match event_type {
            "session_meta" => {
                parse_session_meta(&value, &mut ctx);
            }
            "turn_context" => {
                if let Some(model) = value
                    .pointer("/payload/model")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                {
                    current_model = Some(model.to_string());
                }
            }
            "event_msg" => {
                let payload_type = value
                    .pointer("/payload/type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                if payload_type == "token_count"
                    && let Some(msg) =
                        parse_token_count(&value, &ctx, current_model.as_deref(), line_index)
                {
                    messages.push(msg);
                }
            }
            _ => {}
        }
    }

    (messages, offset)
}

fn parse_session_meta(value: &serde_json::Value, ctx: &mut SessionContext) {
    let payload = match value.get("payload") {
        Some(p) => p,
        None => return,
    };

    ctx.session_id = payload
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| format!("codex-{s}"));

    ctx.cwd = payload
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(String::from);

    ctx.git_branch = payload
        .pointer("/git/branch")
        .and_then(|v| v.as_str())
        .map(String::from);
}

fn parse_token_count(
    value: &serde_json::Value,
    ctx: &SessionContext,
    model: Option<&str>,
    line_index: usize,
) -> Option<ParsedMessage> {
    let last_usage = value.pointer("/payload/info/last_token_usage")?;

    let input_tokens = last_usage
        .get("input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output_tokens = last_usage
        .get("output_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cached_input_tokens = last_usage
        .get("cached_input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    // Skip events with zero tokens (no-op API calls)
    if input_tokens == 0 && output_tokens == 0 {
        return None;
    }

    let timestamp = value
        .get("timestamp")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<DateTime<Utc>>().ok())
        .unwrap_or_else(|| DateTime::from_timestamp(0, 0).expect("epoch is valid"));

    let session_id_str = ctx.session_id.as_deref().unwrap_or("unknown");
    let uuid = deterministic_uuid(session_id_str, line_index);

    Some(ParsedMessage {
        uuid,
        session_id: ctx.session_id.clone(),
        timestamp,
        cwd: ctx.cwd.clone(),
        role: "assistant".to_string(),
        model: model.map(String::from),
        input_tokens,
        output_tokens,
        cache_creation_tokens: 0,
        cache_read_tokens: cached_input_tokens,
        git_branch: ctx.git_branch.clone(),
        repo_id: None,
        provider: "codex".to_string(),
        cost_cents: None,
        session_title: None,
        parent_uuid: None,
        user_name: None,
        machine_name: None,
        cost_confidence: "estimated".to_string(),
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
    })
}

/// OpenAI model pricing lookup (per million tokens, USD).
/// Source: https://platform.openai.com/docs/pricing
///
/// Codex uses OpenAI models (GPT-5.x, o3, etc.) — share pricing with Cursor's
/// OpenAI entries but in a dedicated function for maintainability.
pub fn codex_pricing_for_model(model: &str) -> ModelPricing {
    let m = model.to_lowercase();

    // --- GPT-5.x models ---
    if m.contains("gpt-5.4") && m.contains("nano") {
        ModelPricing {
            input: 0.20,
            output: 1.25,
            cache_write: 0.20,
            cache_read: 0.02,
        }
    } else if m.contains("gpt-5.4") && m.contains("mini") {
        ModelPricing {
            input: 0.75,
            output: 4.50,
            cache_write: 0.75,
            cache_read: 0.075,
        }
    } else if m.contains("gpt-5.4") {
        ModelPricing {
            input: 2.50,
            output: 15.0,
            cache_write: 2.50,
            cache_read: 0.25,
        }
    } else if m.contains("gpt-5.2") || m.contains("gpt-5.3") {
        ModelPricing {
            input: 1.75,
            output: 14.0,
            cache_write: 1.75,
            cache_read: 0.175,
        }
    } else if m.contains("gpt-5") && m.contains("mini") {
        ModelPricing {
            input: 0.25,
            output: 2.0,
            cache_write: 0.25,
            cache_read: 0.025,
        }
    } else if m.contains("gpt-5") && m.contains("fast") {
        ModelPricing {
            input: 2.50,
            output: 20.0,
            cache_write: 2.50,
            cache_read: 0.25,
        }
    } else if m.contains("gpt-5") {
        ModelPricing {
            input: 1.25,
            output: 10.0,
            cache_write: 1.25,
            cache_read: 0.125,
        }

    // --- GPT-4.x models ---
    } else if m.contains("gpt-4o-mini") {
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

    // --- OpenAI reasoning models ---
    } else if m.contains("o3-mini") || m.contains("o1-mini") {
        ModelPricing {
            input: 1.10,
            output: 4.40,
            cache_write: 1.10,
            cache_read: 0.55,
        }
    } else if m.contains("o3") {
        ModelPricing {
            input: 2.0,
            output: 8.0,
            cache_write: 2.0,
            cache_read: 0.20,
        }
    } else if m.contains("o1") {
        ModelPricing {
            input: 15.0,
            output: 60.0,
            cache_write: 15.0,
            cache_read: 7.50,
        }
    } else {
        // Unknown model — use GPT-5.2/5.3 as default (most common Codex model)
        tracing::warn!(
            "Unknown Codex model '{}', using GPT-5.2/5.3 default pricing",
            model
        );
        ModelPricing {
            input: 1.75,
            output: 14.0,
            cache_write: 1.75,
            cache_read: 0.175,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_uuid_is_stable() {
        let a = deterministic_uuid("sess-1", 42);
        let b = deterministic_uuid("sess-1", 42);
        assert_eq!(a, b);
        let c = deterministic_uuid("sess-1", 43);
        assert_ne!(a, c);
    }

    #[test]
    fn parse_session_meta_extracts_fields() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{
            "timestamp": "2026-04-11T19:28:32.582Z",
            "type": "session_meta",
            "payload": {
                "id": "019d7e04-6762-7f50-baee-ea6ac87cd3e9",
                "cwd": "/tmp/project",
                "git": {"branch": "main", "commit_hash": "abc123"}
            }
        }"#,
        )
        .unwrap();

        let mut ctx = SessionContext::default();
        parse_session_meta(&json, &mut ctx);
        assert_eq!(
            ctx.session_id.as_deref(),
            Some("codex-019d7e04-6762-7f50-baee-ea6ac87cd3e9")
        );
        assert_eq!(ctx.cwd.as_deref(), Some("/tmp/project"));
        assert_eq!(ctx.git_branch.as_deref(), Some("main"));
    }

    #[test]
    fn parse_token_count_with_last_usage() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{
            "timestamp": "2026-04-11T19:29:00.415Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "last_token_usage": {
                        "input_tokens": 18063,
                        "cached_input_tokens": 5504,
                        "output_tokens": 561,
                        "reasoning_output_tokens": 401,
                        "total_tokens": 18624
                    }
                }
            }
        }"#,
        )
        .unwrap();

        let ctx = SessionContext {
            session_id: Some("codex-test".to_string()),
            cwd: Some("/tmp".to_string()),
            git_branch: Some("main".to_string()),
        };

        let msg = parse_token_count(&json, &ctx, Some("gpt-5.3-codex"), 1).unwrap();
        assert_eq!(msg.input_tokens, 18063);
        assert_eq!(msg.output_tokens, 561);
        assert_eq!(msg.cache_read_tokens, 5504);
        assert_eq!(msg.cache_creation_tokens, 0);
        assert_eq!(msg.model.as_deref(), Some("gpt-5.3-codex"));
        assert_eq!(msg.session_id.as_deref(), Some("codex-test"));
        assert_eq!(msg.provider, "codex");
        assert_eq!(msg.role, "assistant");
    }

    #[test]
    fn parse_token_count_skips_null_info() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{
            "timestamp": "2026-04-11T19:28:32.704Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": null
            }
        }"#,
        )
        .unwrap();

        let ctx = SessionContext::default();
        assert!(parse_token_count(&json, &ctx, None, 1).is_none());
    }

    #[test]
    fn parse_token_count_skips_zero_tokens() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{
            "timestamp": "2026-04-11T19:28:32.704Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "last_token_usage": {
                        "input_tokens": 0,
                        "cached_input_tokens": 0,
                        "output_tokens": 0,
                        "total_tokens": 0
                    }
                }
            }
        }"#,
        )
        .unwrap();

        let ctx = SessionContext::default();
        assert!(parse_token_count(&json, &ctx, None, 1).is_none());
    }

    #[test]
    fn parse_transcript_full_session() {
        let content = concat!(
            r#"{"timestamp":"2026-04-11T19:28:32.582Z","type":"session_meta","payload":{"id":"sess-1","cwd":"/tmp","git":{"branch":"feat/test","commit_hash":"abc"}}}"#,
            "\n",
            r#"{"timestamp":"2026-04-11T19:28:32.587Z","type":"turn_context","payload":{"model":"gpt-5.3-codex","turn_id":"t1"}}"#,
            "\n",
            r#"{"timestamp":"2026-04-11T19:28:32.704Z","type":"event_msg","payload":{"type":"token_count","info":null}}"#,
            "\n",
            r#"{"timestamp":"2026-04-11T19:29:00.415Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":18063,"cached_input_tokens":5504,"output_tokens":561,"total_tokens":18624}}}}"#,
            "\n",
            r#"{"timestamp":"2026-04-11T19:29:08.850Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":19531,"cached_input_tokens":0,"output_tokens":132,"total_tokens":19663}}}}"#,
            "\n",
        );

        let (msgs, _offset) = parse_codex_transcript(content, 0);
        assert_eq!(msgs.len(), 2);

        assert_eq!(msgs[0].input_tokens, 18063);
        assert_eq!(msgs[0].output_tokens, 561);
        assert_eq!(msgs[0].cache_read_tokens, 5504);
        assert_eq!(msgs[0].model.as_deref(), Some("gpt-5.3-codex"));
        assert_eq!(msgs[0].session_id.as_deref(), Some("codex-sess-1"));
        assert_eq!(msgs[0].cwd.as_deref(), Some("/tmp"));
        assert_eq!(msgs[0].git_branch.as_deref(), Some("feat/test"));

        assert_eq!(msgs[1].input_tokens, 19531);
        assert_eq!(msgs[1].output_tokens, 132);
        assert_eq!(msgs[1].cache_read_tokens, 0);
    }

    #[test]
    fn parse_transcript_incremental() {
        let content = concat!(
            r#"{"timestamp":"2026-04-11T19:28:32.582Z","type":"session_meta","payload":{"id":"s","cwd":"/tmp"}}"#,
            "\n",
            r#"{"timestamp":"2026-04-11T19:29:00.000Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"output_tokens":50,"total_tokens":150}}}}"#,
            "\n",
        );

        let (msgs, offset) = parse_codex_transcript(content, 0);
        assert_eq!(msgs.len(), 1);

        // No new data from offset
        let (msgs2, _) = parse_codex_transcript(content, offset);
        assert!(msgs2.is_empty());
    }

    #[test]
    fn pricing_gpt_5_3_codex() {
        let p = codex_pricing_for_model("gpt-5.3-codex");
        assert_eq!(p.input, 1.75);
        assert_eq!(p.output, 14.0);
    }

    #[test]
    fn pricing_gpt_5_2_codex() {
        let p = codex_pricing_for_model("gpt-5.2-codex");
        assert_eq!(p.input, 1.75);
        assert_eq!(p.output, 14.0);
    }

    #[test]
    fn pricing_gpt_5_4() {
        let p = codex_pricing_for_model("gpt-5.4");
        assert_eq!(p.input, 2.50);
        assert_eq!(p.output, 15.0);
    }

    #[test]
    fn pricing_gpt_4o() {
        let p = codex_pricing_for_model("gpt-4o-2024-08-06");
        assert_eq!(p.input, 2.50);
        assert_eq!(p.output, 10.0);
    }

    #[test]
    fn pricing_o3() {
        let p = codex_pricing_for_model("o3-2025-04-16");
        assert_eq!(p.input, 2.0);
        assert_eq!(p.output, 8.0);
    }

    #[test]
    fn pricing_unknown_defaults_to_gpt_5_2() {
        let p = codex_pricing_for_model("some-future-model");
        assert_eq!(p.input, 1.75);
        assert_eq!(p.output, 14.0);
    }
}
