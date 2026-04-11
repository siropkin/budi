//! Pipeline enrichers: Git, Identity, Cost, Tag.

use std::path::Path;

use crate::analytics::Tag;
use crate::config::TagsConfig;
use crate::jsonl::ParsedMessage;
use crate::pipeline::{Enricher, extract_ticket_id, glob_match};
use crate::repo_id::RepoIdCache;
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

        // Resolve repo_id from cwd
        if msg.repo_id.is_none() {
            if msg.cwd.is_none() {
                tracing::debug!(
                    "GitEnricher: no cwd for message {}, skipping repo resolution",
                    msg.uuid
                );
            }
            if let Some(ref cwd) = msg.cwd {
                let repo_id = self.repo_cache.resolve(Path::new(cwd));
                msg.repo_id = Some(repo_id);
            }
        }

        // Extract ticket_id from git_branch (branch itself is stored as a column, not a tag)
        if let Some(ref branch) = msg.git_branch
            && let Some(ticket) = extract_ticket_id(branch)
        {
            tags.push(Tag {
                key: tk::TICKET_ID.to_string(),
                value: ticket.to_string(),
            });
            if let Some(dash) = ticket.find('-') {
                tags.push(Tag {
                    key: tk::TICKET_PREFIX.to_string(),
                    value: ticket[..dash].to_string(),
                });
            }
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

        // Calculate cost if not already set (skip if API provided exact cost)
        if msg.cost_cents.is_none() && msg.role == "assistant" {
            if msg.model.is_none() {
                tracing::trace!(
                    "CostEnricher: model is None for message {}, using default pricing",
                    msg.uuid
                );
            }
            let model = msg.model.as_deref().unwrap_or("unknown");
            if model == "unknown" {
                tracing::trace!(
                    "CostEnricher: model is 'unknown' for message {}, cost estimate may be inaccurate",
                    msg.uuid
                );
            }
            if msg.provider != "cursor" && msg.provider != "claude_code" {
                tracing::warn!(
                    "CostEnricher: unknown provider '{}', using claude_code pricing",
                    msg.provider
                );
            }
            let pricing = crate::provider::pricing_for_model(model, &msg.provider);

            msg.cost_cents = Some(pricing.calculate_cost_cents(
                msg.input_tokens,
                msg.output_tokens,
                msg.cache_creation_tokens,
                msg.cache_read_tokens,
                msg.cache_creation_1h_tokens,
                msg.speed.as_deref(),
                msg.web_search_requests,
            ));
            // Distinguish between known and unknown model estimates
            if model == "unknown" {
                msg.cost_confidence = "estimated_unknown_model".to_string();
            } else {
                msg.cost_confidence = "estimated".to_string();
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
        assert!(!tags.iter().any(|t| t.key == "repo"));
        assert!(!tags.iter().any(|t| t.key == "branch"));
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
}
