use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use budi_core::config;
use serde_json::Value;

pub mod doctor;
pub mod health;
pub mod import;
pub mod init;
pub mod integrations;
pub mod open;
pub mod repair;
pub mod stats;
pub mod statusline;
pub mod sync;
pub mod uninstall;
pub mod update;

// ---------------------------------------------------------------------------
// Hook event constants and detection helpers — re-exported from budi-core
// ---------------------------------------------------------------------------

pub use budi_core::integrations::{
    CC_HOOK_EVENTS, CURSOR_HOOK_EVENTS, is_budi_cc_hook_entry, is_budi_cursor_hook_entry,
};

// ---------------------------------------------------------------------------
// JSON file I/O helpers
// ---------------------------------------------------------------------------

/// Read a JSON file, returning an empty object if missing or invalid.
pub fn read_json_or_default(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(serde_json::json!({}));
    }
    let raw =
        fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;
    let val = serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| serde_json::json!({}));
    if val.is_object() {
        Ok(val)
    } else {
        Ok(serde_json::json!({}))
    }
}

/// Read a JSON object file strictly, preserving invalid content instead of overwriting it.
///
/// If the file does not exist, returns `{}`. If parsing fails (or the root is not an
/// object), creates a backup next to the file and returns an error.
pub fn read_json_object_strict(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(serde_json::json!({}));
    }

    let raw =
        fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;
    match serde_json::from_str::<Value>(&raw) {
        Ok(val) if val.is_object() => Ok(val),
        Ok(_) => {
            let backup = backup_invalid_json(path)?;
            anyhow::bail!(
                "{} is not a JSON object. Backed up the file to {}",
                path.display(),
                backup.display()
            );
        }
        Err(e) => {
            let backup = backup_invalid_json(path)?;
            anyhow::bail!(
                "Invalid JSON in {}: {}. Backed up the file to {}",
                path.display(),
                e,
                backup.display()
            );
        }
    }
}

fn backup_invalid_json(path: &Path) -> Result<PathBuf> {
    let ts = chrono::Utc::now().format("%Y%m%d%H%M%S");
    let backup = PathBuf::from(format!("{}.invalid.{ts}.bak", path.display()));
    fs::copy(path, &backup)
        .with_context(|| format!("Failed to create backup {}", backup.display()))?;
    Ok(backup)
}

/// Write JSON to a file atomically (write to .tmp, then rename).
pub fn atomic_write_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    let out = serde_json::to_string_pretty(value)?;
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    fs::write(&tmp, &out).with_context(|| format!("Failed to write {}", tmp.display()))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_json_object_strict_missing_file_defaults_to_object() {
        let path = std::env::temp_dir().join(format!("budi-missing-{}.json", std::process::id()));
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
        let parsed = read_json_object_strict(&path).expect("missing file should default");
        assert!(parsed.is_object());
    }

    #[test]
    fn read_json_object_strict_invalid_json_creates_backup() {
        let dir = std::env::temp_dir().join(format!(
            "budi-json-test-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let file = dir.join("settings.json");
        std::fs::write(&file, "{ invalid").expect("write invalid json");

        let err = read_json_object_strict(&file).expect_err("should fail for invalid json");
        let msg = err.to_string();
        assert!(msg.contains("Backed up the file"));

        let mut found_backup = false;
        for entry in std::fs::read_dir(&dir).expect("read temp dir").flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("settings.json.invalid.") && name.ends_with(".bak") {
                found_backup = true;
                break;
            }
        }
        assert!(found_backup, "expected invalid-json backup file");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
