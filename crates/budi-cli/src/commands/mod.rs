use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use budi_core::config;
use serde::Serialize;
use serde_json::Value;

pub mod autostart;
pub mod cloud;
pub mod db;
pub mod doctor;
pub mod import;
pub mod init;
pub mod integrations;
pub mod pricing;
pub mod sessions;
pub mod stats;
pub mod status;
pub mod statusline;
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
///
/// Two contracts beyond the bare write-then-rename (#697):
///
/// 1. **Mode bits preserved.** If the target file already exists, its
///    `st_mode` is captured before the rename and restored after, so a
///    user-set `chmod 600 ~/.claude/settings.json` survives `budi init`
///    rather than reverting to the writer's umask (typically 644).
///    Unix only — Windows is a no-op.
/// 2. **Symlinks preserved.** If `path` is a symlink (dotfile managers
///    like `chezmoi`/`stow`/`yadm` symlink configs into a Git-tracked
///    repo), the symlink itself is left intact and the new content is
///    written atomically at its canonicalized target. If we can't
///    place the tmp inside the target's parent (cross-filesystem
///    rename, permissions, etc.), we fall back to a non-atomic
///    in-place write through the symlink and emit a warning so
///    dotfile managers don't desync silently.
pub fn atomic_write_json(path: &Path, value: &Value) -> Result<()> {
    let out = serde_json::to_string_pretty(value)?;

    // If `path` is a symlink, write to its canonical target so the
    // symlink survives — dotfile managers expect the source-of-truth
    // file (in their tracked repo) to be the one mutated, not the
    // symlink itself.
    let symlink_meta = fs::symlink_metadata(path).ok();
    let is_symlink = symlink_meta
        .as_ref()
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);

    let write_target: PathBuf = if is_symlink {
        match fs::canonicalize(path) {
            Ok(real) => real,
            Err(e) => {
                tracing::warn!(
                    "atomic_write_json: {} is a symlink but its target could not be \
                     resolved ({}); writing through the symlink (non-atomic)",
                    path.display(),
                    e
                );
                fs::write(path, &out)
                    .with_context(|| format!("Failed to write {}", path.display()))?;
                return Ok(());
            }
        }
    } else {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }
        path.to_path_buf()
    };

    // Capture the target's mode bits before we replace it so the
    // umask-derived mode of the freshly written tmp doesn't silently
    // relax a user-set `chmod 600`.
    #[cfg(unix)]
    let preserved_mode: Option<u32> = fs::metadata(&write_target).ok().map(|m| {
        use std::os::unix::fs::PermissionsExt;
        m.permissions().mode()
    });

    // Place the tmp next to the resolved target so the rename stays on
    // the same filesystem.
    let tmp_dir = write_target
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let tmp_name = format!(
        "{}.{}.tmp",
        write_target
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("budi"),
        std::process::id()
    );
    let tmp = tmp_dir.join(tmp_name);

    fs::write(&tmp, &out).with_context(|| format!("Failed to write {}", tmp.display()))?;

    if let Err(e) = fs::rename(&tmp, &write_target) {
        // Cross-filesystem rename, missing-parent race, or similar —
        // fall back to a plain in-place write so the user sees *some*
        // update rather than silent loss. The warn lets dotfile-manager
        // users notice the divergence on next sync.
        tracing::warn!(
            "atomic_write_json: rename {} → {} failed ({}); falling back to in-place write",
            tmp.display(),
            write_target.display(),
            e
        );
        let _ = fs::remove_file(&tmp);
        fs::write(&write_target, &out)
            .with_context(|| format!("Failed to write {}", write_target.display()))?;
    }

    // Restore the original mode bits captured above (Unix only). The
    // tmp file picked up the umask of the writing process, so without
    // this step a `chmod 600` config gets quietly broadened to 644.
    #[cfg(unix)]
    if let Some(mode) = preserved_mode {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(mode);
        if let Err(e) = fs::set_permissions(&write_target, perms) {
            tracing::warn!(
                "atomic_write_json: failed to restore mode 0o{:o} on {}: {}",
                mode,
                write_target.display(),
                e
            );
        }
    }

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

// ---------------------------------------------------------------------------
// CLI JSON output helpers (#445)
// ---------------------------------------------------------------------------
//
// Every `budi` subcommand that emits `--format json` flows through
// [`print_json`] so the user-visible output obeys one contract:
//
//   - Any JSON field whose key ends in `_cents` serialises as an
//     integer (rounded half-to-even via `f64::round`). A raw
//     `f64 cost_cents` on an internal struct surfaces over the wire
//     as `151767`, not `151766.7552219369` — cents are cents.
//
// The normalisation runs on the serialised `serde_json::Value` rather
// than on the source structs so the internal math can stay in `f64`
// (where cost pipelines accumulate fractional cents) without forcing
// every struct to adopt a custom serialiser. Nested objects and
// arrays are walked recursively.

/// Walk `value` in place and round every numeric field whose key ends
/// in `_cents` to an integer. Non-numeric values at those keys are
/// left unchanged. Returns the mutated reference for chaining.
pub fn round_cents_to_integer(value: &mut Value) -> &mut Value {
    match value {
        Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if is_cents_key(k)
                    && let Some(n) = v.as_f64()
                {
                    *v = Value::from(n.round() as i64);
                    continue;
                }
                round_cents_to_integer(v);
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                round_cents_to_integer(v);
            }
        }
        _ => {}
    }
    value
}

fn is_cents_key(key: &str) -> bool {
    key.ends_with("_cents")
}

/// Serialize `value` as pretty JSON and print it to stdout with the
/// CLI cents normalisation applied. All `budi` commands that emit
/// `--format json` should route through this helper so the contract
/// stays consistent.
pub fn print_json<T: Serialize>(value: &T) -> Result<()> {
    let mut v = serde_json::to_value(value).context("serialise to JSON value")?;
    round_cents_to_integer(&mut v);
    let out = serde_json::to_string_pretty(&v).context("serialise JSON value to string")?;
    println!("{out}");
    Ok(())
}

/// Compact variant of [`print_json`] for single-line surfaces like
/// `budi statusline --format json`, where the downstream consumer
/// (Cursor extension, cloud dashboard, user's starship prompt)
/// expects a single-line payload but still benefits from the cents
/// normalisation.
pub fn print_json_compact<T: Serialize>(value: &T) -> Result<()> {
    let mut v = serde_json::to_value(value).context("serialise to JSON value")?;
    round_cents_to_integer(&mut v);
    let out = serde_json::to_string(&v).context("serialise JSON value to string")?;
    println!("{out}");
    Ok(())
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

/// Resolve a user-supplied `--provider` value to its canonical DB name.
///
/// Centralized so every command that takes `--provider` shares one
/// contract: unknown values error with a helpful list, and aliases
/// (`copilot` → `copilot_cli`, `anthropic` → `claude_code`) resolve to
/// the canonical form used in SQLite. Prompt-style commands that want to
/// stay quiet on a typo can map this error to a soft fallback themselves
/// (#615).
pub fn normalize_provider(input: &str) -> Result<String> {
    const KNOWN_PROVIDERS: &[&str] = &[
        "claude_code",
        "cursor",
        "codex",
        "copilot_cli",
        "copilot_chat",
        "openai",
    ];

    if KNOWN_PROVIDERS.contains(&input) {
        return Ok(input.to_string());
    }

    match input {
        "copilot" => Ok("copilot_cli".to_string()),
        "anthropic" => Ok("claude_code".to_string()),
        _ => {
            let all: Vec<&str> = KNOWN_PROVIDERS
                .iter()
                .copied()
                .chain(["copilot", "anthropic"])
                .collect();
            anyhow::bail!(
                "Unknown provider '{}'. Available providers: {}",
                input,
                all.join(", ")
            );
        }
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

    // --- round_cents_to_integer (#445 item 4) -----------------------

    #[test]
    fn round_cents_rounds_ten_digit_float_to_integer() {
        // The exact regression from #445 item 4: a raw `f64 cents`
        // value round-trips through serde_json as a 10-digit float
        // (`151766.7552219369`). After normalisation it must be an
        // integer `151767`.
        let mut v = serde_json::json!({ "total_cost_cents": 151766.7552219369 });
        round_cents_to_integer(&mut v);
        assert_eq!(v["total_cost_cents"], serde_json::json!(151767));
    }

    #[test]
    fn round_cents_recurses_into_nested_objects() {
        let mut v = serde_json::json!({
            "summary": {
                "cost_cents": 1234.567,
                "input_cost": 12.5,
            },
            "breakdown": [
                { "model": "a", "cost_cents": 100.5 },
                { "model": "b", "cost_cents": 0.1 },
                { "model": "c", "cost_cents": 99.49 },
            ],
            "window_start": "2026-04-01",
        });
        round_cents_to_integer(&mut v);
        assert_eq!(v["summary"]["cost_cents"], serde_json::json!(1235));
        // Non-cents fields must be untouched — `input_cost` stays
        // fractional dollars.
        assert_eq!(v["summary"]["input_cost"], serde_json::json!(12.5));
        assert_eq!(v["breakdown"][0]["cost_cents"], serde_json::json!(101));
        assert_eq!(v["breakdown"][1]["cost_cents"], serde_json::json!(0));
        assert_eq!(v["breakdown"][2]["cost_cents"], serde_json::json!(99));
        assert_eq!(v["window_start"], serde_json::json!("2026-04-01"));
    }

    #[test]
    fn round_cents_preserves_already_integer_cents() {
        // Consumers that already pass integer cents (e.g. after cache
        // savings has been pre-rounded) must not see any change.
        let mut v = serde_json::json!({ "cost_cents": 42 });
        let before = v.clone();
        round_cents_to_integer(&mut v);
        assert_eq!(v, before);
    }

    #[test]
    fn round_cents_ignores_non_numeric_cents_values() {
        // If a `*_cents` field ever lands as non-numeric (null / string)
        // the rounder must not panic or mutate it.
        let mut v = serde_json::json!({
            "total_cost_cents": serde_json::Value::Null,
            "other_cents": "n/a",
        });
        let before = v.clone();
        round_cents_to_integer(&mut v);
        assert_eq!(v, before);
    }

    // --- normalize_provider (#615) -----------------------------------

    #[test]
    fn normalize_provider_accepts_canonical_names() {
        // Every canonical name routed through the shared helper round-
        // trips unchanged so callers can pass the result straight to the
        // SQL `provider` column.
        for name in [
            "claude_code",
            "cursor",
            "codex",
            "copilot_cli",
            "copilot_chat",
            "openai",
        ] {
            assert_eq!(normalize_provider(name).unwrap(), name);
        }
    }

    #[test]
    fn normalize_provider_resolves_user_aliases() {
        assert_eq!(normalize_provider("copilot").unwrap(), "copilot_cli");
        assert_eq!(normalize_provider("anthropic").unwrap(), "claude_code");
    }

    #[test]
    fn normalize_provider_rejects_unknown_with_helpful_list() {
        // #615: unknown values must error consistently for every command
        // that takes `--provider` (currently `budi stats` and
        // `budi statusline`). The error mentions the unknown value AND
        // every accepted name so users can fix typos in shell configs
        // without re-reading the docs.
        let err = normalize_provider("doesnotexist").expect_err("must error");
        let msg = err.to_string();
        assert!(msg.contains("doesnotexist"), "error: {msg}");
        for expected in [
            "claude_code",
            "cursor",
            "codex",
            "copilot_cli",
            "copilot_chat",
            "openai",
            "copilot",
            "anthropic",
        ] {
            assert!(msg.contains(expected), "error '{msg}' missing '{expected}'");
        }
    }

    // --- atomic_write_json (#697) ------------------------------------

    /// `chmod 600` on a pre-existing settings.json must survive a
    /// rewrite. Without mode preservation the tmp file's umask-derived
    /// mode (typically 644) becomes the new mode after the rename and
    /// quietly relaxes the user's privacy setting.
    #[cfg(unix)]
    #[test]
    fn atomic_write_json_preserves_mode_bits() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "budi-atomic-mode-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let file = dir.join("settings.json");
        std::fs::write(&file, "{}\n").expect("seed file");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).expect("chmod 600");

        atomic_write_json(&file, &serde_json::json!({"updated": true}))
            .expect("atomic write should succeed");

        let mode = std::fs::metadata(&file)
            .expect("stat after write")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "expected mode to stay 0o600, got 0o{mode:o}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// When the target path is a symlink (dotfile managers like
    /// chezmoi / stow / yadm rely on this layout), the symlink itself
    /// must survive and the new content must land at the symlink's
    /// canonical target.
    #[cfg(unix)]
    #[test]
    fn atomic_write_json_preserves_symlinks() {
        use std::os::unix::fs;

        let dir = std::env::temp_dir().join(format!(
            "budi-atomic-symlink-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let target = dir.join("target.json");
        let link = dir.join("settings.json");
        std::fs::write(&target, "{}\n").expect("seed target");
        fs::symlink(&target, &link).expect("create symlink");

        atomic_write_json(&link, &serde_json::json!({"via": "symlink"}))
            .expect("atomic write through symlink should succeed");

        let link_meta = std::fs::symlink_metadata(&link).expect("stat the link path itself");
        assert!(
            link_meta.file_type().is_symlink(),
            "link must still be a symlink after write"
        );
        let resolved = std::fs::read_link(&link).expect("read symlink target");
        assert_eq!(
            resolved, target,
            "symlink must still point at the original target"
        );
        let body = std::fs::read_to_string(&target).expect("read canonical target");
        assert!(
            body.contains("\"via\""),
            "new content must land at the canonical target, got: {body}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn round_cents_covers_cache_savings_and_avg_cost_keys() {
        // Catalogued `*_cents` surfaces in the current
        // analytics/queries.rs vocabulary: `cost_cents`,
        // `total_cost_cents`, `cache_savings_cents`,
        // `avg_cost_per_message_cents`. Every one of them matches
        // the `_cents` suffix rule without hard-coding a list.
        let mut v = serde_json::json!({
            "cost_cents": 1.5,
            "total_cost_cents": 1000.4,
            "cache_savings_cents": 250.6,
            "avg_cost_per_message_cents": 3.49,
        });
        round_cents_to_integer(&mut v);
        assert_eq!(v["cost_cents"], serde_json::json!(2));
        assert_eq!(v["total_cost_cents"], serde_json::json!(1000));
        assert_eq!(v["cache_savings_cents"], serde_json::json!(251));
        assert_eq!(v["avg_cost_per_message_cents"], serde_json::json!(3));
    }
}
