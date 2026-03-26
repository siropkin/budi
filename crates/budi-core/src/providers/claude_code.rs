//! Claude Code provider — implements the Provider trait by delegating to
//! existing modules (jsonl, cost, hooks).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::jsonl::{self, ParsedMessage};
use crate::provider::{DiscoveredFile, ModelPricing, Provider};

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

}

// ---------------------------------------------------------------------------
// Extracted helpers (previously in analytics.rs and cost.rs)
// ---------------------------------------------------------------------------

fn claude_home() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".claude"))
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

/// Claude model pricing lookup.
pub fn claude_pricing_for_model(model: &str) -> ModelPricing {
    let m = model.to_lowercase();
    if m.contains("opus-4-6") || m.contains("opus-4-5") {
        ModelPricing {
            input: 5.0,
            output: 25.0,
            cache_write: 6.25,
            cache_read: 0.50,
        }
    } else if m.contains("opus") {
        ModelPricing {
            input: 15.0,
            output: 75.0,
            cache_write: 18.75,
            cache_read: 1.50,
        }
    } else if m.contains("sonnet") {
        ModelPricing {
            input: 3.0,
            output: 15.0,
            cache_write: 3.75,
            cache_read: 0.30,
        }
    } else if m.contains("haiku") {
        ModelPricing {
            input: 1.0,
            output: 5.0,
            cache_write: 1.25,
            cache_read: 0.10,
        }
    } else {
        // Unknown model — use sonnet pricing as a reasonable default
        ModelPricing {
            input: 3.0,
            output: 15.0,
            cache_write: 3.75,
            cache_read: 0.30,
        }
    }
}

