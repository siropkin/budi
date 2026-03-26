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

/// Claude model pricing lookup (per million tokens, USD).
/// Source: https://docs.anthropic.com/en/docs/about-claude/pricing
pub fn claude_pricing_for_model(model: &str) -> ModelPricing {
    let m = model.to_lowercase();
    // Opus 4.5/4.6: $5/$25
    if m.contains("opus-4-6") || m.contains("opus-4-5") {
        ModelPricing {
            input: 5.0,
            output: 25.0,
            cache_write: 6.25,
            cache_read: 0.50,
        }
    // Opus 4.0/4.1/3: $15/$75
    } else if m.contains("opus") {
        ModelPricing {
            input: 15.0,
            output: 75.0,
            cache_write: 18.75,
            cache_read: 1.50,
        }
    // All Sonnet variants (3.5/3.7/4/4.5/4.6): $3/$15
    } else if m.contains("sonnet") {
        ModelPricing {
            input: 3.0,
            output: 15.0,
            cache_write: 3.75,
            cache_read: 0.30,
        }
    // Haiku 4.5: $1/$5
    } else if m.contains("haiku-4-5") || m.contains("haiku-4") {
        ModelPricing {
            input: 1.0,
            output: 5.0,
            cache_write: 1.25,
            cache_read: 0.10,
        }
    // Haiku 3.5: $0.80/$4
    } else if m.contains("haiku-3-5") || m.contains("3-5-haiku") {
        ModelPricing {
            input: 0.80,
            output: 4.0,
            cache_write: 1.0,
            cache_read: 0.08,
        }
    // Haiku 3: $0.25/$1.25
    } else if m.contains("haiku-3") || m.contains("3-haiku") {
        ModelPricing {
            input: 0.25,
            output: 1.25,
            cache_write: 0.30,
            cache_read: 0.03,
        }
    // Haiku (unversioned fallback): use Haiku 4.5 pricing
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pricing_opus_4_6() {
        let p = claude_pricing_for_model("claude-opus-4-6");
        assert_eq!(p.input, 5.0);
        assert_eq!(p.output, 25.0);
        assert_eq!(p.cache_write, 6.25);
        assert_eq!(p.cache_read, 0.50);
    }

    #[test]
    fn pricing_opus_4_5() {
        let p = claude_pricing_for_model("claude-opus-4-5-20251101");
        assert_eq!(p.input, 5.0);
        assert_eq!(p.output, 25.0);
    }

    #[test]
    fn pricing_opus_4_1_is_expensive() {
        let p = claude_pricing_for_model("claude-opus-4-1-20250805");
        assert_eq!(p.input, 15.0);
        assert_eq!(p.output, 75.0);
    }

    #[test]
    fn pricing_opus_4_0() {
        let p = claude_pricing_for_model("claude-opus-4-20250514");
        assert_eq!(p.input, 15.0);
        assert_eq!(p.output, 75.0);
    }

    #[test]
    fn pricing_sonnet_4_6() {
        let p = claude_pricing_for_model("claude-sonnet-4-6-20260321");
        assert_eq!(p.input, 3.0);
        assert_eq!(p.output, 15.0);
    }

    #[test]
    fn pricing_haiku_4_5() {
        let p = claude_pricing_for_model("claude-haiku-4-5-20251001");
        assert_eq!(p.input, 1.0);
        assert_eq!(p.output, 5.0);
    }

    #[test]
    fn pricing_haiku_3_5() {
        let p = claude_pricing_for_model("claude-3-5-haiku-20241022");
        assert_eq!(p.input, 0.80);
        assert_eq!(p.output, 4.0);
        assert_eq!(p.cache_write, 1.0);
        assert_eq!(p.cache_read, 0.08);
    }

    #[test]
    fn pricing_haiku_3() {
        let p = claude_pricing_for_model("claude-3-haiku-20240307");
        assert_eq!(p.input, 0.25);
        assert_eq!(p.output, 1.25);
        assert_eq!(p.cache_write, 0.30);
        assert_eq!(p.cache_read, 0.03);
    }

    #[test]
    fn pricing_unknown_defaults_to_sonnet() {
        let p = claude_pricing_for_model("some-unknown-model");
        assert_eq!(p.input, 3.0);
        assert_eq!(p.output, 15.0);
    }

    #[test]
    fn pricing_synthetic_defaults_to_sonnet() {
        let p = claude_pricing_for_model("<synthetic>");
        assert_eq!(p.input, 3.0);
    }
}

