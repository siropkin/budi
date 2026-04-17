//! Prompt classification heuristics for JSONL and proxy ingestion.
//!
//! The activity classifier is intentionally rule-based and explainable — every
//! label can be traced back to a keyword match in this file. See ADR-0088 §5:
//! 8.1 classification stays on the simplest trustworthy path (deterministic
//! heuristics, no new local LLM on the primary path).
//!
//! ## Output
//!
//! Callers should prefer [`classify_prompt_detailed`], which returns a
//! [`Classification`] with `category`, `source`, and `confidence`. The legacy
//! [`classify_prompt`] helper remains for callers that only need the category
//! string.
//!
//! ## Contract (R1.2 / #222)
//!
//! - `source` is a stable string label for the producer. The primary 8.1 path
//!   is always [`SOURCE_RULE`]; the proxy layer tags explicit caller intent
//!   (via `X-Budi-Activity`) as [`SOURCE_HEADER`] in 9.0+.
//! - `confidence` is one of [`CONF_HIGH`], [`CONF_MEDIUM`], [`CONF_LOW`].
//!   It is derived from how many distinct keyword signals a prompt produced
//!   within the chosen category and whether the prompt is short enough that
//!   the label could plausibly be noise.
//! - The taxonomy is `bugfix`, `refactor`, `testing`, `review`, `ops`,
//!   `question`, `writing`, `docs`, `feature`. `docs` was split out of
//!   `writing` in 8.1 so internal documentation (README, changelog, inline
//!   comments) is distinguishable from prose writing. All other labels are
//!   stable with the 8.0 contract.

/// Stable classifier source label for rule-based (keyword) classification.
/// Every label emitted by [`classify_prompt_detailed`] reports this source.
pub const SOURCE_RULE: &str = "rule";

/// Reserved classifier source for explicit caller intent (e.g. a proxy
/// `X-Budi-Activity` header). Not emitted by this function today but kept
/// as part of the stable label set so surfaces can render it without
/// special-casing.
pub const SOURCE_HEADER: &str = "header";

/// Classifier is confident: multiple independent keyword signals matched in
/// the chosen category, or the prompt starts with an anchor phrase and has
/// enough context to make the label trustworthy.
pub const CONF_HIGH: &str = "high";

/// Classifier found a clear single signal with enough context to trust it.
pub const CONF_MEDIUM: &str = "medium";

/// Classifier matched on a single weak signal in a short prompt. Surfaces
/// should render `low` aggregates with visual de-emphasis.
pub const CONF_LOW: &str = "low";

/// Classification result attached to a user prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    pub category: String,
    pub source: &'static str,
    pub confidence: &'static str,
}

fn contains_word(text: &str, word: &str) -> bool {
    let mut start = 0;
    while let Some(pos) = text[start..].find(word) {
        let abs_pos = start + pos;
        let before_ok = abs_pos == 0 || !text.as_bytes()[abs_pos - 1].is_ascii_alphanumeric();
        let after_pos = abs_pos + word.len();
        let after_ok =
            after_pos >= text.len() || !text.as_bytes()[after_pos].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
        start = abs_pos + 1;
    }
    false
}

fn count_matches(text: &str, words: &[&str]) -> usize {
    words.iter().filter(|w| contains_word(text, w)).count()
}

fn starts_with_any(text: &str, phrases: &[&str]) -> bool {
    phrases.iter().any(|p| text.starts_with(p))
}

// ---------------------------------------------------------------------------
// Keyword tables.
//
// Keep these explainable and maintainable: every entry must be a reason a
// developer would describe their prompt in that category. Adding entries is
// safe as long as they do not overlap an earlier-priority category.
// ---------------------------------------------------------------------------

const BUGFIX_WORDS: &[&str] = &[
    "fix",
    "bug",
    "broken",
    "error",
    "crash",
    "issue",
    "debug",
    "failing",
    "fails",
    "wrong",
    "regression",
    "workaround",
    "patch",
    "hotfix",
    "not working",
    "doesn't work",
    "does not work",
    "isn't working",
    "stopped working",
];
/// Unambiguous verbs that mean "I want a fix". When the prompt starts with
/// one of these we keep bugfix precedence even if it mentions question-style
/// framing ("how do I fix …").
const BUGFIX_ACTION_PHRASES: &[&str] = &[
    "fix ",
    "debug ",
    "patch ",
    "hotfix ",
    "resolve ",
    "repair ",
    "workaround ",
];

const REFACTOR_WORDS: &[&str] = &[
    "refactor",
    "rename",
    "clean up",
    "extract",
    "reorganize",
    "simplify",
    "restructure",
    "move",
    "split",
    "consolidate",
    "deduplicate",
    "dedup",
    "inline",
    "remove",
    "delete",
    "deprecate",
    "replace",
    "convert",
    "rewrite",
    "tidy",
];

const TESTING_WORDS: &[&str] = &[
    "test",
    "tests",
    "testing",
    "spec",
    "specs",
    "unit test",
    "integration test",
    "e2e",
    "coverage",
    "assert",
    "mock",
    "fixture",
    "snapshot",
];

const REVIEW_WORDS: &[&str] = &[
    "review",
    "audit",
    "validate",
    "verify",
    "inspect",
    "look at",
    "take a look",
    "examine",
    "analyze",
    "analyse",
    "assess",
    "evaluate",
    "feedback",
    "critique",
];

const OPS_WORDS: &[&str] = &[
    "deploy",
    "release",
    "migrate",
    "upgrade",
    "bump",
    "publish",
    "install",
    "commit",
    "push",
    "merge",
    "rebase",
    "cherry-pick",
    "rollback",
    "revert",
    "configure",
    "provision",
    "ci",
    "cd",
    "docker",
    "k8s",
    "kubernetes",
    "terraform",
    "ansible",
];

const QUESTION_WORDS: &[&str] = &[
    "why",
    "how does",
    "how do",
    "how can",
    "how to",
    "how much",
    "how often",
    "how many",
    "what is",
    "what does",
    "what are",
    "where is",
    "where does",
    "where are",
    "when does",
    "when is",
    "which",
    "can you tell",
    "can you explain",
    "explain",
    "understand",
    "show me",
    "discover",
    "research",
    "what happens",
    "is there",
    "are there",
    "do we",
    "does this",
    "could you",
    "tell me",
];
/// Question-style prompts that should beat bugfix precedence when the prompt
/// starts with them. "explain the error" is a question, not a bug report.
const QUESTION_ANCHOR_PHRASES: &[&str] = &[
    "explain",
    "what is",
    "what are",
    "what does",
    "what happens",
    "why does",
    "why is",
    "why do",
    "why are",
    "how does",
    "how do",
    "how can",
    "how to",
    "show me",
    "tell me",
    "describe",
    "summarize",
    "summarise",
    "walk me through",
    "walk through",
    "can you explain",
    "can you tell",
    "is there",
    "are there",
];

/// Writing = prose (blog, article, draft). `docs` is the internal-documentation
/// counterpart.
const WRITING_WORDS: &[&str] = &["write", "draft", "article", "post", "blog"];

/// Internal documentation signals. These are typical of enterprise-developer
/// workflows: READMEs, runbooks, inline comments, docstrings. Keeping `docs`
/// separate from `writing` makes it easier for a developer to see "I spent
/// 3h on docs this week" vs "I wrote a blog post". Added in 8.1 (#222).
const DOCS_WORDS: &[&str] = &[
    "readme",
    "changelog",
    "documentation",
    "docstring",
    "comment",
    "comments",
    "docs",
    "doc",
    "runbook",
    "adr",
];

const FEATURE_WORDS: &[&str] = &[
    "add",
    "implement",
    "create",
    "build",
    "new feature",
    "integrate",
    "introduce",
    "design",
    "make",
    "change",
    "modify",
    "adjust",
    "tweak",
    "set up",
    "setup",
    "enable",
    "support",
    "extend",
    "enhance",
    "improve",
    "optimize",
    "update",
];

const PLAN_WORDS: &[&str] = &[
    "plan",
    "the plan",
    "implement the plan",
    "read and implement",
];

/// Classify a user prompt into a rule-based [`Classification`].
///
/// Returns `None` when the prompt is too short or too structural (slash
/// commands, bare XML tags) to carry meaningful intent. See the module doc
/// for the stable taxonomy contract.
pub fn classify_prompt_detailed(text: &str) -> Option<Classification> {
    let lower = text.to_lowercase();

    if lower.starts_with('/') || lower.len() < 5 {
        return None;
    }
    if lower.starts_with('<') && !lower.contains(' ') {
        return None;
    }

    let bugfix_hits = count_matches(&lower, BUGFIX_WORDS);
    let refactor_hits = count_matches(&lower, REFACTOR_WORDS);
    let testing_hits = count_matches(&lower, TESTING_WORDS);
    let review_hits = count_matches(&lower, REVIEW_WORDS);
    let ops_hits = count_matches(&lower, OPS_WORDS);
    let question_hits = count_matches(&lower, QUESTION_WORDS);
    let writing_hits = count_matches(&lower, WRITING_WORDS);
    let docs_hits = count_matches(&lower, DOCS_WORDS);
    let feature_hits = count_matches(&lower, FEATURE_WORDS);
    let plan_hits = count_matches(&lower, PLAN_WORDS);

    // Precedence: when a prompt leads with a question anchor phrase, prefer
    // `question` over `bugfix`. This fixes the long-standing "explain the
    // error" false-positive where the word "error" drove the prompt into
    // the bugfix bucket.
    let starts_with_question_anchor = starts_with_any(&lower, QUESTION_ANCHOR_PHRASES);
    let starts_with_bugfix_action = starts_with_any(&lower, BUGFIX_ACTION_PHRASES);

    let (category, hits): (&str, usize) =
        if bugfix_hits > 0 && (starts_with_bugfix_action || !starts_with_question_anchor) {
            ("bugfix", bugfix_hits)
        } else if refactor_hits > 0 {
            ("refactor", refactor_hits)
        } else if testing_hits > 0 {
            ("testing", testing_hits)
        } else if plan_hits > 0 {
            // Plan prompts are a flavour of feature scoping.
            ("feature", plan_hits)
        } else if review_hits > 0 {
            ("review", review_hits)
        } else if ops_hits > 0 {
            ("ops", ops_hits)
        } else if question_hits > 0 || lower.ends_with('?') {
            let q = question_hits.max(if lower.ends_with('?') { 1 } else { 0 });
            ("question", q)
        } else if docs_hits > 0 {
            ("docs", docs_hits)
        } else if writing_hits > 0 {
            ("writing", writing_hits)
        } else if feature_hits > 0 {
            ("feature", feature_hits)
        } else {
            return None;
        };

    // Confidence heuristic:
    // - Multiple distinct keyword matches in the chosen category → high.
    // - Prompt starts with an anchor phrase and has some context → medium.
    // - Single short match with <40 chars of context → low.
    // - Otherwise → medium.
    let len = lower.len();
    let anchored = starts_with_question_anchor || starts_with_bugfix_action;
    let confidence = if hits >= 2 {
        CONF_HIGH
    } else if hits == 1 && (len >= 40 || anchored) {
        CONF_MEDIUM
    } else {
        CONF_LOW
    };

    Some(Classification {
        category: category.to_string(),
        source: SOURCE_RULE,
        confidence,
    })
}

/// Legacy convenience wrapper that returns only the category string.
/// Prefer [`classify_prompt_detailed`] when source / confidence are needed
/// for downstream surfaces.
pub fn classify_prompt(text: &str) -> Option<String> {
    classify_prompt_detailed(text).map(|c| c.category)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_prompt_bugfix() {
        assert_eq!(
            classify_prompt("fix the login bug"),
            Some("bugfix".to_string())
        );
    }

    #[test]
    fn classify_prompt_feature() {
        assert_eq!(
            classify_prompt("add a new button to the dashboard"),
            Some("feature".to_string())
        );
    }

    #[test]
    fn classify_prompt_question() {
        assert_eq!(
            classify_prompt("how does this work?"),
            Some("question".to_string())
        );
    }

    #[test]
    fn classify_prompt_skips_short() {
        assert_eq!(classify_prompt("hi"), None);
        assert_eq!(classify_prompt("ok cool"), None);
    }

    #[test]
    fn classify_prompt_skips_commands() {
        assert_eq!(classify_prompt("<command>/clear</command>"), None);
        assert_eq!(classify_prompt("/exit"), None);
    }

    // ------------------------------------------------------------------
    // R1.2 (#222) additions
    // ------------------------------------------------------------------

    #[test]
    fn detailed_returns_source_and_confidence() {
        let c = classify_prompt_detailed("fix the login bug please").expect("must classify");
        assert_eq!(c.category, "bugfix");
        assert_eq!(c.source, SOURCE_RULE);
        // Single-keyword match in a short-ish prompt → medium (len ≥ 20).
        assert!(
            matches!(c.confidence, CONF_MEDIUM | CONF_HIGH),
            "expected medium/high, got {:?}",
            c.confidence
        );
    }

    #[test]
    fn high_confidence_multi_signal() {
        let c = classify_prompt_detailed("fix the crash and patch the regression")
            .expect("must classify");
        assert_eq!(c.category, "bugfix");
        assert_eq!(c.confidence, CONF_HIGH);
    }

    #[test]
    fn low_confidence_single_weak_keyword() {
        // "remove" is in refactor, prompt is short, no other signals.
        let c = classify_prompt_detailed("remove it").expect("must classify");
        assert_eq!(c.category, "refactor");
        assert_eq!(c.confidence, CONF_LOW);
    }

    #[test]
    fn question_beats_bugfix_when_leading_with_explain() {
        // Previously "explain the error" would land in bugfix because of
        // the word "error". #222: question anchors override bugfix.
        let c =
            classify_prompt_detailed("explain the error in the login flow").expect("must classify");
        assert_eq!(c.category, "question");
    }

    #[test]
    fn bugfix_keeps_precedence_when_action_verb_leads() {
        // "fix" leading should still win even if prompt contains question
        // words like "why".
        let c =
            classify_prompt_detailed("fix why the build fails on macOS").expect("must classify");
        assert_eq!(c.category, "bugfix");
    }

    #[test]
    fn docs_split_from_writing() {
        assert_eq!(
            classify_prompt("update the README with setup instructions"),
            Some("docs".to_string())
        );
        assert_eq!(
            classify_prompt("add a changelog entry"),
            Some("docs".to_string())
        );
        // Prose writing stays in `writing`.
        assert_eq!(
            classify_prompt("draft a blog post about rust async"),
            Some("writing".to_string())
        );
    }

    #[test]
    fn source_is_always_rule_for_primary_path() {
        for prompt in [
            "refactor the auth module",
            "write unit tests for handler",
            "review this pull request",
            "what is the current memory usage?",
            "deploy the release",
        ] {
            let c = classify_prompt_detailed(prompt).expect("classifiable");
            assert_eq!(c.source, SOURCE_RULE, "prompt: {prompt}");
        }
    }
}
