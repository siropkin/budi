//! Resolve a working directory to a canonical repository identity.
//!
//! Resolution logic:
//! 1. Git + remote origin → normalized URL (e.g. `github.com/user/repo`)
//! 2. Git + no remote → git root folder name
//! 3. No git → current folder name

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Resolve a cwd path to a canonical repo_id string.
pub fn resolve_repo_id(cwd: &Path) -> String {
    // Find git root
    let Some(git_root) = find_git_root(cwd) else {
        // No git — use folder name
        return folder_name(cwd);
    };

    // Resolve worktrees to main repo root
    let storage_root = crate::config::resolve_storage_root(&git_root);

    // Try to get remote origin URL
    if let Some(url) = git_remote_origin(&storage_root) {
        return normalize_git_url(&url);
    }

    // Git but no remote — use root folder name
    folder_name(&storage_root)
}

/// Cache for repo_id resolution to avoid repeated git calls during sync.
#[derive(Default)]
pub struct RepoIdCache {
    cache: HashMap<PathBuf, String>,
}

impl RepoIdCache {
    pub fn new() -> Self {
        Self {
            cache: HashMap::new(),
        }
    }

    pub fn resolve(&mut self, cwd: &Path) -> String {
        if let Some(id) = self.cache.get(cwd) {
            return id.clone();
        }
        let id = resolve_repo_id(cwd);
        self.cache.insert(cwd.to_path_buf(), id.clone());
        id
    }
}

/// Walk up from `start` to find a directory containing `.git`.
fn find_git_root(start: &Path) -> Option<PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        if current.join(".git").exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Run `git remote get-url origin` in the given directory.
fn git_remote_origin(repo_root: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(repo_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if url.is_empty() { None } else { Some(url) }
}

/// Normalize a git remote URL to `host/owner/repo` form.
///
/// Handles:
/// - `https://github.com/user/repo.git` → `github.com/user/repo`
/// - `git@github.com:user/repo.git` → `github.com/user/repo`
/// - `ssh://git@github.com/user/repo` → `github.com/user/repo`
fn normalize_git_url(url: &str) -> String {
    let url = url.trim();

    // SSH shorthand: git@github.com:user/repo.git
    if let Some(rest) = url.strip_prefix("git@") {
        let normalized = rest.replace(':', "/");
        return strip_git_suffix(&normalized);
    }

    // Protocol URLs: https://..., ssh://git@..., git://...
    let without_protocol = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .or_else(|| url.strip_prefix("ssh://"))
        .or_else(|| url.strip_prefix("git://"))
        .unwrap_or(url);

    // Strip auth (user@host/... or git@host/...)
    let without_auth = if let Some(pos) = without_protocol.find('@') {
        &without_protocol[pos + 1..]
    } else {
        without_protocol
    };

    strip_git_suffix(without_auth)
}

fn strip_git_suffix(s: &str) -> String {
    let s = s.strip_suffix(".git").unwrap_or(s);
    // Collapse any double slashes (e.g. github.com//user/repo)
    let mut result = String::with_capacity(s.len());
    let mut prev_slash = false;
    for ch in s.chars() {
        if ch == '/' && prev_slash {
            continue;
        }
        prev_slash = ch == '/';
        result.push(ch);
    }
    result
}

fn folder_name(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_https_url() {
        assert_eq!(
            normalize_git_url("https://github.com/siropkin/budi.git"),
            "github.com/siropkin/budi"
        );
    }

    #[test]
    fn normalize_https_no_suffix() {
        assert_eq!(
            normalize_git_url("https://github.com/siropkin/budi"),
            "github.com/siropkin/budi"
        );
    }

    #[test]
    fn normalize_ssh_shorthand() {
        assert_eq!(
            normalize_git_url("git@github.com:siropkin/budi.git"),
            "github.com/siropkin/budi"
        );
    }

    #[test]
    fn normalize_ssh_protocol() {
        assert_eq!(
            normalize_git_url("ssh://git@github.com/siropkin/budi.git"),
            "github.com/siropkin/budi"
        );
    }

    #[test]
    fn normalize_git_protocol() {
        assert_eq!(
            normalize_git_url("git://github.com/siropkin/budi.git"),
            "github.com/siropkin/budi"
        );
    }

    #[test]
    fn folder_name_extracts_last_component() {
        assert_eq!(
            folder_name(Path::new("/home/user/my-project")),
            "my-project"
        );
    }

    #[test]
    fn cache_returns_consistent_results() {
        let mut cache = RepoIdCache::new();
        // Non-git directory — should use folder name
        let id1 = cache.resolve(Path::new("/tmp"));
        let id2 = cache.resolve(Path::new("/tmp"));
        assert_eq!(id1, id2);
        assert_eq!(id1, "tmp");
    }
}
