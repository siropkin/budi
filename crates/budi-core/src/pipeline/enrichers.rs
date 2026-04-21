//! Pipeline enrichers: Git, Identity, Cost, Tag.

use std::path::Path;

use crate::analytics::Tag;
use crate::config::TagsConfig;
use crate::file_attribution;
use crate::jsonl::ParsedMessage;
use crate::pipeline::{Enricher, emit, extract_ticket_from_branch, glob_match};
use crate::repo_id::{RepoIdCache, repo_root_for};
use crate::tag_keys as tk;

// ---------------------------------------------------------------------------
// GitEnricher — resolves repo_id from cwd, extracts ticket_id from branch
// ---------------------------------------------------------------------------

pub struct GitEnricher {
    repo_cache: RepoIdCache,
}

impl Default for GitEnricher {
    fn default() -> Self {
        Self::new()
    }
}

impl GitEnricher {
    pub fn new() -> Self {
        Self {
            repo_cache: RepoIdCache::new(),
        }
    }
}

impl Enricher for GitEnricher {
    fn enrich(&mut self, msg: &mut ParsedMessage) -> Vec<Tag> {
        let mut tags = Vec::new();

        // Resolve repo_id from cwd. #442: `resolve_repo_id` now returns
        // `None` when the cwd is not inside a git repo with a remote
        // origin, so non-repo work (scratch dirs, `~/Desktop`, brew
        // checkouts) stays NULL and rolls up into a single `(no
        // repository)` bucket on render.
        if msg.repo_id.is_none() {
            if msg.cwd.is_none() {
                tracing::debug!(
                    "GitEnricher: no cwd for message {}, skipping repo resolution",
                    msg.uuid
                );
            }
            if let Some(ref cwd) = msg.cwd {
                msg.repo_id = self.repo_cache.resolve(Path::new(cwd));
            }
        }

        // Extract ticket_id from git_branch (branch itself is stored as a
        // column, not a tag). R1.3 (#221) unified the extractor so live
        // tailing and `budi db import` tag pure-numeric branches like
        // `fix/1234-typo` consistently, while staying readable against
        // retained 8.1 legacy history. #335: emit the triplet through the
        // shared helper so a future caller cannot land a `ticket_id`
        // without its sibling `ticket_source` tag.
        if let Some(ref branch) = msg.git_branch
            && let Some((ticket, source)) = extract_ticket_from_branch(branch)
        {
            emit::ticket(&mut tags, &ticket, source);
        }

        tags
    }
}

// ---------------------------------------------------------------------------
// ToolEnricher — emits per-message tool tags from parsed tool names
// ---------------------------------------------------------------------------

pub struct ToolEnricher;

impl Enricher for ToolEnricher {
    fn enrich(&mut self, msg: &mut ParsedMessage) -> Vec<Tag> {
        if msg.role != "assistant" {
            return Vec::new();
        }

        let mut dedup = std::collections::HashSet::new();
        let mut tags = Vec::new();
        for tool in &msg.tool_names {
            let normalized = tool.trim();
            if normalized.is_empty() {
                continue;
            }
            if dedup.insert(normalized.to_string()) {
                tags.push(Tag {
                    key: tk::TOOL.to_string(),
                    value: normalized.to_string(),
                });
            }
        }

        let mut dedup_ids = std::collections::HashSet::new();
        for tool_use_id in &msg.tool_use_ids {
            let normalized = tool_use_id.trim();
            if normalized.is_empty() {
                continue;
            }
            if dedup_ids.insert(normalized.to_string()) {
                tags.push(Tag {
                    key: tk::TOOL_USE_ID.to_string(),
                    value: normalized.to_string(),
                });
            }
        }
        tags
    }
}

// ---------------------------------------------------------------------------
// FileEnricher — turns raw tool_files into repo-relative file_path tags
// ---------------------------------------------------------------------------

/// Emits per-file tags from `ParsedMessage::tool_files` after normalizing
/// each path against the message's `cwd` / resolved repo root. Always
/// runs **after** `GitEnricher` so `cwd` and `repo_id` are set, and so
/// the repo-root walk resolves the same identity analytics will join
/// against. See R1.4 (#292) and ADR-0083.
///
/// Emitted tags:
/// - `file_path` — one per accepted file (multi-valued tag).
/// - `file_path_source` — dominant source (`tool_arg` or `cwd_relative`).
/// - `file_path_confidence` — dominant confidence (`high` or `medium`).
///
/// The source/confidence pair is recorded once per message because in
/// practice all files on a given assistant message came from the same
/// extractor pass and share the same provenance. Sibling tags mirror
/// the R1.2 (#222) `activity_source` / `activity_confidence` shape so
/// downstream queries can use the same pattern.
pub struct FileEnricher {
    /// Per-cwd cache so repeated messages in a session don't re-walk the
    /// filesystem for every tool-use block.
    cache: std::collections::HashMap<String, Option<std::path::PathBuf>>,
}

impl Default for FileEnricher {
    fn default() -> Self {
        Self::new()
    }
}

impl FileEnricher {
    pub fn new() -> Self {
        Self {
            cache: std::collections::HashMap::new(),
        }
    }

    fn repo_root(&mut self, cwd: &str) -> Option<std::path::PathBuf> {
        if let Some(hit) = self.cache.get(cwd) {
            return hit.clone();
        }
        let resolved = repo_root_for(std::path::Path::new(cwd));
        self.cache.insert(cwd.to_string(), resolved.clone());
        resolved
    }
}

impl Enricher for FileEnricher {
    fn enrich(&mut self, msg: &mut ParsedMessage) -> Vec<Tag> {
        if msg.role != "assistant" || msg.tool_files.is_empty() {
            return Vec::new();
        }
        let repo_root = msg.cwd.as_deref().and_then(|cwd| self.repo_root(cwd));
        let attribution = file_attribution::attribute_files(
            &msg.tool_files,
            msg.cwd.as_deref(),
            repo_root.as_deref(),
        );
        if attribution.paths.is_empty() {
            return Vec::new();
        }
        // `attribute_files` guarantees that `source` / `confidence` are
        // `Some` whenever `paths` is non-empty; the `expect` calls pin
        // that invariant so the shared helper (#335) can take
        // non-optional siblings without smuggling in a silent-fallback
        // path.
        let source = attribution
            .source
            .expect("attribute_files sets source when paths is non-empty");
        let confidence = attribution
            .confidence
            .expect("attribute_files sets confidence when paths is non-empty");
        let mut tags = Vec::new();
        emit::file_paths(&mut tags, attribution.paths, source, confidence);
        tags
    }
}

// ---------------------------------------------------------------------------
// IdentityEnricher — emits explicit local identity tags
// ---------------------------------------------------------------------------

pub struct IdentityEnricher {
    user_name: String,
    machine_name: String,
    platform: String,
    git_user: String,
}

impl Default for IdentityEnricher {
    fn default() -> Self {
        Self::new()
    }
}

impl IdentityEnricher {
    pub fn new() -> Self {
        let user_name = std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_default();
        let machine_name = get_hostname();
        let platform = std::env::consts::OS.to_string();
        let git_user = get_git_user_identity();
        Self {
            user_name,
            machine_name,
            platform,
            git_user,
        }
    }
}

fn get_hostname() -> String {
    // Fast paths that avoid spawning a subprocess
    if let Ok(h) = std::env::var("HOSTNAME")
        && !h.is_empty()
    {
        return h;
    }
    if let Ok(h) = std::fs::read_to_string("/etc/hostname") {
        let trimmed = h.trim().to_string();
        if !trimmed.is_empty() {
            return trimmed;
        }
    }
    // Fallback: hostname command (macOS, other Unix, Windows)
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if s.is_empty() { None } else { Some(s) }
            } else {
                None
            }
        })
        .unwrap_or_default()
}

fn read_git_config(args: &[&str]) -> Option<String> {
    std::process::Command::new("git")
        .args(args)
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if value.is_empty() { None } else { Some(value) }
            } else {
                None
            }
        })
}

fn non_empty_env(var_name: &str) -> Option<String> {
    std::env::var(var_name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn get_git_user_identity() -> String {
    non_empty_env("GIT_AUTHOR_NAME")
        .or_else(|| read_git_config(&["config", "--get", "user.name"]))
        .or_else(|| read_git_config(&["config", "--global", "--get", "user.name"]))
        .or_else(|| non_empty_env("GIT_AUTHOR_EMAIL"))
        .or_else(|| read_git_config(&["config", "--get", "user.email"]))
        .or_else(|| read_git_config(&["config", "--global", "--get", "user.email"]))
        .unwrap_or_default()
}

impl Enricher for IdentityEnricher {
    fn enrich(&mut self, msg: &mut ParsedMessage) -> Vec<Tag> {
        let mut tags = Vec::new();

        let user = msg.user_name.as_deref().unwrap_or(&self.user_name);
        if !user.is_empty() {
            tags.push(Tag {
                key: tk::USER.to_string(),
                value: user.to_string(),
            });
        }
        let machine = msg.machine_name.as_deref().unwrap_or(&self.machine_name);
        if !machine.is_empty() {
            tags.push(Tag {
                key: tk::MACHINE.to_string(),
                value: machine.to_string(),
            });
        }
        if !self.platform.is_empty() {
            tags.push(Tag {
                key: tk::PLATFORM.to_string(),
                value: self.platform.clone(),
            });
        }
        if !self.git_user.is_empty() {
            tags.push(Tag {
                key: tk::GIT_USER.to_string(),
                value: self.git_user.clone(),
            });
        }

        tags
    }
}

// ---------------------------------------------------------------------------
// CostEnricher — calculates cost_cents from tokens × pricing
// ---------------------------------------------------------------------------

pub struct CostEnricher;

impl Enricher for CostEnricher {
    fn enrich(&mut self, msg: &mut ParsedMessage) -> Vec<Tag> {
        let mut tags = Vec::new();

        if msg.role == "assistant" {
            tags.push(Tag {
                key: tk::PROVIDER.to_string(),
                value: msg.provider.clone(),
            });

            if let Some(ref model) = msg.model {
                tags.push(Tag {
                    key: tk::MODEL.to_string(),
                    value: model.clone(),
                });
            }
        }

        // Calculate cost if not already set (skip if API provided exact cost).
        // 8.3 / #376: sources pricing from `pricing::lookup` (ADR-0091). An
        // unknown model short-circuits the whole calc — we must not pipe
        // zero rates through `calculate_cost_cents`, because the web-search
        // surcharge ($0.01/search) would leak onto a supposedly-zero row.
        if msg.cost_cents.is_none() && msg.role == "assistant" {
            if msg.model.is_none() {
                tracing::trace!(
                    "CostEnricher: model is None for message {}, pricing as unknown",
                    msg.uuid
                );
            }
            let model = msg.model.as_deref().unwrap_or("unknown");
            match crate::pricing::lookup(model, &msg.provider) {
                crate::pricing::PricingOutcome::Known { pricing, source } => {
                    msg.cost_cents = Some(pricing.calculate_cost_cents(
                        msg.input_tokens,
                        msg.output_tokens,
                        msg.cache_creation_tokens,
                        msg.cache_read_tokens,
                        msg.cache_creation_1h_tokens,
                        msg.speed.as_deref(),
                        msg.web_search_requests,
                    ));
                    msg.cost_confidence = "estimated".to_string();
                    msg.pricing_source = Some(source.as_column_value());
                }
                crate::pricing::PricingOutcome::Unknown { .. } => {
                    // ADR-0091 §2: $0 + warn, not a silent per-provider default.
                    msg.cost_cents = Some(0.0);
                    msg.cost_confidence = "estimated_unknown_model".to_string();
                    msg.pricing_source = Some(crate::pricing::COLUMN_VALUE_UNKNOWN.to_string());
                }
            }
        }

        // Ensure cost_confidence is always set for assistant messages.
        // Only apply fallback if cost_confidence is not already set (preserves API-provided values).
        if msg.role == "assistant" && msg.cost_cents.is_some() && msg.cost_confidence.is_empty() {
            tracing::warn!(
                "CostEnricher: cost_cents is set but cost_confidence is empty for message {}; falling back to 'estimated'",
                msg.uuid
            );
            msg.cost_confidence = "estimated".to_string();
        }

        // Invariant: if cost_cents is set, cost_confidence must explain how
        // the cost was determined (e.g. "exact", "exact_cost", "estimated").
        debug_assert!(
            msg.cost_cents.is_none() || !msg.cost_confidence.is_empty(),
            "cost_cents is Some but cost_confidence is empty for message {}",
            msg.uuid
        );

        if let Some(ref speed) = msg.speed
            && speed != "standard"
        {
            tags.push(Tag {
                key: tk::SPEED.to_string(),
                value: speed.clone(),
            });
        }

        if msg.role == "assistant" && msg.cost_cents.is_some() {
            tags.push(Tag {
                key: tk::COST_CONFIDENCE.to_string(),
                value: msg.cost_confidence.clone(),
            });
        }

        tags
    }
}

// ---------------------------------------------------------------------------
// TagEnricher — applies user-defined tag rules from tags.toml
// ---------------------------------------------------------------------------

pub struct TagEnricher {
    config: Option<TagsConfig>,
}

impl TagEnricher {
    pub fn new(config: Option<TagsConfig>) -> Self {
        Self { config }
    }
}

impl Enricher for TagEnricher {
    fn enrich(&mut self, msg: &mut ParsedMessage) -> Vec<Tag> {
        let config = match &self.config {
            Some(c) => c,
            None => return Vec::new(),
        };

        let mut tags = Vec::new();
        for rule in &config.rules {
            let matches = if let Some(ref pattern) = rule.match_repo {
                msg.repo_id
                    .as_deref()
                    .map(|r| glob_match(pattern, r))
                    .unwrap_or(false)
            } else {
                true // No match condition → always applies
            };

            if matches {
                tags.push(Tag {
                    key: rule.key.clone(),
                    value: rule.value.clone(),
                });
            }
        }
        tags
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::tests::test_msg;

    #[test]
    fn identity_enricher_produces_tags() {
        let mut enricher = IdentityEnricher::new();
        let mut msg = test_msg();
        let tags = enricher.enrich(&mut msg);
        // Should produce identity tags (values depend on environment)
        if !enricher.user_name.is_empty() {
            assert!(tags.iter().any(|t| t.key == "user"));
        }
        if !enricher.machine_name.is_empty() {
            assert!(tags.iter().any(|t| t.key == "machine"));
        }
        if !enricher.platform.is_empty() {
            assert!(tags.iter().any(|t| t.key == "platform"));
        }
        if !enricher.git_user.is_empty() {
            assert!(tags.iter().any(|t| t.key == "git_user"));
        }
    }

    #[test]
    fn identity_enricher_emits_explicit_platform_machine_and_git_user() {
        let mut enricher = IdentityEnricher {
            user_name: "local-user".to_string(),
            machine_name: "workstation-01".to_string(),
            platform: "macos".to_string(),
            git_user: "Alice Dev".to_string(),
        };
        let mut msg = test_msg();
        let tags = enricher.enrich(&mut msg);

        assert!(
            tags.iter()
                .any(|t| t.key == "user" && t.value == "local-user")
        );
        assert!(
            tags.iter()
                .any(|t| t.key == "machine" && t.value == "workstation-01")
        );
        assert!(
            tags.iter()
                .any(|t| t.key == "platform" && t.value == "macos")
        );
        assert!(
            tags.iter()
                .any(|t| t.key == "git_user" && t.value == "Alice Dev")
        );
    }

    #[test]
    fn cost_enricher_calculates_cost() {
        let mut enricher = CostEnricher;
        let mut msg = test_msg();
        msg.role = "assistant".to_string();
        msg.model = Some("claude-opus-4-6".to_string());
        msg.input_tokens = 1_000_000;
        msg.output_tokens = 100_000;
        let tags = enricher.enrich(&mut msg);
        assert!(msg.cost_cents.is_some());
        // Cost was calculated → confidence should be "estimated"
        assert_eq!(msg.cost_confidence, "estimated");
        // cost_confidence tag should reflect the final value
        assert!(
            tags.iter()
                .any(|t| t.key == "cost_confidence" && t.value == "estimated")
        );
        // Should have provider and model tags
        assert!(tags.iter().any(|t| t.key == "provider"));
        assert!(tags.iter().any(|t| t.key == "model"));
    }

    #[test]
    fn cost_enricher_skips_user_messages() {
        let mut enricher = CostEnricher;
        let mut msg = test_msg();
        msg.role = "user".to_string();
        enricher.enrich(&mut msg);
        assert!(msg.cost_cents.is_none());
    }

    #[test]
    fn tag_enricher_applies_rules() {
        let config = TagsConfig {
            rules: vec![
                crate::config::TagRule {
                    key: "team".to_string(),
                    value: "platform".to_string(),
                    match_repo: Some("*Verkada-Web*".to_string()),
                },
                crate::config::TagRule {
                    key: "org".to_string(),
                    value: "verkada".to_string(),
                    match_repo: None,
                },
            ],
        };
        let mut enricher = TagEnricher::new(Some(config));

        let mut msg = test_msg();
        msg.repo_id = Some("github.com/verkada/Verkada-Web".to_string());
        let tags = enricher.enrich(&mut msg);
        assert_eq!(tags.len(), 2);
        assert!(
            tags.iter()
                .any(|t| t.key == "team" && t.value == "platform")
        );
        assert!(tags.iter().any(|t| t.key == "org" && t.value == "verkada"));
    }

    #[test]
    fn tag_enricher_no_match() {
        let config = TagsConfig {
            rules: vec![crate::config::TagRule {
                key: "team".to_string(),
                value: "platform".to_string(),
                match_repo: Some("*Verkada-Web*".to_string()),
            }],
        };
        let mut enricher = TagEnricher::new(Some(config));

        let mut msg = test_msg();
        msg.repo_id = Some("github.com/other/repo".to_string());
        let tags = enricher.enrich(&mut msg);
        assert!(tags.is_empty());
    }

    #[test]
    fn git_enricher_extracts_ticket() {
        let mut enricher = GitEnricher {
            repo_cache: RepoIdCache::new(),
        };
        let mut msg = test_msg();
        msg.git_branch = Some("PAVA-2057-fix-auth".to_string());
        msg.repo_id = Some("test-repo".to_string());
        let tags = enricher.enrich(&mut msg);
        assert!(
            tags.iter()
                .any(|t| t.key == "ticket_id" && t.value == "PAVA-2057")
        );
        assert!(
            tags.iter()
                .any(|t| t.key == "ticket_prefix" && t.value == "PAVA")
        );
        // R1.3 (#221): GitEnricher records the extractor source so the
        // analytics layer can show provenance the same way R1.2 (#222)
        // did for activity classification.
        assert!(
            tags.iter()
                .any(|t| t.key == "ticket_source" && t.value == "branch"),
            "expected ticket_source=branch tag for alphanumeric ticket, got: {tags:?}"
        );
        assert!(!tags.iter().any(|t| t.key == "repo"));
        assert!(!tags.iter().any(|t| t.key == "branch"));
    }

    /// R1.3 (#221): `budi db import` must also honour the ADR-0082 §9
    /// numeric-only ticket fallback so it matches the live tailer.
    /// Before R1.3, `fix/1234-typo` produced a ticket tag in legacy
    /// proxy-era rows but not on `budi db import`, so analytics disagreed.
    #[test]
    fn git_enricher_extracts_numeric_only_ticket() {
        let mut enricher = GitEnricher {
            repo_cache: RepoIdCache::new(),
        };
        let mut msg = test_msg();
        msg.git_branch = Some("fix/1234-typo".to_string());
        msg.repo_id = Some("test-repo".to_string());
        let tags = enricher.enrich(&mut msg);
        assert!(
            tags.iter()
                .any(|t| t.key == "ticket_id" && t.value == "1234"),
            "expected numeric ticket_id=1234, got: {tags:?}"
        );
        assert!(
            !tags.iter().any(|t| t.key == "ticket_prefix"),
            "numeric tickets have no prefix, got: {tags:?}"
        );
        assert!(
            tags.iter()
                .any(|t| t.key == "ticket_source" && t.value == "branch_numeric"),
            "expected ticket_source=branch_numeric, got: {tags:?}"
        );
    }

    /// Integration branches (main/master/develop/HEAD) never produce a
    /// ticket tag — this pins the unified extractor's filter behaviour
    /// on the import path.
    #[test]
    fn git_enricher_skips_integration_branches() {
        let mut enricher = GitEnricher {
            repo_cache: RepoIdCache::new(),
        };
        for branch in ["main", "master", "develop", "HEAD"] {
            let mut msg = test_msg();
            msg.git_branch = Some(branch.to_string());
            msg.repo_id = Some("test-repo".to_string());
            let tags = enricher.enrich(&mut msg);
            assert!(
                !tags
                    .iter()
                    .any(|t| t.key == "ticket_id" || t.key == "ticket_source"),
                "{branch} must not produce ticket tags, got: {tags:?}"
            );
        }
    }

    #[test]
    fn cost_enricher_preserves_sub_cent_precision() {
        let mut enricher = CostEnricher;
        let mut msg = test_msg();
        msg.role = "assistant".to_string();
        msg.model = Some("claude-opus-4-6".to_string());
        // Tiny message: 3 input + 36 output
        // Cost: 3*$5/1M + 36*$25/1M = $0.000015 + $0.0009 = $0.000915
        // In cents: 0.0915 — must NOT be rounded to 0
        msg.input_tokens = 3;
        msg.output_tokens = 36;
        enricher.enrich(&mut msg);
        let cost = msg.cost_cents.unwrap();
        assert!(cost > 0.0, "sub-cent cost must not be rounded to zero");
        assert!(
            (cost - 0.0915).abs() < 0.001,
            "cost should be ~0.0915 cents, got {}",
            cost
        );
    }

    #[test]
    fn cost_enricher_large_message_precision() {
        let mut enricher = CostEnricher;
        let mut msg = test_msg();
        msg.role = "assistant".to_string();
        msg.model = Some("claude-opus-4-6".to_string());
        // 1M input * $5/M = $5.00, 100K output * $25/M = $2.50
        // Total: $7.50 = 750.0 cents exactly
        msg.input_tokens = 1_000_000;
        msg.output_tokens = 100_000;
        enricher.enrich(&mut msg);
        assert_eq!(msg.cost_cents.unwrap(), 750.0);
    }

    #[test]
    fn cost_enricher_fast_mode_6x() {
        let mut enricher = CostEnricher;
        let mut msg = test_msg();
        msg.role = "assistant".to_string();
        msg.model = Some("claude-opus-4-6".to_string());
        msg.input_tokens = 1_000_000;
        msg.output_tokens = 100_000;
        msg.speed = Some("fast".to_string());
        enricher.enrich(&mut msg);
        // Standard cost: $7.50. Fast mode: 6x = $45.00 = 4500 cents
        assert_eq!(msg.cost_cents.unwrap(), 4500.0);
    }

    #[test]
    fn cost_enricher_standard_speed_no_multiplier() {
        let mut enricher = CostEnricher;
        let mut msg = test_msg();
        msg.role = "assistant".to_string();
        msg.model = Some("claude-opus-4-6".to_string());
        msg.input_tokens = 1_000_000;
        msg.speed = Some("standard".to_string());
        enricher.enrich(&mut msg);
        // Standard: 1M * $5/M = $5.00 = 500 cents (no multiplier)
        assert_eq!(msg.cost_cents.unwrap(), 500.0);
    }

    #[test]
    fn cost_enricher_1h_cache_tier() {
        let mut enricher = CostEnricher;
        let mut msg = test_msg();
        msg.role = "assistant".to_string();
        msg.model = Some("claude-opus-4-6".to_string());
        // All cache tokens in 1-hour tier
        msg.cache_creation_tokens = 1_000_000;
        msg.cache_creation_1h_tokens = 1_000_000;
        enricher.enrich(&mut msg);
        // 1h cache: 1M * $10/M (2x input of $5) = $10.00 = 1000 cents
        assert_eq!(msg.cost_cents.unwrap(), 1000.0);
    }

    #[test]
    fn cost_enricher_mixed_cache_tiers() {
        let mut enricher = CostEnricher;
        let mut msg = test_msg();
        msg.role = "assistant".to_string();
        msg.model = Some("claude-opus-4-6".to_string());
        // 800K in 5-min tier, 200K in 1-hour tier
        msg.cache_creation_tokens = 1_000_000;
        msg.cache_creation_1h_tokens = 200_000;
        enricher.enrich(&mut msg);
        // 5m: 800K * $6.25/M = $5.00
        // 1h: 200K * $10/M = $2.00
        // Total: $7.00 = 700 cents
        assert_eq!(msg.cost_cents.unwrap(), 700.0);
    }

    #[test]
    fn cost_enricher_web_search() {
        let mut enricher = CostEnricher;
        let mut msg = test_msg();
        msg.role = "assistant".to_string();
        msg.model = Some("claude-opus-4-6".to_string());
        msg.web_search_requests = 5;
        enricher.enrich(&mut msg);
        // 5 web searches * $0.01/search = $0.05 = 5 cents
        assert_eq!(msg.cost_cents.unwrap(), 5.0);
    }

    #[test]
    fn cost_enricher_fast_with_web_search() {
        let mut enricher = CostEnricher;
        let mut msg = test_msg();
        msg.role = "assistant".to_string();
        msg.model = Some("claude-opus-4-6".to_string());
        msg.input_tokens = 1_000_000;
        msg.speed = Some("fast".to_string());
        msg.web_search_requests = 10;
        enricher.enrich(&mut msg);
        // Token cost: 1M * $5/M = $5.00, fast 6x = $30.00
        // Web search: 10 * $0.01 = $0.10 (NOT multiplied by fast)
        // Total: $30.10 = 3010 cents
        assert_eq!(msg.cost_cents.unwrap(), 3010.0);
    }

    // ---- FileEnricher --------------------------------------------------

    #[test]
    fn file_enricher_ignores_user_messages() {
        let mut enricher = FileEnricher::new();
        let mut msg = test_msg();
        msg.role = "user".to_string();
        msg.tool_files = vec!["src/main.rs".into()];
        assert!(enricher.enrich(&mut msg).is_empty());
    }

    #[test]
    fn file_enricher_skips_when_no_tool_files() {
        let mut enricher = FileEnricher::new();
        let mut msg = test_msg();
        msg.role = "assistant".to_string();
        msg.tool_files.clear();
        assert!(enricher.enrich(&mut msg).is_empty());
    }

    #[test]
    fn file_enricher_emits_repo_relative_file_tags_without_cwd() {
        let mut enricher = FileEnricher::new();
        let mut msg = test_msg();
        msg.role = "assistant".to_string();
        msg.cwd = None;
        msg.tool_files = vec!["src/main.rs".into(), "README.md".into()];
        let tags = enricher.enrich(&mut msg);

        let files: Vec<&str> = tags
            .iter()
            .filter(|t| t.key == tk::FILE_PATH)
            .map(|t| t.value.as_str())
            .collect();
        assert_eq!(files, vec!["README.md", "src/main.rs"]);

        assert!(
            tags.iter()
                .any(|t| t.key == tk::FILE_PATH_SOURCE && t.value == "tool_arg")
        );
        assert!(
            tags.iter()
                .any(|t| t.key == tk::FILE_PATH_CONFIDENCE && t.value == "high")
        );
    }

    #[test]
    fn file_enricher_drops_absolute_path_without_repo_root() {
        // With no cwd/repo-root signal, absolute paths are privacy-sensitive
        // and must be dropped — this is the ADR-0083 invariant.
        let mut enricher = FileEnricher::new();
        let mut msg = test_msg();
        msg.role = "assistant".to_string();
        msg.cwd = None;
        msg.tool_files = vec!["/Users/dev/secret.rs".into()];
        let tags = enricher.enrich(&mut msg);
        assert!(
            !tags.iter().any(|t| t.key == tk::FILE_PATH),
            "absolute path must not leak into file_path tags"
        );
    }
}
