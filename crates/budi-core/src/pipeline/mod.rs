//! Data pipeline: Extract → Normalize → Enrich → Load.
//!
//! Provides a pluggable enrichment pipeline that transforms raw `ParsedMessage`s
//! before they are ingested into the database.

pub mod enrichers;

use crate::analytics::Tag;
use crate::jsonl::ParsedMessage;

/// Trait for pipeline enrichers. Each enricher can mutate a message and produce tags.
pub trait Enricher: Send {
    fn enrich(&mut self, msg: &mut ParsedMessage) -> Vec<Tag>;
}

/// The enrichment pipeline — runs a sequence of enrichers over messages.
pub struct Pipeline {
    enrichers: Vec<Box<dyn Enricher>>,
}

impl Pipeline {
    /// Create the default pipeline with all standard enrichers.
    /// `session_cache` is pre-loaded from the sessions/hook_events tables.
    pub fn default_pipeline(
        tags_config: Option<crate::config::TagsConfig>,
        session_cache: std::collections::HashMap<String, crate::hooks::SessionMeta>,
    ) -> Self {
        let enrichers: Vec<Box<dyn Enricher>> = vec![
            Box::new(enrichers::HookEnricher::new(session_cache)),
            Box::new(enrichers::IdentityEnricher::new()),
            Box::new(enrichers::GitEnricher::new()),
            Box::new(enrichers::CostEnricher),
            Box::new(enrichers::TagEnricher::new(tags_config)),
        ];
        Self { enrichers }
    }

    /// Process a batch of messages through all enrichers.
    /// Returns a parallel Vec of tags for each message.
    /// Session-level tags (ticket_id, ticket_prefix, branch, repo) are
    /// deduplicated: only emitted for the first message in each session.
    pub fn process(&mut self, messages: &mut [ParsedMessage]) -> Vec<Vec<Tag>> {
        use std::collections::HashSet;
        let mut all_tags = Vec::with_capacity(messages.len());
        // Track (session_id, key, value) to avoid duplicate session-level tags.
        let mut seen_session_tags: HashSet<(String, String, String)> = HashSet::new();
        let session_level_keys: &[&str] = &[
            "ticket_id", "ticket_prefix", "branch", "repo", "session_title", "user", "machine",
            "composer_mode", "permission_mode", "activity", "user_email", "duration", "dominant_tool",
        ];

        // Sort by (session_id, timestamp) to handle out-of-order batches.
        messages.sort_by(|a, b| {
            a.session_id.cmp(&b.session_id).then(a.timestamp.cmp(&b.timestamp))
        });

        // Propagate git_branch, repo_id, cwd from user messages to subsequent
        // assistant messages in the same session (JSONL only has gitBranch on user entries).
        // Uses temporal propagation: each message inherits from the most recent
        // preceding message in the same session that has the field set.
        let mut session_branch: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let mut session_repo: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let mut session_cwd: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        for msg in messages.iter_mut() {
            if let Some(ref sid) = msg.session_id {
                // If this message has the field, update the running value
                // If not, inherit from the most recent preceding message
                if let Some(ref b) = msg.git_branch {
                    session_branch.insert(sid.clone(), b.clone());
                } else if let Some(b) = session_branch.get(sid) {
                    msg.git_branch = Some(b.clone());
                }
                if let Some(ref r) = msg.repo_id {
                    session_repo.insert(sid.clone(), r.clone());
                } else if let Some(r) = session_repo.get(sid) {
                    msg.repo_id = Some(r.clone());
                }
                if let Some(ref c) = msg.cwd {
                    session_cwd.insert(sid.clone(), c.clone());
                } else if let Some(c) = session_cwd.get(sid) {
                    msg.cwd = Some(c.clone());
                }
            }
        }

        for msg in messages.iter_mut() {
            normalize(msg);
            let mut msg_tags = Vec::new();
            for enricher in &mut self.enrichers {
                for tag in enricher.enrich(msg) {
                    if session_level_keys.contains(&tag.key.as_str()) {
                        // Use session_id for dedup, or message uuid for unsessionized messages
                        let dedup_id = msg.session_id.clone().unwrap_or_else(|| msg.uuid.clone());
                        let key = (dedup_id, tag.key.clone(), tag.value.clone());
                        if !seen_session_tags.insert(key) {
                            tracing::trace!("pipeline: skipping duplicate session tag {}={}", tag.key, tag.value);
                            continue;
                        }
                    }
                    msg_tags.push(tag);
                }
            }
            all_tags.push(msg_tags);
        }
        all_tags
    }
}

/// Normalize a parsed message (trim whitespace, apply defaults).
fn normalize(msg: &mut ParsedMessage) {
    // Trim whitespace from string fields
    if let Some(ref mut cwd) = msg.cwd {
        let trimmed = cwd.trim().to_string();
        if trimmed.is_empty() {
            msg.cwd = None;
        } else {
            *cwd = trimmed;
        }
    }
    if let Some(ref mut branch) = msg.git_branch {
        let trimmed = branch.trim().to_string();
        if trimmed.is_empty() {
            msg.git_branch = None;
        } else {
            *branch = trimmed;
        }
    }
}

/// Simple glob matching supporting `*` (any chars) and `?` (single char).
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let txt: Vec<char> = text.chars().collect();
    glob_match_inner(&pat, &txt)
}

fn glob_match_inner(pat: &[char], txt: &[char]) -> bool {
    let (mut pi, mut ti) = (0, 0);
    let (mut star_pi, mut star_ti) = (usize::MAX, 0);

    while ti < txt.len() {
        if pi < pat.len() && (pat[pi] == '?' || pat[pi] == txt[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < pat.len() && pat[pi] == '*' {
            star_pi = pi;
            star_ti = ti;
            pi += 1;
        } else if star_pi != usize::MAX {
            pi = star_pi + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }

    while pi < pat.len() && pat[pi] == '*' {
        pi += 1;
    }

    pi == pat.len()
}

/// Extract a ticket ID (e.g. `PAVA-2057`) from a branch name.
/// Matches `[a-zA-Z]{2,}-\d+` and returns it uppercased.
/// Handles both standard (`PAVA-2057-fix`) and Graphite-style (`03-20-pava-2120_desc`) branches.
/// Requires 2+ alpha chars in the prefix to avoid matching date fragments like `03-20`.
pub fn extract_ticket_id(branch: &str) -> Option<String> {
    let bytes = branch.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        // Find start of alphabetic sequence
        if !bytes[i].is_ascii_alphabetic() {
            i += 1;
            continue;
        }
        let start = i;
        while i < len && bytes[i].is_ascii_alphabetic() {
            i += 1;
        }
        let alpha_len = i - start;
        // Need at least 2 alpha chars followed by '-'
        if alpha_len < 2 || i >= len || bytes[i] != b'-' {
            continue;
        }
        i += 1; // skip '-'
        // Need at least one digit
        let digit_start = i;
        while i < len && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i > digit_start {
            let ticket = &branch[start..i];
            return Some(ticket.to_ascii_uppercase());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_exact_match() {
        assert!(glob_match("hello", "hello"));
        assert!(!glob_match("hello", "world"));
    }

    #[test]
    fn glob_star() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("foo*", "foobar"));
        assert!(glob_match("*bar", "foobar"));
        assert!(glob_match(
            "*Verkada*",
            "github.com/verkada/Verkada-Backend"
        ));
    }

    #[test]
    fn glob_question_mark() {
        assert!(glob_match("h?llo", "hello"));
        assert!(!glob_match("h?llo", "heello"));
    }

    #[test]
    fn glob_combined() {
        assert!(glob_match("*.rs", "main.rs"));
        assert!(glob_match("src/*/*.rs", "src/lib/mod.rs"));
    }

    #[test]
    fn extract_ticket_basic() {
        assert_eq!(
            extract_ticket_id("PAVA-2057-fix-thing"),
            Some("PAVA-2057".into())
        );
        assert_eq!(extract_ticket_id("feature/ABC-123"), Some("ABC-123".into()));
        assert_eq!(extract_ticket_id("main"), None);
        assert_eq!(extract_ticket_id("fix-bug"), None);
    }

    #[test]
    fn extract_ticket_at_end() {
        assert_eq!(
            extract_ticket_id("feature/TICKET-99"),
            Some("TICKET-99".into())
        );
    }

    #[test]
    fn extract_ticket_multiple() {
        // Returns first match
        assert_eq!(extract_ticket_id("ABC-1-DEF-2"), Some("ABC-1".into()));
    }

    #[test]
    fn extract_ticket_graphite_lowercase() {
        assert_eq!(
            extract_ticket_id("03-20-pava-2120_extract_min_max_volume"),
            Some("PAVA-2120".into())
        );
        assert_eq!(
            extract_ticket_id("sen-10553-format-relative-time"),
            Some("SEN-10553".into())
        );
        assert_eq!(
            extract_ticket_id("ivan.seredkin/pava-1908-regression"),
            Some("PAVA-1908".into())
        );
    }

    #[test]
    fn extract_ticket_skips_date_prefix() {
        // "03-20" should not match (single-digit alpha count)
        assert_eq!(
            extract_ticket_id("03-20-pava-2120_desc"),
            Some("PAVA-2120".into())
        );
        // No ticket at all
        assert_eq!(extract_ticket_id("kiyoshi/pava-searchbars"), None);
    }

    #[test]
    fn normalize_trims_cwd() {
        let mut msg = test_msg();
        msg.cwd = Some("  /tmp/project  ".to_string());
        normalize(&mut msg);
        assert_eq!(msg.cwd.as_deref(), Some("/tmp/project"));
    }

    #[test]
    fn normalize_clears_empty_cwd() {
        let mut msg = test_msg();
        msg.cwd = Some("   ".to_string());
        normalize(&mut msg);
        assert!(msg.cwd.is_none());
    }

    pub fn test_msg() -> ParsedMessage {
        ParsedMessage {
            uuid: "test".to_string(),
            session_id: None,
            timestamp: "2026-03-14T18:13:42Z".parse().unwrap(),
            cwd: None,
            role: "user".to_string(),
            model: None,
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            git_branch: None,
            repo_id: None,
            provider: "claude_code".to_string(),
            cost_cents: None,
            context_tokens_used: None,
            context_token_limit: None,
            session_title: None,
            parent_uuid: None,
            user_name: None,
            machine_name: None,
            cost_confidence: "exact".to_string(),
        }
    }
}
