use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use budi_core::config;

use crate::daemon::{ensure_daemon_running, fetch_status_snapshot};

pub fn cmd_status(repo_root: Option<PathBuf>) -> Result<()> {
    let repo_root = super::resolve_repo_root(repo_root)?;
    let config = config::load_or_default(&repo_root)?;
    ensure_daemon_running(&repo_root, &config)?;
    let response =
        fetch_status_snapshot(&config.daemon_base_url(), &repo_root.display().to_string())
            .context("Status endpoint returned error")?;

    println!("budi daemon {}", response.daemon_version);
    println!("repo: {}", response.repo_root);
    println!("hooks detected: {}", response.hooks_detected);
    Ok(())
}

// ─── Repo Management ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepoStorageEntryKind {
    Active,
    Stale,
    MarkerMissing,
}

#[derive(Debug, Clone)]
struct RepoStorageEntry {
    repo_id: String,
    data_dir: PathBuf,
    marker_repo_root: Option<PathBuf>,
    kind: RepoStorageEntryKind,
}

fn collect_repo_storage_entries() -> Result<(PathBuf, Vec<RepoStorageEntry>)> {
    let repos_root = config::repos_root_dir()?;
    if !repos_root.exists() {
        return Ok((repos_root, Vec::new()));
    }

    let mut entries = Vec::new();
    for entry in fs::read_dir(&repos_root)
        .with_context(|| format!("Failed reading {}", repos_root.display()))?
    {
        let entry = entry?;
        let data_dir = entry.path();
        if !data_dir.is_dir() {
            continue;
        }
        let repo_id = entry.file_name().to_string_lossy().to_string();
        let marker_repo_root = config::read_repo_root_marker(&data_dir);
        let kind = match marker_repo_root.as_ref() {
            Some(repo_root) if repo_root.join(".git").exists() => RepoStorageEntryKind::Active,
            Some(_) => RepoStorageEntryKind::Stale,
            None => RepoStorageEntryKind::MarkerMissing,
        };
        entries.push(RepoStorageEntry {
            repo_id,
            data_dir,
            marker_repo_root,
            kind,
        });
    }
    entries.sort_by(|left, right| left.repo_id.cmp(&right.repo_id));
    Ok((repos_root, entries))
}

pub fn cmd_repo_list(stale_only: bool) -> Result<()> {
    let (repos_root, entries) = collect_repo_storage_entries()?;
    if entries.is_empty() {
        println!("No local repo storage found at {}", repos_root.display());
        return Ok(());
    }
    let scanned = entries.len();
    let active = entries
        .iter()
        .filter(|e| e.kind == RepoStorageEntryKind::Active)
        .count();
    let stale = entries
        .iter()
        .filter(|e| e.kind == RepoStorageEntryKind::Stale)
        .count();
    let marker_missing = entries
        .iter()
        .filter(|e| e.kind == RepoStorageEntryKind::MarkerMissing)
        .count();

    println!("repo storage root: {}", repos_root.display());
    println!(
        "scanned={} active={} stale={} unknown_without_marker={}",
        scanned, active, stale, marker_missing
    );
    let filtered: Vec<_> = entries
        .iter()
        .filter(|e| !stale_only || e.kind == RepoStorageEntryKind::Stale)
        .collect();
    if filtered.is_empty() {
        if stale_only {
            println!("No stale repo state directories found.");
        } else {
            println!("No repo state directories found.");
        }
        return Ok(());
    }
    for entry in filtered {
        match entry.kind {
            RepoStorageEntryKind::Active => {
                if let Some(root) = &entry.marker_repo_root {
                    println!(
                        "- active  {} data_dir={} repo_root={}",
                        entry.repo_id,
                        entry.data_dir.display(),
                        root.display()
                    );
                }
            }
            RepoStorageEntryKind::Stale => {
                if let Some(root) = &entry.marker_repo_root {
                    println!(
                        "- stale   {} data_dir={} repo_root={}",
                        entry.repo_id,
                        entry.data_dir.display(),
                        root.display()
                    );
                }
            }
            RepoStorageEntryKind::MarkerMissing => {
                println!(
                    "- unknown {} data_dir={} repo_root=missing-marker",
                    entry.repo_id,
                    entry.data_dir.display()
                );
            }
        }
    }
    Ok(())
}

pub fn cmd_repo_remove(repo_root: PathBuf, dry_run: bool) -> Result<()> {
    let locator = if repo_root.is_absolute() {
        repo_root
    } else {
        std::env::current_dir()
            .context("Failed resolving current directory")?
            .join(repo_root)
    };
    let data_dir = config::repo_paths(&locator)?.data_dir;
    if !data_dir.exists() {
        println!("No local repo state found at {}", data_dir.display());
        return Ok(());
    }
    if dry_run {
        println!("Dry run: would remove {}", data_dir.display());
        return Ok(());
    }
    fs::remove_dir_all(&data_dir)
        .with_context(|| format!("Failed removing repo state {}", data_dir.display()))?;
    println!("Removed repo state {}", data_dir.display());
    Ok(())
}

pub fn cmd_repo_wipe(confirm: bool, dry_run: bool) -> Result<()> {
    let (repos_root, entries) = collect_repo_storage_entries()?;
    if entries.is_empty() {
        println!("No local repo storage found at {}", repos_root.display());
        return Ok(());
    }
    if !confirm {
        anyhow::bail!("Refusing to wipe repo storage without --confirm");
    }
    if dry_run {
        println!(
            "Dry run: would remove {} repo state directorie(s) from {}",
            entries.len(),
            repos_root.display()
        );
        for entry in &entries {
            println!(
                "- {} ({})",
                entry.data_dir.display(),
                match entry.kind {
                    RepoStorageEntryKind::Active => "active",
                    RepoStorageEntryKind::Stale => "stale",
                    RepoStorageEntryKind::MarkerMissing => "unknown",
                }
            );
        }
        return Ok(());
    }
    let mut removed = 0usize;
    for entry in entries {
        fs::remove_dir_all(&entry.data_dir)
            .with_context(|| format!("Failed removing repo state {}", entry.data_dir.display()))?;
        removed = removed.saturating_add(1);
    }
    println!("Removed {} repo state directorie(s).", removed);
    Ok(())
}
