use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GitSnapshot {
    pub branch: String,
    pub head: String,
    pub dirty_files: Vec<String>,
}

fn run_git(repo_root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .output()
        .with_context(|| format!("Failed to execute git {}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn run_git_lossy(repo_root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

pub fn branch(repo_root: &Path) -> Result<String> {
    if let Some(value) = run_git_lossy(repo_root, &["branch", "--show-current"]) {
        let cleaned = value.trim();
        if !cleaned.is_empty() {
            return Ok(cleaned.to_string());
        }
    }
    Ok("unknown".to_string())
}

pub fn head_sha(repo_root: &Path) -> Result<String> {
    if let Some(value) = run_git_lossy(repo_root, &["rev-parse", "HEAD"]) {
        let cleaned = value.trim();
        if !cleaned.is_empty() {
            return Ok(cleaned.to_string());
        }
    }
    Ok("UNCOMMITTED".to_string())
}

pub fn dirty_files(repo_root: &Path) -> Result<Vec<String>> {
    let raw = run_git(repo_root, &["status", "--porcelain"])?;
    let mut files = Vec::new();
    for line in raw.lines() {
        if line.len() < 4 {
            continue;
        }
        files.push(line[3..].trim().to_string());
    }
    Ok(files)
}

pub fn snapshot(repo_root: &Path) -> Result<GitSnapshot> {
    Ok(GitSnapshot {
        branch: branch(repo_root)?,
        head: head_sha(repo_root)?,
        dirty_files: dirty_files(repo_root)?,
    })
}

pub fn resolve_file(repo_root: &Path, relative_path: &str) -> PathBuf {
    repo_root.join(relative_path)
}
