//! Copilot CLI provider — imports historical sessions from the standalone
//! GitHub Copilot CLI transcripts stored at `~/.copilot/session-state/`.
//!
//! Session directories contain `events.jsonl` with typed events:
//! `user.message`, `assistant.turn_start`, `assistant.turn_end`,
//! `assistant.usage`, `assistant.message`, etc. Token usage is extracted
//! from `assistant.usage` events.
//!
//! The base directory can be overridden via the `COPILOT_HOME` env var.

use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use crate::jsonl::ParsedMessage;
use crate::provider::{DiscoveredFile, Provider};

/// The Copilot CLI provider.
pub struct CopilotProvider;

impl Provider for CopilotProvider {
    fn name(&self) -> &'static str {
        "copilot_cli"
    }

    fn display_name(&self) -> &'static str {
        "Copilot CLI"
    }

    fn is_available(&self) -> bool {
        copilot_home()
            .map(|p| p.join("session-state").exists())
            .unwrap_or(false)
    }

    fn discover_files(&self) -> Result<Vec<DiscoveredFile>> {
        let session_state_dir = copilot_home()?.join("session-state");
        let mut files = Vec::new();

        let Ok(entries) = std::fs::read_dir(&session_state_dir) else {
            return Ok(files);
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let events_file = path.join("events.jsonl");
                if events_file.exists() {
                    files.push(DiscoveredFile { path: events_file });
                }
            }
        }

        // Sort by modification time descending (newest first) for progressive sync.
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
        // Try to extract session ID from the parent directory name.
        let session_id = path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(|s| format!("copilot-{s}"));

        // Try to read workspace.yaml for session metadata.
        let workspace = path
            .parent()
            .map(|dir| dir.join("workspace.yaml"))
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|yaml| parse_workspace_yaml(&yaml));

        Ok(parse_copilot_transcript(
            content, offset, session_id, workspace,
        ))
    }

    fn watch_roots(&self) -> Vec<PathBuf> {
        let Ok(home) = copilot_home() else {
            return Vec::new();
        };
        watch_roots_under(&home)
    }
}

/// Compute Copilot CLI's tailer watch root relative to the given Copilot home.
///
/// Copilot writes session events to `<COPILOT_HOME>/session-state/<session-id>/events.jsonl`.
/// New sessions create new subdirectories, so the daemon attaches a recursive
/// watcher to `session-state/` rather than to each individual session directory.
/// Returns an empty vector when the directory is absent so the daemon can
/// skip the watcher rather than failing to start.
fn watch_roots_under(copilot_home: &Path) -> Vec<PathBuf> {
    let session_state = copilot_home.join("session-state");
    if session_state.is_dir() {
        vec![session_state]
    } else {
        Vec::new()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn copilot_home() -> Result<PathBuf> {
    if let Ok(custom) = std::env::var("COPILOT_HOME") {
        return Ok(PathBuf::from(custom));
    }
    Ok(crate::config::home_dir()?.join(".copilot"))
}

/// Generate a deterministic UUID from a session ID and line index.
fn deterministic_uuid(session_id: &str, line_index: usize) -> String {
    let mut hasher = Sha256::new();
    hasher.update(session_id.as_bytes());
    hasher.update(line_index.to_le_bytes());
    let hash = hasher.finalize();
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        u32::from_be_bytes([hash[0], hash[1], hash[2], hash[3]]),
        u16::from_be_bytes([hash[4], hash[5]]),
        u16::from_be_bytes([hash[6], hash[7]]),
        u16::from_be_bytes([hash[8], hash[9]]),
        u64::from_be_bytes([
            0, 0, hash[10], hash[11], hash[12], hash[13], hash[14], hash[15]
        ])
    )
}

// ---------------------------------------------------------------------------
// Workspace metadata
// ---------------------------------------------------------------------------

/// Metadata extracted from `workspace.yaml`.
#[derive(Debug, Default)]
struct WorkspaceMetadata {
    cwd: Option<String>,
    git_branch: Option<String>,
}

/// Minimal YAML parser for workspace.yaml — avoids a serde_yaml dependency.
/// Looks for `cwd:` and `git_branch:` (or `branch:`) top-level keys.
fn parse_workspace_yaml(content: &str) -> Option<WorkspaceMetadata> {
    let mut meta = WorkspaceMetadata::default();
    for line in content.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("cwd:") {
            let val = val.trim().trim_matches('"').trim_matches('\'');
            if !val.is_empty() {
                meta.cwd = Some(val.to_string());
            }
        } else if let Some(val) = line
            .strip_prefix("branch:")
            .or_else(|| line.strip_prefix("git_branch:"))
        {
            let val = val.trim().trim_matches('"').trim_matches('\'');
            if !val.is_empty() {
                meta.git_branch = Some(val.to_string());
            }
        }
    }
    Some(meta)
}

// ---------------------------------------------------------------------------
// JSONL parsing
// ---------------------------------------------------------------------------

/// Parse a Copilot CLI session events.jsonl file into `ParsedMessage` records.
///
/// Each `assistant.usage` event produces one message. The model is tracked
/// from the most recent `assistant.turn_start` event.
fn parse_copilot_transcript(
    content: &str,
    start_offset: usize,
    session_id: Option<String>,
    workspace: Option<WorkspaceMetadata>,
) -> (Vec<ParsedMessage>, usize) {
    let mut messages = Vec::new();
    let mut offset = start_offset;

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
            "assistant.turn_start" => {
                // Model may be in data.model or data.config.model
                if let Some(model) = value
                    .pointer("/data/model")
                    .or_else(|| value.pointer("/data/config/model"))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                {
                    current_model = Some(model.to_string());
                }
            }
            "assistant.usage" => {
                // Model may also appear in usage events
                if let Some(model) = value
                    .pointer("/data/model")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                {
                    current_model = Some(model.to_string());
                }

                if let Some(msg) = parse_usage_event(
                    &value,
                    session_id.as_deref(),
                    current_model.as_deref(),
                    workspace.as_ref(),
                    line_index,
                ) {
                    messages.push(msg);
                }
            }
            _ => {}
        }
    }

    (messages, offset)
}

fn parse_usage_event(
    value: &serde_json::Value,
    session_id: Option<&str>,
    model: Option<&str>,
    workspace: Option<&WorkspaceMetadata>,
    line_index: usize,
) -> Option<ParsedMessage> {
    let data = value.get("data")?;

    let input_tokens = data
        .get("input_tokens")
        .or_else(|| data.pointer("/usage/input_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output_tokens = data
        .get("output_tokens")
        .or_else(|| data.pointer("/usage/output_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cached_input_tokens = data
        .get("cached_input_tokens")
        .or_else(|| data.pointer("/usage/cached_input_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    // Skip events with zero tokens
    if input_tokens == 0 && output_tokens == 0 {
        return None;
    }

    let timestamp = value
        .get("timestamp")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<DateTime<Utc>>().ok())
        .unwrap_or_else(|| DateTime::from_timestamp(0, 0).expect("epoch is valid"));

    let sid = session_id.unwrap_or("unknown");
    let uuid = deterministic_uuid(sid, line_index);

    Some(ParsedMessage {
        uuid,
        session_id: session_id.map(String::from),
        timestamp,
        cwd: workspace.and_then(|w| w.cwd.clone()),
        role: "assistant".to_string(),
        model: model.map(String::from),
        input_tokens,
        output_tokens,
        cache_creation_tokens: 0,
        cache_read_tokens: cached_input_tokens,
        git_branch: workspace.and_then(|w| w.git_branch.clone()),
        repo_id: None,
        provider: "copilot_cli".to_string(),
        cost_cents: None,
        session_title: None,
        parent_uuid: None,
        user_name: None,
        machine_name: None,
        cost_confidence: "estimated".to_string(),
        pricing_source: None,
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_uuid_is_stable() {
        let a = deterministic_uuid("copilot-sess-1", 42);
        let b = deterministic_uuid("copilot-sess-1", 42);
        assert_eq!(a, b);
        let c = deterministic_uuid("copilot-sess-1", 43);
        assert_ne!(a, c);
    }

    #[test]
    fn parse_workspace_yaml_extracts_fields() {
        let yaml = "cwd: /home/user/project\nbranch: main\nother: value";
        let meta = parse_workspace_yaml(yaml).unwrap();
        assert_eq!(meta.cwd.as_deref(), Some("/home/user/project"));
        assert_eq!(meta.git_branch.as_deref(), Some("main"));
    }

    #[test]
    fn parse_workspace_yaml_quoted_values() {
        let yaml = "cwd: \"/tmp/my project\"\ngit_branch: \"feat/test\"";
        let meta = parse_workspace_yaml(yaml).unwrap();
        assert_eq!(meta.cwd.as_deref(), Some("/tmp/my project"));
        assert_eq!(meta.git_branch.as_deref(), Some("feat/test"));
    }

    #[test]
    fn parse_usage_event_extracts_tokens() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{
            "type": "assistant.usage",
            "data": {
                "input_tokens": 12500,
                "output_tokens": 800,
                "cached_input_tokens": 3000,
                "model": "gpt-5.3"
            },
            "id": "evt-1",
            "timestamp": "2026-04-12T10:30:00.000Z",
            "parentId": null
        }"#,
        )
        .unwrap();

        let workspace = WorkspaceMetadata {
            cwd: Some("/tmp/project".to_string()),
            git_branch: Some("main".to_string()),
        };

        let msg = parse_usage_event(
            &json,
            Some("copilot-sess-1"),
            Some("gpt-5.3"),
            Some(&workspace),
            5,
        )
        .unwrap();
        assert_eq!(msg.input_tokens, 12500);
        assert_eq!(msg.output_tokens, 800);
        assert_eq!(msg.cache_read_tokens, 3000);
        assert_eq!(msg.model.as_deref(), Some("gpt-5.3"));
        assert_eq!(msg.session_id.as_deref(), Some("copilot-sess-1"));
        assert_eq!(msg.cwd.as_deref(), Some("/tmp/project"));
        assert_eq!(msg.git_branch.as_deref(), Some("main"));
        assert_eq!(msg.provider, "copilot_cli");
        assert_eq!(msg.role, "assistant");
    }

    #[test]
    fn parse_usage_event_nested_usage_field() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{
            "type": "assistant.usage",
            "data": {
                "usage": {
                    "input_tokens": 5000,
                    "output_tokens": 200,
                    "cached_input_tokens": 1000
                }
            },
            "id": "evt-2",
            "timestamp": "2026-04-12T10:31:00.000Z",
            "parentId": null
        }"#,
        )
        .unwrap();

        let msg = parse_usage_event(&json, Some("copilot-sess-1"), Some("o3"), None, 3).unwrap();
        assert_eq!(msg.input_tokens, 5000);
        assert_eq!(msg.output_tokens, 200);
        assert_eq!(msg.cache_read_tokens, 1000);
    }

    #[test]
    fn parse_usage_event_skips_zero_tokens() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{
            "type": "assistant.usage",
            "data": {
                "input_tokens": 0,
                "output_tokens": 0,
                "cached_input_tokens": 0
            },
            "id": "evt-3",
            "timestamp": "2026-04-12T10:32:00.000Z",
            "parentId": null
        }"#,
        )
        .unwrap();

        assert!(parse_usage_event(&json, Some("copilot-sess-1"), None, None, 1).is_none());
    }

    #[test]
    fn parse_transcript_full_session() {
        let content = concat!(
            r#"{"type":"assistant.turn_start","data":{"turnId":"t1","model":"gpt-5.3"},"id":"e1","timestamp":"2026-04-12T10:30:00.000Z","parentId":null}"#,
            "\n",
            r#"{"type":"user.message","data":{"content":"fix the bug","turnId":"t1"},"id":"e2","timestamp":"2026-04-12T10:30:00.100Z","parentId":null}"#,
            "\n",
            r#"{"type":"assistant.usage","data":{"input_tokens":15000,"output_tokens":500,"cached_input_tokens":2000},"id":"e3","timestamp":"2026-04-12T10:30:05.000Z","parentId":null}"#,
            "\n",
            r#"{"type":"assistant.turn_end","data":{"turnId":"t1","status":"success"},"id":"e4","timestamp":"2026-04-12T10:30:05.100Z","parentId":null}"#,
            "\n",
            r#"{"type":"assistant.turn_start","data":{"turnId":"t2","model":"o3"},"id":"e5","timestamp":"2026-04-12T10:31:00.000Z","parentId":null}"#,
            "\n",
            r#"{"type":"assistant.usage","data":{"input_tokens":20000,"output_tokens":1000,"cached_input_tokens":0},"id":"e6","timestamp":"2026-04-12T10:31:10.000Z","parentId":null}"#,
            "\n",
        );

        let workspace = WorkspaceMetadata {
            cwd: Some("/home/user/project".to_string()),
            git_branch: Some("feat/test".to_string()),
        };

        let (msgs, _offset) = parse_copilot_transcript(
            content,
            0,
            Some("copilot-sess-1".to_string()),
            Some(workspace),
        );
        assert_eq!(msgs.len(), 2);

        assert_eq!(msgs[0].input_tokens, 15000);
        assert_eq!(msgs[0].output_tokens, 500);
        assert_eq!(msgs[0].cache_read_tokens, 2000);
        assert_eq!(msgs[0].model.as_deref(), Some("gpt-5.3"));
        assert_eq!(msgs[0].session_id.as_deref(), Some("copilot-sess-1"));
        assert_eq!(msgs[0].cwd.as_deref(), Some("/home/user/project"));
        assert_eq!(msgs[0].git_branch.as_deref(), Some("feat/test"));

        assert_eq!(msgs[1].input_tokens, 20000);
        assert_eq!(msgs[1].output_tokens, 1000);
        assert_eq!(msgs[1].cache_read_tokens, 0);
        assert_eq!(msgs[1].model.as_deref(), Some("o3"));
    }

    #[test]
    fn parse_transcript_incremental() {
        let content = concat!(
            r#"{"type":"assistant.usage","data":{"input_tokens":100,"output_tokens":50},"id":"e1","timestamp":"2026-04-12T10:30:00.000Z","parentId":null}"#,
            "\n",
        );

        let (msgs, offset) =
            parse_copilot_transcript(content, 0, Some("copilot-sess-1".to_string()), None);
        assert_eq!(msgs.len(), 1);

        // No new data from offset
        let (msgs2, _) =
            parse_copilot_transcript(content, offset, Some("copilot-sess-1".to_string()), None);
        assert!(msgs2.is_empty());
    }

    #[test]
    fn parse_transcript_model_from_usage_event() {
        let content = concat!(
            r#"{"type":"assistant.usage","data":{"input_tokens":500,"output_tokens":100,"model":"claude-sonnet-4-20250514"},"id":"e1","timestamp":"2026-04-12T10:30:00.000Z","parentId":null}"#,
            "\n",
        );

        let (msgs, _) =
            parse_copilot_transcript(content, 0, Some("copilot-sess-1".to_string()), None);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].model.as_deref(), Some("claude-sonnet-4-20250514"));
    }

    #[test]
    fn watch_roots_returns_session_state_when_present() {
        let tmp = std::env::temp_dir().join("budi-copilot-watch-roots-present");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("session-state/sess-1")).unwrap();

        let roots = watch_roots_under(&tmp);
        assert_eq!(roots, vec![tmp.join("session-state")]);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn watch_roots_empty_when_session_state_absent() {
        let tmp = std::env::temp_dir().join("budi-copilot-watch-roots-absent");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let roots = watch_roots_under(&tmp);
        assert!(roots.is_empty(), "expected empty roots, got {roots:?}");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
