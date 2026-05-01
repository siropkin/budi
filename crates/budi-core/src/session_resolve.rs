//! Server-side resolution for the `current` / `latest` session tokens
//! exposed by `budi sessions <token>` and consumed by the `/budi`
//! Claude Code skill (#603).
//!
//! - `current` is **filesystem-aware**: it walks
//!   `~/.claude/projects/<encoded-cwd>/*.jsonl` and returns the session
//!   id of the most recently modified transcript. This is robust under
//!   multiple concurrent Claude Code sessions across different
//!   projects, where a DB-`latest` view would point at whichever
//!   project sent the last assistant message globally.
//! - `latest` is **DB-driven**: it returns the newest `session_id`
//!   from the `sessions` table. Mirrors the pre-#603 client-side
//!   behaviour in a single server-side place so the wire surface
//!   stays consistent.
//!
//! Encoding: Claude Code's transcript dir form replaces every
//! non-alphanumeric character of the absolute cwd with `-`. The
//! observable consequence is `/Users/me/_proj/x` →
//! `-Users-me--proj-x` (the `_` and `/` and `.` all collapse to `-`).
//! Reverse-engineered against real Claude Code installs; documented
//! as the implementation contract here so future Claude Code changes
//! to that mapping land in one place rather than scattering through
//! the daemon and CLI.

use std::path::{Path, PathBuf};

/// Encode an absolute cwd into Claude Code's `~/.claude/projects/`
/// directory naming convention. Any character that is not ASCII
/// alphanumeric is replaced with `-`. The encoding is one-way (a `_`
/// in the original path collides with `/` in the encoded form), which
/// is why we always start from the cwd the CLI hands us rather than
/// trying to decode a directory name back to a path.
pub fn encode_cwd_for_claude_projects(cwd: &Path) -> String {
    let raw = cwd.to_string_lossy();
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    out
}

/// Look under `<home>/.claude/projects/<encoded-cwd>/` for the most
/// recently modified `*.jsonl` transcript and return the session id
/// (filename stem). Returns `None` when:
///
/// - The encoded directory does not exist (Claude Code never opened
///   a session in this cwd, or Claude Code itself isn't installed).
/// - The directory exists but contains no `*.jsonl` files yet.
/// - The directory contains transcripts whose filenames look invalid
///   (no UTF-8 stem). Empty / sentinel filenames are rejected so the
///   fallback path triggers cleanly instead of returning `Some("")`.
///
/// Caller composes this with the `latest` fallback for the full
/// "current → latest with stderr note" UX described in #603.
pub fn find_current_session_id(home: &Path, cwd: &Path) -> Option<String> {
    let encoded = encode_cwd_for_claude_projects(cwd);
    let project_dir = home.join(".claude").join("projects").join(&encoded);
    newest_jsonl_session_id(&project_dir)
}

/// Walk a single project dir for the newest `*.jsonl` and return the
/// filename stem. Pulled out so the unit tests can exercise the
/// most-recent-mtime tie-breaking without standing up a fake home dir.
pub fn newest_jsonl_session_id(project_dir: &Path) -> Option<String> {
    let entries = std::fs::read_dir(project_dir).ok()?;
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().map(|e| e == "jsonl").unwrap_or(false) {
            let mtime = path
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            match &newest {
                Some((cur, _)) if *cur >= mtime => {}
                _ => newest = Some((mtime, path)),
            }
        }
    }
    let (_, path) = newest?;
    let stem = path.file_stem().and_then(|s| s.to_str())?;
    if stem.is_empty() {
        return None;
    }
    Some(stem.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, SystemTime};

    fn unique_tmp(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "budi-session-resolve-{name}-{}-{stamp}",
            std::process::id()
        ))
    }

    #[test]
    fn encode_replaces_slash_dot_underscore_with_dash() {
        let encoded = encode_cwd_for_claude_projects(Path::new("/Users/me/_proj/foo.bar"));
        assert_eq!(encoded, "-Users-me--proj-foo-bar");
    }

    #[test]
    fn encode_preserves_alphanumerics() {
        let encoded = encode_cwd_for_claude_projects(Path::new("/abc123/XYZ"));
        assert_eq!(encoded, "-abc123-XYZ");
    }

    #[test]
    fn newest_jsonl_picks_most_recent_mtime() {
        let dir = unique_tmp("newest-mtime");
        fs::create_dir_all(&dir).unwrap();
        let older = dir.join("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa.jsonl");
        let newer = dir.join("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb.jsonl");

        // Write with explicit mtimes via `File::set_modified` so the
        // ordering is deterministic regardless of filesystem timestamp
        // granularity (some Linux filesystems and macOS HFS+ round to
        // 1 s, which would race a naive `sleep(...)` between writes).
        fs::write(&older, "").unwrap();
        fs::write(&newer, "").unwrap();
        let now = SystemTime::now();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&older)
            .unwrap()
            .set_modified(now - Duration::from_secs(60))
            .unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&newer)
            .unwrap()
            .set_modified(now)
            .unwrap();

        let stem = newest_jsonl_session_id(&dir).expect("found newest");
        assert_eq!(stem, "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn newest_jsonl_returns_none_for_empty_dir() {
        let dir = unique_tmp("empty");
        fs::create_dir_all(&dir).unwrap();
        assert!(newest_jsonl_session_id(&dir).is_none());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn newest_jsonl_returns_none_for_missing_dir() {
        let dir = unique_tmp("missing");
        // Intentionally never created.
        assert!(newest_jsonl_session_id(&dir).is_none());
    }

    #[test]
    fn newest_jsonl_ignores_non_jsonl_files() {
        let dir = unique_tmp("non-jsonl");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("README.md"), "").unwrap();
        fs::write(dir.join("notes.txt"), "").unwrap();
        assert!(newest_jsonl_session_id(&dir).is_none());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn find_current_uses_encoded_cwd_under_home() {
        let home = unique_tmp("find-current");
        let cwd = Path::new("/Users/me/_proj/foo");
        let encoded = encode_cwd_for_claude_projects(cwd);
        let project_dir = home.join(".claude").join("projects").join(&encoded);
        fs::create_dir_all(&project_dir).unwrap();
        let session_file = project_dir.join("11111111-1111-1111-1111-111111111111.jsonl");
        fs::write(&session_file, "").unwrap();

        let resolved = find_current_session_id(&home, cwd).expect("resolves");
        assert_eq!(resolved, "11111111-1111-1111-1111-111111111111");

        fs::remove_dir_all(&home).ok();
    }
}
