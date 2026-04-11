//! Legacy hook helpers retained for migration compatibility and prompt classification.
//!
//! Hook ingestion and the `hook_events` table were removed in v22. This module
//! keeps only the functions needed by migration paths (v17→v18 link backfill)
//! and prompt classification used by the JSONL parser.

use anyhow::Result;
use rusqlite::{Connection, params};
use serde_json::Value;

pub const HOOK_LINK_EXACT_REQUEST_ID: &str = "exact_request_id";
pub const HOOK_LINK_EXACT_TOOL_USE_ID: &str = "exact_tool_use_id";
pub const HOOK_LINK_UNLINKED: &str = "unlinked";

fn extract_string_path(json: &Value, path: &[&str]) -> Option<String> {
    let mut current = json;
    for key in path {
        current = current.get(*key)?;
    }
    current
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

pub fn extract_hook_message_request_id(json: &Value) -> Option<String> {
    let top_level_keys = [
        "message_request_id",
        "message_id",
        "request_id",
        "response_id",
        "id",
    ];
    for key in top_level_keys {
        if let Some(value) = json
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(value.to_string());
        }
    }

    let nested_paths = [
        ["message", "id"].as_slice(),
        ["request", "id"].as_slice(),
        ["response", "id"].as_slice(),
        ["payload", "id"].as_slice(),
        ["data", "id"].as_slice(),
    ];
    nested_paths
        .iter()
        .find_map(|path| extract_string_path(json, path))
}

pub fn extract_hook_tool_use_id(json: &Value) -> Option<String> {
    let top_level_keys = ["tool_use_id", "tool_call_id"];
    for key in top_level_keys {
        if let Some(value) = json
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(value.to_string());
        }
    }

    let nested_paths = [
        ["tool_use", "id"].as_slice(),
        ["tool_call", "id"].as_slice(),
        ["payload", "tool_use_id"].as_slice(),
        ["payload", "tool_call_id"].as_slice(),
    ];
    nested_paths
        .iter()
        .find_map(|path| extract_string_path(json, path))
}

/// Link a hook event to a message row by request_id or tool_use_id.
/// Used by migration v17→v18 backfill; hook_events table is dropped in v22.
pub fn resolve_hook_message_link(
    conn: &Connection,
    session_id: Option<&str>,
    message_request_id: Option<&str>,
    tool_use_id: Option<&str>,
) -> Result<(Option<String>, String)> {
    let Some(sid) = session_id.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok((None, HOOK_LINK_UNLINKED.to_string()));
    };

    if let Some(request_id) = message_request_id.map(str::trim).filter(|s| !s.is_empty()) {
        let linked_by_request: Option<String> = conn
            .query_row(
                "SELECT id
                 FROM messages
                 WHERE session_id = ?1 AND request_id = ?2
                 ORDER BY timestamp DESC
                 LIMIT 1",
                params![sid, request_id],
                |row| row.get(0),
            )
            .ok();
        if let Some(uuid) = linked_by_request {
            return Ok((Some(uuid), HOOK_LINK_EXACT_REQUEST_ID.to_string()));
        }
    }

    if let Some(tool_id) = tool_use_id.map(str::trim).filter(|s| !s.is_empty()) {
        let linked_by_tool: Option<String> = conn
            .query_row(
                "SELECT m.id
                 FROM messages m
                 JOIN tags t ON t.message_id = m.id
                 WHERE m.session_id = ?1
                   AND t.key = 'tool_use_id'
                   AND t.value = ?2
                 ORDER BY m.timestamp DESC
                 LIMIT 1",
                params![sid, tool_id],
                |row| row.get(0),
            )
            .ok();
        if let Some(uuid) = linked_by_tool {
            return Ok((Some(uuid), HOOK_LINK_EXACT_TOOL_USE_ID.to_string()));
        }
    }

    Ok((None, HOOK_LINK_UNLINKED.to_string()))
}

fn contains_word(text: &str, word: &str) -> bool {
    let mut start = 0;
    while let Some(pos) = text[start..].find(word) {
        let abs_pos = start + pos;
        let before_ok = abs_pos == 0 || !text.as_bytes()[abs_pos - 1].is_ascii_alphanumeric();
        let after_pos = abs_pos + word.len();
        let after_ok =
            after_pos >= text.len() || !text.as_bytes()[after_pos].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
        start = abs_pos + 1;
    }
    false
}

/// Classify a user prompt into a category using keyword heuristics.
/// Returns None if no category matches (system commands, very short, or ambiguous).
///
/// Categories: bugfix, refactor, testing, review, ops, question, writing, feature.
pub fn classify_prompt(text: &str) -> Option<String> {
    let lower = text.to_lowercase();

    if lower.starts_with('/') || lower.len() < 5 {
        return None;
    }
    if lower.starts_with('<') && !lower.contains(' ') {
        return None;
    }

    let bugfix_words = [
        "fix",
        "bug",
        "broken",
        "error",
        "crash",
        "issue",
        "debug",
        "failing",
        "fails",
        "wrong",
        "regression",
        "workaround",
        "patch",
        "hotfix",
        "not working",
        "doesn't work",
        "does not work",
        "isn't working",
        "stopped working",
    ];
    let refactor_words = [
        "refactor",
        "rename",
        "clean up",
        "extract",
        "reorganize",
        "simplify",
        "restructure",
        "move",
        "split",
        "consolidate",
        "deduplicate",
        "dedup",
        "inline",
        "remove",
        "delete",
        "deprecate",
        "replace",
        "convert",
        "rewrite",
        "tidy",
    ];
    let testing_words = [
        "test",
        "tests",
        "testing",
        "spec",
        "specs",
        "unit test",
        "integration test",
        "e2e",
        "coverage",
        "assert",
        "mock",
        "fixture",
        "snapshot",
    ];
    let review_words = [
        "review",
        "audit",
        "validate",
        "verify",
        "inspect",
        "look at",
        "take a look",
        "examine",
        "analyze",
        "analyse",
        "assess",
        "evaluate",
        "feedback",
        "critique",
    ];
    let ops_words = [
        "deploy",
        "release",
        "migrate",
        "upgrade",
        "bump",
        "publish",
        "install",
        "commit",
        "push",
        "merge",
        "rebase",
        "cherry-pick",
        "rollback",
        "revert",
        "configure",
        "provision",
        "ci",
        "cd",
        "docker",
        "k8s",
        "kubernetes",
        "terraform",
        "ansible",
    ];
    let question_words = [
        "why",
        "how does",
        "how do",
        "how can",
        "how to",
        "how much",
        "how often",
        "how many",
        "what is",
        "what does",
        "what are",
        "where is",
        "where does",
        "where are",
        "when does",
        "when is",
        "which",
        "can you tell",
        "can you explain",
        "explain",
        "understand",
        "show me",
        "discover",
        "research",
        "what happens",
        "is there",
        "are there",
        "do we",
        "does this",
        "could you",
        "tell me",
    ];
    let feature_words = [
        "add",
        "implement",
        "create",
        "build",
        "new feature",
        "integrate",
        "introduce",
        "design",
        "make",
        "change",
        "modify",
        "adjust",
        "tweak",
        "set up",
        "setup",
        "enable",
        "support",
        "extend",
        "enhance",
        "improve",
        "optimize",
        "update",
    ];
    let writing_words = [
        "write",
        "draft",
        "article",
        "post",
        "document",
        "blog",
        "readme",
        "changelog",
        "documentation",
    ];
    let plan_words = [
        "plan",
        "the plan",
        "implement the plan",
        "read and implement",
    ];

    if bugfix_words.iter().any(|w| contains_word(&lower, w)) {
        Some("bugfix".to_string())
    } else if refactor_words.iter().any(|w| contains_word(&lower, w)) {
        Some("refactor".to_string())
    } else if testing_words.iter().any(|w| contains_word(&lower, w)) {
        Some("testing".to_string())
    } else if plan_words.iter().any(|w| contains_word(&lower, w)) {
        Some("feature".to_string())
    } else if review_words.iter().any(|w| contains_word(&lower, w)) {
        Some("review".to_string())
    } else if ops_words.iter().any(|w| contains_word(&lower, w)) {
        Some("ops".to_string())
    } else if question_words.iter().any(|w| contains_word(&lower, w)) || lower.ends_with('?') {
        Some("question".to_string())
    } else if writing_words.iter().any(|w| contains_word(&lower, w)) {
        Some("writing".to_string())
    } else if feature_words.iter().any(|w| contains_word(&lower, w)) {
        Some("feature".to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_prompt_bugfix() {
        assert_eq!(
            classify_prompt("fix the login bug"),
            Some("bugfix".to_string())
        );
    }

    #[test]
    fn classify_prompt_feature() {
        assert_eq!(
            classify_prompt("add a new button to the dashboard"),
            Some("feature".to_string())
        );
    }

    #[test]
    fn classify_prompt_question() {
        assert_eq!(
            classify_prompt("how does this work?"),
            Some("question".to_string())
        );
    }

    #[test]
    fn classify_prompt_skips_short() {
        assert_eq!(classify_prompt("hi"), None);
        assert_eq!(classify_prompt("ok cool"), None);
    }

    #[test]
    fn classify_prompt_skips_commands() {
        assert_eq!(classify_prompt("<command>/clear</command>"), None);
        assert_eq!(classify_prompt("/exit"), None);
    }
}
