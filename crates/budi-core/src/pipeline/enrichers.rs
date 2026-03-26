//! Pipeline enrichers: Git, Identity, Cost, Tag, Hook.

use std::collections::HashMap;
use std::path::Path;

use crate::analytics::Tag;
use crate::config::TagsConfig;
use crate::hooks::SessionMeta;
use crate::jsonl::ParsedMessage;
use crate::pipeline::{Enricher, extract_ticket_id, glob_match};
use crate::repo_id::RepoIdCache;

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
                msg.repo_id = Some(repo_id.clone());
                tags.push(Tag {
                    key: "repo".to_string(),
                    value: repo_id,
                });
            }
        } else if let Some(ref repo_id) = msg.repo_id {
            tags.push(Tag {
                key: "repo".to_string(),
                value: repo_id.clone(),
            });
        }

        // Extract ticket_id from git_branch (branch itself is stored as a column, not a tag)
        if let Some(ref branch) = msg.git_branch
            && let Some(ticket) = extract_ticket_id(branch) {
                tags.push(Tag {
                    key: "ticket_id".to_string(),
                    value: ticket.to_string(),
                });
                // Extract prefix (e.g. "PAVA" from "PAVA-2057")
                if let Some(dash) = ticket.find('-') {
                    tags.push(Tag {
                        key: "ticket_prefix".to_string(),
                        value: ticket[..dash].to_string(),
                    });
                }
            }

        tags
    }
}

// ---------------------------------------------------------------------------
// IdentityEnricher — sets user_name and machine_name
// ---------------------------------------------------------------------------

pub struct IdentityEnricher {
    user_name: String,
    machine_name: String,
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
        Self {
            user_name,
            machine_name,
        }
    }
}

fn get_hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .or_else(|_| {
            std::process::Command::new("hostname")
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        })
        .unwrap_or_default()
}

impl Enricher for IdentityEnricher {
    fn enrich(&mut self, msg: &mut ParsedMessage) -> Vec<Tag> {
        let mut tags = Vec::new();

        // Produce user/machine tags (session-level dedup handled by Pipeline)
        let user = msg.user_name.as_deref().unwrap_or(&self.user_name);
        if !user.is_empty() {
            tags.push(Tag {
                key: "user".to_string(),
                value: user.to_string(),
            });
        }
        let machine = msg.machine_name.as_deref().unwrap_or(&self.machine_name);
        if !machine.is_empty() {
            tags.push(Tag {
                key: "machine".to_string(),
                value: machine.to_string(),
            });
        }

        // Produce session_title tag if present
        if let Some(ref title) = msg.session_title
            && !title.is_empty() {
                tags.push(Tag {
                    key: "session_title".to_string(),
                    value: title.clone(),
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

        // Add provider tag
        tags.push(Tag {
            key: "provider".to_string(),
            value: msg.provider.clone(),
        });

        // Add model tag
        if let Some(ref model) = msg.model {
            tags.push(Tag {
                key: "model".to_string(),
                value: model.clone(),
            });
        }

        // Calculate cost if not already set (skip if API provided exact cost)
        if msg.cost_cents.is_none() && msg.role == "assistant" {
            if msg.model.is_none() {
                tracing::debug!(
                    "CostEnricher: model is None for message {}, using default pricing",
                    msg.uuid
                );
            }
            let model = msg.model.as_deref().unwrap_or("unknown");
            if model == "unknown" {
                tracing::debug!(
                    "CostEnricher: model is 'unknown' for message {}, cost estimate may be inaccurate",
                    msg.uuid
                );
            }
            let pricing = match msg.provider.as_str() {
                "cursor" => crate::providers::cursor::cursor_pricing_for_model(model),
                _ => crate::providers::claude_code::claude_pricing_for_model(model),
            };
            let cost = msg.input_tokens as f64 * pricing.input / 1_000_000.0
                + msg.output_tokens as f64 * pricing.output / 1_000_000.0
                + msg.cache_creation_tokens as f64 * pricing.cache_write / 1_000_000.0
                + msg.cache_read_tokens as f64 * pricing.cache_read / 1_000_000.0;
            // Always set cost_cents for assistant messages (Some(0.0) for zero-cost)
            // so they are distinguishable from NULL (unknown cost) in queries.
            msg.cost_cents = Some(cost * 100.0);
            msg.cost_confidence = "estimated".to_string();
        }

        // Ensure cost_confidence is always set for assistant messages
        // Only apply fallback if cost_confidence is not already set (preserves API-provided values)
        if msg.role == "assistant" && msg.cost_cents.is_some() && msg.cost_confidence.is_empty() {
            msg.cost_confidence = "estimated".to_string();
        }

        // Emit cost_confidence tag for assistant messages that have cost data
        if msg.role == "assistant" && msg.cost_cents.is_some() {
            tags.push(Tag {
                key: "cost_confidence".to_string(),
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

// ---------------------------------------------------------------------------
// HookEnricher — produces tags from session metadata (hooks + sessions table)
// ---------------------------------------------------------------------------

pub struct HookEnricher {
    session_cache: HashMap<String, SessionMeta>,
}

impl HookEnricher {
    /// Create a new HookEnricher with pre-loaded session metadata.
    pub fn new(session_cache: HashMap<String, SessionMeta>) -> Self {
        Self { session_cache }
    }
}

impl Enricher for HookEnricher {
    fn enrich(&mut self, msg: &mut ParsedMessage) -> Vec<Tag> {
        let mut tags = Vec::new();

        let Some(ref session_id) = msg.session_id else {
            return tags;
        };
        let Some(meta) = self.session_cache.get(session_id) else {
            tracing::debug!("HookEnricher: session '{}' not found in cache", session_id);
            return tags;
        };

        if let Some(ref mode) = meta.composer_mode {
            tags.push(Tag {
                key: "composer_mode".to_string(),
                value: mode.clone(),
            });
        }
        if let Some(ref mode) = meta.permission_mode {
            tags.push(Tag {
                key: "permission_mode".to_string(),
                value: mode.clone(),
            });
        }
        if let Some(ref category) = meta.prompt_category {
            tags.push(Tag {
                key: "activity".to_string(),
                value: category.clone(),
            });
        }
        if let Some(ref email) = meta.user_email {
            tags.push(Tag {
                key: "user_email".to_string(),
                value: email.clone(),
            });
        }
        if let Some(ms) = meta.duration_ms {
            let bucket = if ms < 300_000 {
                "short"
            } else if ms < 1_800_000 {
                "medium"
            } else {
                "long"
            };
            tags.push(Tag {
                key: "duration".to_string(),
                value: bucket.to_string(),
            });
        }
        if let Some(ref tool) = meta.dominant_tool {
            tags.push(Tag {
                key: "dominant_tool".to_string(),
                value: tool.clone(),
            });
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
        // Should produce user and machine tags (values depend on environment)
        if !enricher.user_name.is_empty() {
            assert!(tags.iter().any(|t| t.key == "user"));
        }
        if !enricher.machine_name.is_empty() {
            assert!(tags.iter().any(|t| t.key == "machine"));
        }
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
        // branch should NOT be emitted as a tag (stored as column only)
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
}
