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

    #[test]
    fn watch_roots_returns_projects_dir_when_present() {
        let tmp = std::env::temp_dir().join("budi-claude-watch-roots-present");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join(".claude/projects")).unwrap();

        let roots = watch_roots_for_home(&tmp);
        assert_eq!(roots, vec![tmp.join(".claude/projects")]);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn watch_roots_empty_when_projects_dir_absent() {
        let tmp = std::env::temp_dir().join("budi-claude-watch-roots-absent");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let roots = watch_roots_for_home(&tmp);
        assert!(roots.is_empty(), "expected empty roots, got {roots:?}");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
