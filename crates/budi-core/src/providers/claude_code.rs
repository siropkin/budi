//! Claude Code provider — implements the Provider trait by delegating to
//! existing modules (jsonl, cost, hooks).

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::jsonl::{self, ParsedMessage};
use crate::provider::{DiscoveredFile, Provider};

/// The Claude Code provider.
pub struct ClaudeCodeProvider;

impl Provider for ClaudeCodeProvider {
    fn name(&self) -> &'static str {
        "claude_code"
    }

    fn display_name(&self) -> &'static str {
        "Claude Code"
    }

    fn is_available(&self) -> bool {
        claude_home().map(|p| p.exists()).unwrap_or(false)
    }

    fn discover_files(&self) -> Result<Vec<DiscoveredFile>> {
        let files = discover_jsonl_files()?;
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
        Ok(jsonl::parse_transcript(content, offset))
    }

    fn watch_roots(&self) -> Vec<PathBuf> {
        let Ok(home) = crate::config::home_dir() else {
            return Vec::new();
        };
        watch_roots_for_home(&home)
    }
}

/// Compute Claude Code's tailer watch roots relative to the given home dir.
///
/// Claude Code writes JSONL transcripts under `~/.claude/projects/<encoded-cwd>/*.jsonl`.
/// The daemon's tailer attaches a recursive watcher to `~/.claude/projects`,
/// so this function returns that single root when it exists. Returning an
/// empty vector when the directory is absent lets the daemon skip the
/// watcher rather than failing to start.
fn watch_roots_for_home(home: &Path) -> Vec<PathBuf> {
    let projects = home.join(".claude").join("projects");
    if projects.is_dir() {
        vec![projects]
    } else {
        Vec::new()
    }
}

// ---------------------------------------------------------------------------
// Extracted helpers (previously in analytics.rs and cost.rs)
// ---------------------------------------------------------------------------

fn claude_home() -> Result<PathBuf> {
    Ok(crate::config::home_dir()?.join(".claude"))
}

/// Discover all Claude Code JSONL transcript files under `~/.claude/projects/`.
pub(crate) fn discover_jsonl_files() -> Result<Vec<PathBuf>> {
    let claude_dir = claude_home()?.join("projects");
    let mut files = Vec::new();
    collect_jsonl_recursive(&claude_dir, &mut files, 0);
    // Sort by modification time descending (newest first) so that the most
    // recent transcripts are synced first — this gives progressive first-sync
    // UX where today's data appears in seconds instead of waiting for full
    // history to be processed.
    files.sort_by(|a, b| {
        let mtime = |p: &PathBuf| {
            p.metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
        };
        mtime(b).cmp(&mtime(a))
    });
    Ok(files)
}

fn collect_jsonl_recursive(dir: &Path, files: &mut Vec<PathBuf>, depth: u32) {
    if depth > 4 {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // HOME env mutation must be serialized — multiple tests mutate it.
    static HOME_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct HomeGuard {
        prev: Option<String>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl HomeGuard {
        fn new(home: &Path) -> Self {
            let lock = HOME_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            let prev = std::env::var("HOME").ok();
            unsafe { std::env::set_var("HOME", home) };
            Self { prev, _lock: lock }
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(h) => unsafe { std::env::set_var("HOME", h) },
                None => unsafe { std::env::remove_var("HOME") },
            }
        }
    }

    fn fresh_tmp(name: &str) -> PathBuf {
        let tmp = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        tmp
    }

    #[test]
    fn watch_roots_returns_projects_dir_when_present() {
        let tmp = fresh_tmp("budi-claude-watch-roots-present");
        std::fs::create_dir_all(tmp.join(".claude/projects")).unwrap();

        let roots = watch_roots_for_home(&tmp);
        assert_eq!(roots, vec![tmp.join(".claude/projects")]);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn watch_roots_empty_when_projects_dir_absent() {
        let tmp = fresh_tmp("budi-claude-watch-roots-absent");

        let roots = watch_roots_for_home(&tmp);
        assert!(roots.is_empty(), "expected empty roots, got {roots:?}");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // -------------------------------------------------------------------
    // Provider trait surface
    // -------------------------------------------------------------------

    #[test]
    fn provider_identity() {
        let p = ClaudeCodeProvider;
        assert_eq!(p.name(), "claude_code");
        assert_eq!(p.display_name(), "Claude Code");
    }

    #[test]
    fn is_available_reflects_claude_home_presence() {
        let tmp = fresh_tmp("budi-claude-is-available");
        let _guard = HomeGuard::new(&tmp);

        let p = ClaudeCodeProvider;
        assert!(!p.is_available(), "no .claude dir → unavailable");

        std::fs::create_dir_all(tmp.join(".claude")).unwrap();
        assert!(p.is_available(), ".claude dir exists → available");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn watch_roots_uses_resolved_home() {
        let tmp = fresh_tmp("budi-claude-watch-roots-via-home");
        std::fs::create_dir_all(tmp.join(".claude/projects")).unwrap();
        let _guard = HomeGuard::new(&tmp);

        let roots = ClaudeCodeProvider.watch_roots();
        assert_eq!(roots, vec![tmp.join(".claude/projects")]);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn discover_files_walks_projects_subdirectories() {
        let tmp = fresh_tmp("budi-claude-discover");
        let projects = tmp.join(".claude/projects");
        // Two encoded-cwd dirs, each with one .jsonl file plus a non-jsonl file.
        let a = projects.join("-tmp-project-a");
        let b = projects.join("-tmp-project-b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("sess-1.jsonl"), "").unwrap();
        std::fs::write(a.join("notes.txt"), "ignored").unwrap();
        std::fs::write(b.join("sess-2.jsonl"), "").unwrap();

        let _guard = HomeGuard::new(&tmp);
        let mut paths: Vec<_> = ClaudeCodeProvider
            .discover_files()
            .unwrap()
            .into_iter()
            .map(|f| f.path)
            .collect();
        paths.sort();
        assert_eq!(paths, vec![a.join("sess-1.jsonl"), b.join("sess-2.jsonl")]);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn discover_jsonl_files_orders_by_mtime_descending() {
        let tmp = fresh_tmp("budi-claude-discover-mtime");
        let projects = tmp.join(".claude/projects/-tmp-x");
        std::fs::create_dir_all(&projects).unwrap();
        let older = projects.join("older.jsonl");
        let newer = projects.join("newer.jsonl");
        std::fs::write(&older, "").unwrap();
        // Ensure measurable mtime gap on filesystems with coarse timestamps.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&newer, "").unwrap();

        let _guard = HomeGuard::new(&tmp);
        let files = discover_jsonl_files().unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0], newer, "newest file must come first");
        assert_eq!(files[1], older);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn collect_jsonl_recursive_respects_depth_limit() {
        // Build a 6-level-deep tree under root: root/d1/d2/d3/d4/d5/deep.jsonl,
        // plus a sibling .jsonl at root level. collect_jsonl_recursive only
        // recurses up to depth 4 (depth starts at 0), so the depth-5 file is
        // unreachable.
        let tmp = fresh_tmp("budi-claude-depth");
        let shallow = tmp.join("shallow.jsonl");
        std::fs::write(&shallow, "").unwrap();
        let mut deep_path = tmp.clone();
        for d in 1..=5 {
            deep_path = deep_path.join(format!("d{d}"));
            std::fs::create_dir_all(&deep_path).unwrap();
        }
        let deep_file = deep_path.join("deep.jsonl");
        std::fs::write(&deep_file, "").unwrap();

        let mut out = Vec::new();
        collect_jsonl_recursive(&tmp, &mut out, 0);
        assert!(out.contains(&shallow), "shallow file should be discovered");
        assert!(
            !out.contains(&deep_file),
            "depth-5 file should be skipped by the depth limit"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn collect_jsonl_recursive_ignores_unreadable_root() {
        let mut out = Vec::new();
        // Non-existent path returns early without panicking.
        collect_jsonl_recursive(Path::new("/nonexistent-budi-claude-test"), &mut out, 0);
        assert!(out.is_empty());
    }

    // -------------------------------------------------------------------
    // Fixture-based parser tests (acceptance criteria — 3 representative
    // message shapes parsed end to end via the Provider::parse_file seam).
    // -------------------------------------------------------------------

    #[test]
    fn parse_file_fixture_assistant_text_turn() {
        // Plain assistant text turn — the most common shape.
        let content = concat!(
            r#"{"parentUuid":null,"isSidechain":false,"type":"user","message":{"role":"user","content":"hello"},"uuid":"u-1","timestamp":"2026-04-01T12:00:00.000Z","sessionId":"sess-A","cwd":"/work/repo","gitBranch":"main"}"#,
            "\n",
            r#"{"parentUuid":"u-1","isSidechain":false,"type":"assistant","message":{"model":"claude-opus-4-6","id":"req-A","type":"message","role":"assistant","content":[{"type":"text","text":"hi there"}],"stop_reason":"end_turn","usage":{"input_tokens":42,"output_tokens":17,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}},"uuid":"a-1","timestamp":"2026-04-01T12:00:01.000Z","sessionId":"sess-A","cwd":"/work/repo","gitBranch":"main"}"#,
            "\n",
        );

        let provider = ClaudeCodeProvider;
        let (msgs, offset) = provider
            .parse_file(Path::new("/fake/path.jsonl"), content, 0)
            .unwrap();
        assert_eq!(
            offset,
            content.len(),
            "offset should advance past both lines"
        );
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[0].git_branch.as_deref(), Some("main"));
        assert_eq!(msgs[1].role, "assistant");
        assert_eq!(msgs[1].model.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(msgs[1].input_tokens, 42);
        assert_eq!(msgs[1].output_tokens, 17);
        assert_eq!(msgs[1].request_id.as_deref(), Some("req-A"));
    }

    #[test]
    fn parse_file_fixture_tool_use_turn() {
        // Assistant tool-use turn: surfaces tool_names / tool_use_ids.
        let content = concat!(
            r#"{"parentUuid":"u-1","isSidechain":false,"type":"assistant","message":{"model":"claude-opus-4-6","id":"req-T","type":"message","role":"assistant","content":[{"type":"text","text":"reading"},{"type":"tool_use","id":"tu-1","name":"Read","input":{"path":"x"}}],"stop_reason":"tool_use","usage":{"input_tokens":100,"output_tokens":40,"cache_creation_input_tokens":200,"cache_read_input_tokens":300}},"uuid":"a-2","timestamp":"2026-04-01T12:00:02.000Z","sessionId":"sess-B","cwd":"/work/repo"}"#,
            "\n",
        );

        let (msgs, _offset) = ClaudeCodeProvider
            .parse_file(Path::new("/fake/path.jsonl"), content, 0)
            .unwrap();
        assert_eq!(msgs.len(), 1);
        let m = &msgs[0];
        assert_eq!(m.role, "assistant");
        assert_eq!(m.tool_names, vec!["Read".to_string()]);
        assert_eq!(m.tool_use_ids, vec!["tu-1".to_string()]);
        assert_eq!(m.cache_creation_tokens, 200);
        assert_eq!(m.cache_read_tokens, 300);
    }

    #[test]
    fn parse_file_fixture_tool_result_error_turn() {
        // User tool-result turn where the tool reported an error — surfaces
        // a `tool_outcomes` entry with outcome=error.
        let content = concat!(
            r#"{"parentUuid":"a-2","isSidechain":false,"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu-1","is_error":true,"content":"file not found"}]},"uuid":"u-2","timestamp":"2026-04-01T12:00:03.000Z","sessionId":"sess-B","cwd":"/work/repo"}"#,
            "\n",
        );

        let (msgs, _offset) = ClaudeCodeProvider
            .parse_file(Path::new("/fake/path.jsonl"), content, 0)
            .unwrap();
        assert_eq!(msgs.len(), 1);
        let m = &msgs[0];
        assert_eq!(m.role, "user");
        assert_eq!(m.tool_outcomes.len(), 1);
        assert_eq!(m.tool_outcomes[0].tool_use_id, "tu-1");
        assert_eq!(m.tool_outcomes[0].outcome, crate::jsonl::TOOL_OUTCOME_ERROR);
    }

    #[test]
    fn parse_file_respects_start_offset() {
        // Verify incremental parsing: starting from a non-zero offset skips
        // already-consumed bytes and only emits the remainder.
        let first = r#"{"parentUuid":null,"isSidechain":false,"type":"user","message":{"role":"user","content":"a"},"uuid":"u-x","timestamp":"2026-04-01T12:00:00.000Z","sessionId":"s"}"#;
        let second = r#"{"parentUuid":null,"isSidechain":false,"type":"user","message":{"role":"user","content":"b"},"uuid":"u-y","timestamp":"2026-04-01T12:00:01.000Z","sessionId":"s"}"#;
        let content = format!("{first}\n{second}\n");
        let start = first.len() + 1; // skip past first line and its newline

        let (msgs, offset) = ClaudeCodeProvider
            .parse_file(Path::new("/fake.jsonl"), &content, start)
            .unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].uuid, "u-y");
        assert_eq!(offset, content.len());
    }
}
