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
    pub fn default_pipeline(tags_config: Option<crate::config::TagsConfig>) -> Self {
        // Enricher order is critical — do not reorder without understanding dependencies:
        //   1. IdentityEnricher — populates local identity tags (user/platform/machine/git_user)
        //   2. GitEnricher   — sets repo_id for glob_match (MUST run before TagEnricher)
        //   3. ToolEnricher  — emits per-message tool tags from parsed tool calls
        //   4. CostEnricher  — calculates cost_cents and cost_confidence
        //   5. TagEnricher   — applies user rules (depends on repo_id, model, cost_confidence)
        let enrichers: Vec<Box<dyn Enricher>> = vec![
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
    /// - **Identity tags** (user, platform, machine, git_user, composer_mode, …): constant for the
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
                // R1.2 (#222) also emits source/confidence as sibling tags so
                // aggregates in `activity_cost*` can render explainable
                // labels per-activity instead of a global fallback.
                if let Some(ref cat) = msg.prompt_category {
                    msg_tags.push(Tag {
                        key: tk::ACTIVITY.to_string(),
                        value: cat.clone(),
                    });
                    if let Some(ref src) = msg.prompt_category_source {
                        msg_tags.push(Tag {
                            key: tk::ACTIVITY_SOURCE.to_string(),
                            value: src.clone(),
                        });
                    }
                    if let Some(ref conf) = msg.prompt_category_confidence {
                        msg_tags.push(Tag {
                            key: tk::ACTIVITY_CONFIDENCE.to_string(),
                            value: conf.clone(),
                        });
                    }
                }
            }

            all_tags.push(msg_tags);
        }
        all_tags
    }
}

/// Propagate git_branch, repo_id, cwd, prompt_category, and the R1.2
/// (#222) classification source/confidence fields from earlier messages to
/// later messages within the same session. Each message inherits from the
/// most recent preceding message in the same session that has the field
/// set.
fn propagate_session_context(messages: &mut [ParsedMessage]) {
    struct Ctx {
        branch: Option<String>,
        repo: Option<String>,
        cwd: Option<String>,
        category: Option<String>,
        category_source: Option<String>,
        category_confidence: Option<String>,
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
            category_source: None,
            category_confidence: None,
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
            if !cat.is_empty() {
                // Activity classification can change mid-session.
                // Keep the latest non-empty prompt_category and propagate it forward.
                ctx.category = Some(cat.clone());
                // Source / confidence move as a unit with the category. If
                // the latest user message produced a category but no source
                // / confidence (e.g. legacy ingest), fall through to `rule`
                // / `medium` so downstream queries still get a label.
                ctx.category_source = msg
                    .prompt_category_source
                    .clone()
                    .or_else(|| Some(crate::hooks::SOURCE_RULE.to_string()));
                ctx.category_confidence = msg
                    .prompt_category_confidence
                    .clone()
                    .or_else(|| Some(crate::hooks::CONF_MEDIUM.to_string()));
            }
        } else if let Some(ref cat) = ctx.category {
            msg.prompt_category = Some(cat.clone());
            if msg.prompt_category_source.is_none() {
                msg.prompt_category_source = ctx.category_source.clone();
            }
            if msg.prompt_category_confidence.is_none() {
                msg.prompt_category_confidence = ctx.category_confidence.clone();
            }
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

/// Canonical source label for tickets derived from the alphanumeric
/// `<PREFIX>-<NUM>` pattern (e.g. `PAVA-2057`). Stable across the 8.x
/// wire format — callers pin on this value.
pub const TICKET_SOURCE_BRANCH: &str = "branch";

/// Canonical source label for tickets derived from the pure-numeric
/// fallback defined in ADR-0082 §9 (e.g. `1234` in `fix/1234-typo`).
pub const TICKET_SOURCE_BRANCH_NUMERIC: &str = "branch_numeric";

/// Branch names that are never tickets — integration branches and the
/// literal detached-HEAD sentinel. Kept in one place so proxy ingest and
/// the import pipeline agree.
const INTEGRATION_BRANCHES: &[&str] = &["main", "master", "develop", "HEAD"];

/// Extract a ticket ID (e.g. `PAVA-2057`) from a branch name.
/// Matches `[a-zA-Z]{2,}-\d+` and returns it uppercased.
/// Handles both standard (`PAVA-2057-fix`) and Graphite-style (`03-20-pava-2120_desc`) branches.
/// Requires 2+ alpha chars in the prefix to avoid matching date fragments like `03-20`.
///
/// Backwards-compatible thin wrapper around `extract_ticket_from_branch`:
/// callers that only care about the alphanumeric pattern still get what
/// they expect. Use `extract_ticket_from_branch` when the call site needs
/// to distinguish the numeric fallback or record the source.
pub fn extract_ticket_id(branch: &str) -> Option<String> {
    extract_ticket_alpha(branch)
}

/// Unified ticket extractor used by both the proxy ingest path and the
/// batch import pipeline. Returns `(ticket_id, source)` where `source` is
/// one of `TICKET_SOURCE_BRANCH` or `TICKET_SOURCE_BRANCH_NUMERIC`.
///
/// Behavior (R1.3, #221):
/// 1. Integration branches (`main`, `master`, `develop`, `HEAD`) and
///    empty branch names are never tickets.
/// 2. Tries the alphanumeric pattern first (`[a-zA-Z]{2,}-\d+`). This
///    covers the vast majority of enterprise dev workflows and wins over
///    the numeric fallback so `fix/PAVA-42-and-1234` still resolves to
///    `PAVA-42` rather than `42`.
/// 3. Falls back to the pure-numeric pattern from ADR-0082 §9 — a
///    leading numeric segment in the last `/`-separated path component,
///    followed by `-` or end-of-string. This handles `fix/1234-typo`
///    conventions that many GitHub-flow teams rely on and keeps proxy
///    behaviour consistent with batch ingest.
pub fn extract_ticket_from_branch(branch: &str) -> Option<(String, &'static str)> {
    if branch.is_empty() || INTEGRATION_BRANCHES.contains(&branch) {
        return None;
    }
    if let Some(id) = extract_ticket_alpha(branch) {
        return Some((id, TICKET_SOURCE_BRANCH));
    }
    if let Some(id) = extract_ticket_numeric(branch) {
        return Some((id, TICKET_SOURCE_BRANCH_NUMERIC));
    }
    None
}

/// Extract an alphanumeric ticket (`[a-zA-Z]{2,}-\d+`) from anywhere in
/// the branch, uppercased. Private helper used by both `extract_ticket_id`
/// and `extract_ticket_from_branch`.
fn extract_ticket_alpha(branch: &str) -> Option<String> {
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

/// Extract a numeric-only ticket ID from the last path component of a
/// branch name per ADR-0082 §9. Matches a leading digit run followed by
/// `-` or end-of-string, e.g. `fix/1234-typo` → `"1234"`. Refuses bare
/// year-style prefixes like `2024-roadmap` when they would collide with
/// an alpha ticket (callers try alpha first).
fn extract_ticket_numeric(branch: &str) -> Option<String> {
    let segment = branch.rsplit('/').next().unwrap_or(branch);
    let bytes = segment.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_digit() {
        return None;
    }
    let end = bytes
        .iter()
        .position(|&b| !b.is_ascii_digit())
        .unwrap_or(bytes.len());
    if end == 0 {
        return None;
    }
    // Must be followed by '-' or end-of-string to be a ticket, not just any number
    if end < bytes.len() && bytes[end] != b'-' {
        return None;
    }
    Some(segment[..end].to_string())
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

    // Unified extractor contract (R1.3, #221) — must agree with proxy
    // ingest and batch import so `--tickets` tells the same story from
    // both ingest paths.

    #[test]
    fn extract_ticket_from_branch_prefers_alpha_pattern() {
        assert_eq!(
            extract_ticket_from_branch("feature/PROJ-42-fix"),
            Some(("PROJ-42".to_string(), TICKET_SOURCE_BRANCH))
        );
        assert_eq!(
            extract_ticket_from_branch("03-20-pava-2120_desc"),
            Some(("PAVA-2120".to_string(), TICKET_SOURCE_BRANCH))
        );
    }

    #[test]
    fn extract_ticket_from_branch_falls_back_to_numeric() {
        // ADR-0082 §9 numeric-only fallback — matches branches like
        // `fix/1234-typo` used by GitHub-flow teams.
        assert_eq!(
            extract_ticket_from_branch("fix/1234-typo"),
            Some(("1234".to_string(), TICKET_SOURCE_BRANCH_NUMERIC))
        );
        assert_eq!(
            extract_ticket_from_branch("42-stabilize-auth"),
            Some(("42".to_string(), TICKET_SOURCE_BRANCH_NUMERIC))
        );
    }

    #[test]
    fn extract_ticket_from_branch_integration_branches_are_none() {
        for b in ["main", "master", "develop", "HEAD", ""] {
            assert_eq!(
                extract_ticket_from_branch(b),
                None,
                "{b} must not be treated as a ticket"
            );
        }
    }

    #[test]
    fn extract_ticket_from_branch_no_ticket_yields_none() {
        assert_eq!(extract_ticket_from_branch("kiyoshi/pava-searchbars"), None);
        assert_eq!(extract_ticket_from_branch("release/v1"), None);
    }

    #[test]
    fn extract_ticket_from_branch_alpha_wins_over_numeric() {
        // `fix/PROJ-42-and-1234` must resolve to the alpha ticket, not
        // the trailing numeric fragment.
        assert_eq!(
            extract_ticket_from_branch("fix/PROJ-42-and-1234"),
            Some(("PROJ-42".to_string(), TICKET_SOURCE_BRANCH))
        );
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
            prompt_category_source: None,
            prompt_category_confidence: None,
            tool_names: Vec::new(),
            tool_use_ids: Vec::new(),
        }
    }

    #[test]
    fn no_tags_on_user_messages() {
        use crate::config::TagsConfig;
        let mut pipeline = Pipeline::default_pipeline(Some(TagsConfig::default()));

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
        let mut pipeline = Pipeline::default_pipeline(Some(TagsConfig::default()));

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

        // Identity tags should appear only once across all messages.
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
    fn activity_tag_tracks_latest_prompt_category() {
        use crate::config::TagsConfig;
        let mut pipeline = Pipeline::default_pipeline(Some(TagsConfig::default()));

        let mut u1 = test_msg();
        u1.uuid = "u1".into();
        u1.session_id = Some("sess-1".into());
        u1.role = "user".into();
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
        u2.prompt_category = Some("feature".into());
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

        assert!(
            all_tags[1]
                .iter()
                .any(|t| t.key == "activity" && t.value == "bugfix"),
            "a1 should keep first activity tag, got: {:?}",
            all_tags[1]
        );
        assert!(
            all_tags[3]
                .iter()
                .any(|t| t.key == "activity" && t.value == "feature"),
            "a2 should inherit updated activity tag, got: {:?}",
            all_tags[3]
        );
    }

    #[test]
    fn tool_tags_emit_all_tools_per_message() {
        use crate::config::TagsConfig;
        let mut pipeline = Pipeline::default_pipeline(Some(TagsConfig::default()));

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
