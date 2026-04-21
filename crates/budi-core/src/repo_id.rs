//! Resolve a working directory to a canonical repository identity.
//!
//! Resolution logic (#442):
//! 1. Git + remote origin → `Some("host/owner/repo")` (normalized URL).
//! 2. Anything else (no git, git but no remote, scratch dirs) → `None`.
//!
//! Before 8.3.0 this also returned the git-root folder name or the cwd's
//! folder name when no remote was available, which meant `budi stats
//! --projects` silently mixed real GitHub repos with ad-hoc directories
//! like `Desktop`, `~`, `.cursor`, and brew-tap checkouts. Non-repo work
//! is now rolled up into a single `(no repository)` bucket on the render
//! side.
//!
//! # Design history
//!
//! The original design (2026-03-22, pre-8.0) stored `repo_id` as a
//! SHA-256 hash of the canonical repo root path. The hash successfully
//! dedup'd worktree checkouts against the main checkout, but it was
//! opaque in the dashboard — users saw a hex blob instead of a
//! recognizable project name. 8.3.0 pivots to the normalized
//! `host/owner/repo` URL (this module's current implementation) so
//! stats output and cloud rollups render human-readable project names
//! directly, while worktrees continue to collapse to the main checkout
//! via [`crate::config::resolve_storage_root`]. Commits on a repo with
//! no remote stay in the `(no repository)` bucket until the repo gets
//! pushed somewhere — there is no longer a fallback to a bare folder
//! name. Cloud sync still hashes the normalized URL before leaving the
//! machine per [ADR-0083 §6].
//!
//! [ADR-0083 §6]: ../../../../docs/adr/0083-cloud-ingest-identity-and-privacy-contract.md

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Resolve a cwd path to a canonical repo_id string.
///
/// Returns `None` when the cwd is not inside a git repo with a remote
/// origin. Callers should treat `None` as "non-repository work" and
/// persist it as `NULL` in the analytics DB so it collapses to a single
/// `(no repository)` bucket at query time.
pub fn resolve_repo_id(cwd: &Path) -> Option<String> {
    // Find git root.
    let git_root = find_git_root(cwd)?;

    // Resolve worktrees to main repo root so a detached worktree checkout
    // shares identity with its parent.
    let storage_root = crate::config::resolve_storage_root(&git_root);

    // Require a remote origin — an init'd-but-unpushed local repo stays
    // in the `(no repository)` bucket until it has an upstream.
    let url = git_remote_origin(&storage_root)?;

    Some(normalize_git_url(&url))
}

/// Returns `true` when the given string looks like a normalized
/// `resolve_repo_id` output: at least two `/` separators, and the part
/// before the first `/` contains a `.` (i.e. looks like a host).
///
/// Used by the idempotent 8.3 backfill to distinguish real repo URLs
/// from pre-8.3 bare-folder-name residue.
pub fn looks_like_repo_url(s: &str) -> bool {
    let Some(first_slash) = s.find('/') else {
        return false;
    };
    let host = &s[..first_slash];
    if !host.contains('.') {
        return false;
    }
    // Need at least one more `/` after the host, i.e. `host/owner/repo`.
    s[first_slash + 1..].contains('/')
}

/// Cache for repo_id resolution to avoid repeated git calls during sync.
#[derive(Default)]
pub struct RepoIdCache {
    cache: HashMap<PathBuf, Option<String>>,
}

impl RepoIdCache {
    pub fn new() -> Self {
        Self {
            cache: HashMap::new(),
        }
    }

    pub fn resolve(&mut self, cwd: &Path) -> Option<String> {
        if let Some(id) = self.cache.get(cwd) {
            return id.clone();
        }
        let id = resolve_repo_id(cwd);
        self.cache.insert(cwd.to_path_buf(), id.clone());
        id
    }
}

/// Walk up from `start` to find the enclosing repo root (directory
/// containing `.git`). Used by `FileEnricher` to normalize tool-call
/// file paths against the same root that defines `repo_id`, so the
/// "inside the repo" privacy check matches what analytics see. Added
/// in R1.4 (#292).
pub fn repo_root_for(cwd: &Path) -> Option<PathBuf> {
    find_git_root(cwd).map(|root| crate::config::resolve_storage_root(&root))
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
    fn looks_like_repo_url_accepts_normalized_outputs() {
        assert!(looks_like_repo_url("github.com/siropkin/budi"));
        assert!(looks_like_repo_url("gitlab.com/group/subgroup/project"));
        assert!(looks_like_repo_url("bitbucket.org/user/repo"));
        assert!(looks_like_repo_url("git.sr.ht/~user/repo"));
    }

    #[test]
    fn looks_like_repo_url_rejects_bare_folder_names() {
        // Every example from the #442 repro table.
        assert!(!looks_like_repo_url("Desktop"));
        assert!(!looks_like_repo_url("ivan.seredkin")); // dot but no slash
        assert!(!looks_like_repo_url("budi-cursor"));
        assert!(!looks_like_repo_url(".cursor"));
        assert!(!looks_like_repo_url("homebrew-budi"));
        assert!(!looks_like_repo_url("awesome-vibe-coding-1"));
        // A lone host stays out of the repo set — we require owner/repo.
        assert!(!looks_like_repo_url("github.com"));
        assert!(!looks_like_repo_url("github.com/owner")); // missing repo segment
        // Paths that are URL-shaped but lack a dotted host also stay out.
        assert!(!looks_like_repo_url("local/owner/repo"));
    }

    #[test]
    fn resolve_repo_id_returns_none_for_non_git_paths() {
        // Non-git dirs never get a repo_id, regardless of their name.
        let tmp = std::env::temp_dir();
        assert_eq!(resolve_repo_id(&tmp), None);
    }

    #[test]
    fn cache_returns_consistent_results() {
        let mut cache = RepoIdCache::new();
        let tmp = std::env::temp_dir();
        // Non-git directory — should be None, and stay None on repeat calls.
        let id1 = cache.resolve(&tmp);
        let id2 = cache.resolve(&tmp);
        assert_eq!(id1, id2);
        assert_eq!(id1, None);
    }
}
