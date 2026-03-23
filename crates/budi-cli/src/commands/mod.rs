use std::path::PathBuf;

use anyhow::Result;
use budi_core::config;

pub mod doctor;
pub mod hook;
pub mod init;
pub mod insights;
pub mod open;
pub mod repo;
pub mod stats;
pub mod statusline;
pub mod sync;
pub mod update;

pub fn resolve_repo_root(candidate: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = candidate {
        return Ok(path);
    }
    let cwd = std::env::current_dir()?;
    config::find_repo_root(&cwd)
}
