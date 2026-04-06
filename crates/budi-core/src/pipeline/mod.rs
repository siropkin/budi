//! Data pipeline: Extract → Normalize → Enrich → Load.
//!
//! Provides a pluggable enrichment pipeline that transforms raw `ParsedMessage`s
//! before they are ingested into the database.

pub mod enrichers;

use crate::analytics::Tag;
use crate::jsonl::ParsedMessage;
use crate::tag_keys as tk;

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
        // Enricher order is critical — do not reorder without understanding dependencies:
        //   1. HookEnricher  — populates session-level metadata (composer_mode, etc.)
        //   2. IdentityEnricher — populates user/machine tags
        //   3. GitEnricher   — sets repo_id for glob_match (MUST run before TagEnricher)
        //   4. ToolEnricher  — emits per-message tool tags from parsed tool calls
        //   5. CostEnricher  — calculates cost_cents and cost_confidence
        //   6. TagEnricher   — applies user rules (depends on repo_id, model, cost_confidence)
        let enrichers: Vec<Box<dyn Enricher>> = vec![
            Box::new(enrichers::HookEnricher::new(session_cache)),
            Box::new(enrichers::IdentityEnricher::new()),
            Box::new(enrichers::GitEnricher::new()),
            Box::new(enrichers::ToolEnricher),
            Box::new(enrichers::CostEnricher),
            Box::new(enrichers::TagEnricher::new(tags_config)),
        ];
        Self { enrichers }
    }

    /// Process a batch of messages through all enrichers.
    /// Returns a parallel Vec of tags for each message.
    ///
    /// Two kinds of tags:
    /// - **Identity tags** (user, machine, composer_mode, …): constant for the
    ///   whole session → deduplicated, emitted once on the first assistant msg.
    /// - **Context tags** (ticket_id, activity, tool, …): can change mid-session
    ///   → emitted on every assistant message so cost attribution is accurate.
    ///
    /// All tags land on assistant messages only (queries filter `role='assistant'`).
    pub fn process(&mut self, messages: &mut [ParsedMessage]) -> Vec<Vec<Tag>> {
        use std::collections::{HashMap, HashSet};
        let mut all_tags = Vec::with_capacity(messages.len());

        // Identity tags: dedup per session, buffer from user→assistant.
        let identity_keys = tk::SESSION_IDENTITY_KEYS;
        let mut seen_identity: HashSet<(String, String, String)> = HashSet::new();
        let mut pending_identity: HashMap<String, Vec<Tag>> = HashMap::new();

        // Sort by (session_id, timestamp) to handle out-of-order batches.
        messages.sort_by(|a, b| {
            a.session_id
                .cmp(&b.session_id)
                .then(a.timestamp.cmp(&b.timestamp))
        });

        propagate_session_context(messages);

        for msg in messages.iter_mut() {
            let mut msg_tags = Vec::new();
            let dedup_id = msg.session_id.clone().unwrap_or_else(|| msg.uuid.clone());

            for enricher in &mut self.enrichers {
                for tag in enricher.enrich(msg) {
                    if identity_keys.contains(&tag.key.as_str()) {
                        let key = (dedup_id.clone(), tag.key.clone(), tag.value.clone());
                        if !seen_identity.insert(key) {
                            continue;
                        }
                        if msg.role != "assistant" {
                            pending_identity
                                .entry(dedup_id.clone())
                                .or_default()
                                .push(tag);
                            continue;
                        }
                    }
                    if msg.role == "assistant" {
                        msg_tags.push(tag);
                    }
                }
            }

            if msg.role == "assistant" {
                // Flush buffered identity tags from prior user messages.
                if let Some(buffered) = pending_identity.remove(&dedup_id) {
                    msg_tags.extend(buffered);
                }

                // Activity tag from prompt_category (propagated from user msg).
                if let Some(ref cat) = msg.prompt_category {
                    msg_tags.push(Tag {
                        key: tk::ACTIVITY.to_string(),
                        value: cat.clone(),
                    });
                }
            }

            all_tags.push(msg_tags);
        }
        all_tags
    }
}

/// Propagate git_branch, repo_id, cwd, and prompt_category from earlier messages
/// to later messages within the same session. Each message inherits from the most
/// recent preceding message in the same session that has the field set.
fn propagate_session_context(messages: &mut [ParsedMessage]) {
    struct Ctx {
        branch: Option<String>,
        repo: Option<String>,
        cwd: Option<String>,
        category: Option<String>,
    }
    let mut session_ctx: std::collections::HashMap<String, Ctx> = std::collections::HashMap::new();
    for msg in messages.iter_mut() {
        let key = msg
            .session_id
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| msg.uuid.clone());

        let ctx = session_ctx.entry(key).or_insert_with(|| Ctx {
            branch: None,
            repo: None,
            cwd: None,
            category: None,
        });

        if let Some(b) = &msg.git_branch {
            if !b.is_empty() {
                ctx.branch = Some(b.clone());
            }
        } else if let Some(ref b) = ctx.branch {
            msg.git_branch = Some(b.clone());
        }
        if let Some(r) = &msg.repo_id {
            if !r.is_empty() {
                ctx.repo = Some(r.clone());
            }
        } else if let Some(ref r) = ctx.repo {
            msg.repo_id = Some(r.clone());
        }
        if let Some(c) = &msg.cwd {
            if !c.is_empty() {
                ctx.cwd = Some(c.clone());
            }
        } else if let Some(ref c) = ctx.cwd {
            msg.cwd = Some(c.clone());
        }
        if let Some(cat) = &msg.prompt_category {
            if !cat.is_empty() && ctx.category.is_none() {
                ctx.category = Some(cat.clone());
            }
        } else if let Some(ref cat) = ctx.category {
            msg.prompt_category = Some(cat.clone());
        }
    }
}

/// Simple glob matching supporting `*` (any chars) and `?` (single char).
/// Operates on bytes for zero-allocation matching.
///
/// **ASCII-only**: `?` matches a single byte, not a single Unicode character.
/// This is correct for repo IDs and branch names which are ASCII in practice.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let pat = pattern.as_bytes();
    let txt = text.as_bytes();
    let (mut pi, mut ti) = (0, 0);
    let (mut star_pi, mut star_ti) = (usize::MAX, 0);

    while ti < txt.len() {
        if pi < pat.len() && (pat[pi] == b'?' || pat[pi] == txt[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < pat.len() && pat[pi] == b'*' {
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

    while pi < pat.len() && pat[pi] == b'*' {
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
            session_title: None,
            parent_uuid: None,
            user_name: None,
            machine_name: None,
            cost_confidence: "n/a".to_string(),
            request_id: None,
            speed: None,
            cache_creation_1h_tokens: 0,
            web_search_requests: 0,
            prompt_category: None,
            tool_names: Vec::new(),
            tool_use_ids: Vec::new(),
        }
    }

    #[test]
    fn no_tags_on_user_messages() {
        use crate::config::TagsConfig;
        let mut pipeline = Pipeline::default_pipeline(
            Some(TagsConfig::default()),
            std::collections::HashMap::new(),
        );

        let mut user_msg = test_msg();
        user_msg.uuid = "u1".into();
        user_msg.session_id = Some("sess-1".into());
        user_msg.role = "user".into();
        user_msg.repo_id = Some("github.com/test/repo".into());
        user_msg.git_branch = Some("PROJ-123-feature".into());
        user_msg.prompt_category = Some("bugfix".into());
        user_msg.timestamp = "2026-03-14T18:13:42Z".parse().unwrap();

        let mut asst_msg = test_msg();
        asst_msg.uuid = "a1".into();
        asst_msg.session_id = Some("sess-1".into());
        asst_msg.role = "assistant".into();
        asst_msg.model = Some("claude-opus".into());
        asst_msg.output_tokens = 100;
        asst_msg.timestamp = "2026-03-14T18:13:43Z".parse().unwrap();

        let mut msgs = vec![user_msg, asst_msg];
        let all_tags = pipeline.process(&mut msgs);

        assert!(
            all_tags[0].is_empty(),
            "user message should have zero tags, got: {:?}",
            all_tags[0]
        );

        let asst_keys: Vec<&str> = all_tags[1].iter().map(|t| t.key.as_str()).collect();
        assert!(
            asst_keys.contains(&"ticket_id"),
            "missing ticket_id, got: {asst_keys:?}"
        );
        assert!(
            asst_keys.contains(&"activity"),
            "missing activity, got: {asst_keys:?}"
        );
    }

    #[test]
    fn context_tags_on_every_assistant_message() {
        use crate::config::TagsConfig;
        let mut pipeline = Pipeline::default_pipeline(
            Some(TagsConfig::default()),
            std::collections::HashMap::new(),
        );

        let mut u1 = test_msg();
        u1.uuid = "u1".into();
        u1.session_id = Some("sess-1".into());
        u1.role = "user".into();
        u1.repo_id = Some("github.com/test/repo".into());
        u1.git_branch = Some("PROJ-123-feature".into());
        u1.prompt_category = Some("bugfix".into());
        u1.timestamp = "2026-03-14T18:13:42Z".parse().unwrap();

        let mut a1 = test_msg();
        a1.uuid = "a1".into();
        a1.session_id = Some("sess-1".into());
        a1.role = "assistant".into();
        a1.model = Some("claude-opus".into());
        a1.output_tokens = 100;
        a1.timestamp = "2026-03-14T18:13:43Z".parse().unwrap();

        let mut u2 = test_msg();
        u2.uuid = "u2".into();
        u2.session_id = Some("sess-1".into());
        u2.role = "user".into();
        u2.timestamp = "2026-03-14T18:14:00Z".parse().unwrap();

        let mut a2 = test_msg();
        a2.uuid = "a2".into();
        a2.session_id = Some("sess-1".into());
        a2.role = "assistant".into();
        a2.model = Some("claude-opus".into());
        a2.output_tokens = 200;
        a2.timestamp = "2026-03-14T18:14:01Z".parse().unwrap();

        let mut msgs = vec![u1, a1, u2, a2];
        let all_tags = pipeline.process(&mut msgs);

        // Both assistant messages should have ticket_id (context tag).
        for (idx, label) in [(1, "a1"), (3, "a2")] {
            let keys: Vec<&str> = all_tags[idx].iter().map(|t| t.key.as_str()).collect();
            assert!(
                keys.contains(&"ticket_id"),
                "{label} should have 'ticket_id' tag, got: {keys:?}"
            );
        }

        // Identity tags (user, machine) should appear only once across all messages.
        let all_user_tags: Vec<_> = all_tags
            .iter()
            .flat_map(|t| t.iter())
            .filter(|t| t.key == "user")
            .collect();
        assert!(
            all_user_tags.len() <= 1,
            "identity tag 'user' should be deduplicated, found {} occurrences",
            all_user_tags.len()
        );
    }

    #[test]
    fn tool_tags_emit_all_tools_per_message() {
        use crate::config::TagsConfig;
        let mut pipeline = Pipeline::default_pipeline(
            Some(TagsConfig::default()),
            std::collections::HashMap::new(),
        );

        let mut msg = test_msg();
        msg.uuid = "a1".into();
        msg.session_id = Some("sess-1".into());
        msg.role = "assistant".into();
        msg.model = Some("claude-opus".into());
        msg.tool_names = vec![
            "Read".into(),
            "Bash".into(),
            "Read".into(), // duplicate should be deduped
        ];

        let mut msgs = vec![msg];
        let tags = pipeline.process(&mut msgs);
        let tool_values: std::collections::HashSet<String> = tags[0]
            .iter()
            .filter(|t| t.key == "tool")
            .map(|t| t.value.clone())
            .collect();

        assert!(tool_values.contains("Read"));
        assert!(tool_values.contains("Bash"));
        assert_eq!(tool_values.len(), 2);
    }
}
