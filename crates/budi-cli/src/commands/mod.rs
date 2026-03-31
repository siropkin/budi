use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use budi_core::config;
use serde_json::Value;

pub mod doctor;
pub mod health;
pub mod hook;
pub mod init;
pub mod mcp;
pub mod open;
pub mod stats;
pub mod statusline;
pub mod sync;
pub mod uninstall;
pub mod update;

// ---------------------------------------------------------------------------
// Hook event constants — single source of truth for init, doctor, uninstall
// ---------------------------------------------------------------------------

/// Claude Code hook events (PascalCase).
pub const CC_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "PostToolUse",
    "SubagentStop",
    "PreCompact",
    "Stop",
    "UserPromptSubmit",
];

/// Cursor hook events (camelCase).
pub const CURSOR_HOOK_EVENTS: &[&str] = &[
    "sessionStart",
    "sessionEnd",
    "postToolUse",
    "subagentStop",
    "preCompact",
    "stop",
    "afterFileEdit",
    "beforeSubmitPrompt",
];

// ---------------------------------------------------------------------------
// Hook detection helpers — shared by init, doctor, uninstall
// ---------------------------------------------------------------------------

/// Match any variant of the budi hook command (with or without `|| true` wrapper).
pub fn is_budi_hook_cmd(cmd: &str) -> bool {
    let trimmed = cmd.trim();
    trimmed == "budi hook" || trimmed.starts_with("budi hook ")
}

/// Check if a Claude Code hook entry (nested format) contains a budi hook command.
pub fn is_budi_cc_hook_entry(entry: &Value) -> bool {
    entry
        .get("hooks")
        .and_then(|h| h.as_array())
        .map(|hooks| {
            hooks.iter().any(|h| {
                h.get("command")
                    .and_then(|c| c.as_str())
                    .is_some_and(is_budi_hook_cmd)
            })
        })
        .unwrap_or(false)
}

/// Check if a Cursor hook entry (flat format) contains a budi hook command.
pub fn is_budi_cursor_hook_entry(entry: &Value) -> bool {
    entry
        .get("command")
        .and_then(|c| c.as_str())
        .is_some_and(is_budi_hook_cmd)
}

// ---------------------------------------------------------------------------
// JSON file I/O helpers
// ---------------------------------------------------------------------------

/// Read a JSON file, returning an empty object if missing or invalid.
pub fn read_json_or_default(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(serde_json::json!({}));
    }
    let raw = fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let val = serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| serde_json::json!({}));
    if val.is_object() {
        Ok(val)
    } else {
        Ok(serde_json::json!({}))
    }
}

/// Write JSON to a file atomically (write to .tmp, then rename).
pub fn atomic_write_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    let out = serde_json::to_string_pretty(value)?;
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    fs::write(&tmp, &out)
        .with_context(|| format!("Failed to write {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("Failed to rename {} → {}", tmp.display(), path.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Formatting and utilities
// ---------------------------------------------------------------------------

/// Returns true if color output should be used (NO_COLOR env var is not set).
pub fn use_color() -> bool {
    std::env::var("NO_COLOR").is_err()
}

/// Returns the ANSI escape code if color is enabled, otherwise empty string.
pub fn ansi(code: &str) -> &str {
    if use_color() { code } else { "" }
}

/// Try to resolve a repo root, but return None if not in a git repository.
pub fn try_resolve_repo_root(candidate: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(path) = candidate {
        return Some(path);
    }
    let cwd = std::env::current_dir().ok()?;
    config::find_repo_root(&cwd).ok()
}

/// Format a cost value in dollars: $1.2K, $123, $12.50, $0.42, $0.00
pub fn format_cost(dollars: f64) -> String {
    if dollars >= 1000.0 {
        format!("${:.1}K", dollars / 1000.0)
    } else if dollars >= 100.0 {
        format!("${:.0}", dollars)
    } else if dollars > 0.0 {
        format!("${:.2}", dollars)
    } else {
        "$0.00".to_string()
    }
}
