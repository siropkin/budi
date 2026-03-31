//! Claude Code provider — implements the Provider trait by delegating to
//! existing modules (jsonl, cost, hooks).

use std::path::{Path, PathBuf};

use anyhow::Result;

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

/// Claude model pricing lookup (per million tokens, USD).
/// Source: https://docs.anthropic.com/en/docs/about-claude/pricing
pub fn claude_pricing_for_model(model: &str) -> ModelPricing {
    let m = model.to_ascii_lowercase();
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
    } else if m.contains("haiku-4-5") || m.contains("haiku-4") {
        ModelPricing {
            input: 1.0,
            output: 5.0,
            cache_write: 1.25,
            cache_read: 0.10,
        }
    } else if m.contains("haiku-3-5") || m.contains("3-5-haiku") {
        ModelPricing {
            input: 0.80,
            output: 4.0,
            cache_write: 1.0,
            cache_read: 0.08,
        }
    } else if m.contains("haiku-3") || m.contains("3-haiku") {
        ModelPricing {
            input: 0.25,
            output: 1.25,
            cache_write: 0.30,
            cache_read: 0.03,
        }
    } else if m.contains("haiku") {
        ModelPricing {
            input: 1.0,
            output: 5.0,
            cache_write: 1.25,
            cache_read: 0.10,
        }
    } else {
        tracing::warn!(
            "Unknown Claude model '{}', using Sonnet default pricing",
            model
        );
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

    // -----------------------------------------------------------------------
    // Pricing validation against official Anthropic pricing page
    // Source: https://platform.claude.com/docs/en/docs/about-claude/pricing
    // Last verified: 2026-03-25
    //
    // Official pricing (per MTok):
    //   Opus 4.6/4.5: input=$5, output=$25, 5m_cache=$6.25, cache_read=$0.50
    //   Opus 4.1/4.0/3: input=$15, output=$75, 5m_cache=$18.75, cache_read=$1.50
    //   Sonnet 4.6/4.5/4.0/3.7: input=$3, output=$15, 5m_cache=$3.75, cache_read=$0.30
    //   Haiku 4.5: input=$1, output=$5, 5m_cache=$1.25, cache_read=$0.10
    //   Haiku 3.5: input=$0.80, output=$4, 5m_cache=$1.00, cache_read=$0.08
    //   Haiku 3: input=$0.25, output=$1.25, 5m_cache=$0.30, cache_read=$0.03
    // -----------------------------------------------------------------------

    /// Verify all official model pricing against Anthropic's published rates.
    #[test]
    fn pricing_matches_official_anthropic_rates() {
        // (model_id, input, output, cache_write, cache_read)
        let official: &[(&str, f64, f64, f64, f64)] = &[
            // Opus 4.6 — all known ID variants
            ("claude-opus-4-6", 5.0, 25.0, 6.25, 0.50),
            ("claude-opus-4-6-20260321", 5.0, 25.0, 6.25, 0.50),
            // Opus 4.5
            ("claude-opus-4-5-20251101", 5.0, 25.0, 6.25, 0.50),
            // Opus 4.1
            ("claude-opus-4-1-20250805", 15.0, 75.0, 18.75, 1.50),
            // Opus 4.0
            ("claude-opus-4-20250514", 15.0, 75.0, 18.75, 1.50),
            // Opus 3 (deprecated)
            ("claude-3-opus-20240229", 15.0, 75.0, 18.75, 1.50),
            // Sonnet 4.6
            ("claude-sonnet-4-6-20260321", 3.0, 15.0, 3.75, 0.30),
            ("claude-sonnet-4-6", 3.0, 15.0, 3.75, 0.30),
            // Sonnet 4.5 — all known ID variants
            ("claude-sonnet-4-5-20241022", 3.0, 15.0, 3.75, 0.30),
            ("claude-sonnet-4-5-20250514", 3.0, 15.0, 3.75, 0.30),
            // Sonnet 4.0
            ("claude-sonnet-4-20250514", 3.0, 15.0, 3.75, 0.30),
            // Sonnet 3.7 (deprecated)
            ("claude-3-7-sonnet-20250219", 3.0, 15.0, 3.75, 0.30),
            // Haiku 4.5
            ("claude-haiku-4-5-20251001", 1.0, 5.0, 1.25, 0.10),
            // Haiku 3.5
            ("claude-3-5-haiku-20241022", 0.80, 4.0, 1.0, 0.08),
            // Haiku 3
            ("claude-3-haiku-20240307", 0.25, 1.25, 0.30, 0.03),
        ];

        for &(model, exp_in, exp_out, exp_cw, exp_cr) in official {
            let p = claude_pricing_for_model(model);
            assert_eq!(p.input, exp_in, "{model}: input mismatch");
            assert_eq!(p.output, exp_out, "{model}: output mismatch");
            assert_eq!(p.cache_write, exp_cw, "{model}: cache_write mismatch");
            assert_eq!(p.cache_read, exp_cr, "{model}: cache_read mismatch");
        }
    }

    /// Verify cache write pricing uses 5-minute tier (1.25x base input).
    /// Anthropic also offers 1-hour cache (2x base input) but Claude Code
    /// currently uses only 5-minute caching. Verified 2026-03-25.
    #[test]
    fn cache_write_is_1_25x_base_input() {
        let models = [
            "claude-opus-4-6",
            "claude-sonnet-4-6",
            "claude-haiku-4-5-20251001",
        ];
        for model in models {
            let p = claude_pricing_for_model(model);
            let expected_cache_write = p.input * 1.25;
            assert!(
                (p.cache_write - expected_cache_write).abs() < 0.001,
                "{model}: cache_write should be 1.25x input ({expected_cache_write}), got {}",
                p.cache_write
            );
        }
    }

    /// Verify Opus 4.6 1M context variant has same pricing as base Opus 4.6.
    /// Claude Code currently uses "claude-opus-4-6[1m]" model ID which maps to
    /// "claude-opus-4-6" in JSONL — verify our matcher handles both.
    #[test]
    fn pricing_opus_4_6_1m_context() {
        // The JSONL strips "[1m]" — but if it ever appears, verify it still matches
        let p = claude_pricing_for_model("claude-opus-4-6");
        let p1m = claude_pricing_for_model("claude-opus-4-6[1m]");
        // Both should contain "opus-4-6" and match the same pricing
        assert_eq!(p.input, p1m.input);
        assert_eq!(p.output, p1m.output);
    }

    /// Verify cache read pricing is 0.1x base input.
    #[test]
    fn cache_read_is_0_1x_base_input() {
        let models = [
            "claude-opus-4-6",
            "claude-sonnet-4-6",
            "claude-haiku-4-5-20251001",
        ];
        for model in models {
            let p = claude_pricing_for_model(model);
            let expected_cache_read = p.input * 0.10;
            assert!(
                (p.cache_read - expected_cache_read).abs() < 0.001,
                "{model}: cache_read should be 0.1x input ({expected_cache_read}), got {}",
                p.cache_read
            );
        }
    }

    /// Verify that a realistic message produces the expected cost.
    /// Uses real-world token counts from Claude Code JSONL transcripts.
    #[test]
    fn realistic_opus_message_cost() {
        // Real example: input=3, output=32, cache_create=13460, cache_read=12720
        let p = claude_pricing_for_model("claude-opus-4-6");
        let cost = 3.0 * p.input / 1_000_000.0
            + 32.0 * p.output / 1_000_000.0
            + 13460.0 * p.cache_write / 1_000_000.0
            + 12720.0 * p.cache_read / 1_000_000.0;
        // input: 3 * $5/M = $0.000015
        // output: 32 * $25/M = $0.0008
        // cache_write: 13460 * $6.25/M = $0.084125
        // cache_read: 12720 * $0.50/M = $0.00636
        let expected = 0.000015 + 0.0008 + 0.084125 + 0.00636;
        assert!(
            (cost - expected).abs() < 0.000001,
            "cost {cost} vs expected {expected}"
        );
    }
}
