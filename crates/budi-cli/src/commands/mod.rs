use std::path::PathBuf;

use budi_core::config;

pub mod doctor;
pub mod hook;
pub mod init;
pub mod open;
pub mod stats;
pub mod statusline;
pub mod sync;
pub mod update;

/// Try to resolve a repo root, but return None if not in a git repository.
pub fn try_resolve_repo_root(candidate: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(path) = candidate {
        return Some(path);
    }
    let cwd = std::env::current_dir().ok()?;
    config::find_repo_root(&cwd).ok()
}
