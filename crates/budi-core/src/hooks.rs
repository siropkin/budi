//! Prompt classification heuristics for JSONL ingestion.

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

/// Classify a user prompt into a category using keyword heuristics.
/// Returns None if no category matches (system commands, very short, or ambiguous).
///
/// Categories: bugfix, refactor, testing, review, ops, question, writing, feature.
pub fn classify_prompt(text: &str) -> Option<String> {
    let lower = text.to_lowercase();

    if lower.starts_with('/') || lower.len() < 5 {
        return None;
    }
    if lower.starts_with('<') && !lower.contains(' ') {
        return None;
    }

    let bugfix_words = [
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
    let refactor_words = [
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
    let testing_words = [
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
    let review_words = [
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
    let ops_words = [
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
    let question_words = [
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
    let feature_words = [
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
    let writing_words = [
        "write",
        "draft",
        "article",
        "post",
        "document",
        "blog",
        "readme",
        "changelog",
        "documentation",
    ];
    let plan_words = [
        "plan",
        "the plan",
        "implement the plan",
        "read and implement",
    ];

    if bugfix_words.iter().any(|w| contains_word(&lower, w)) {
        Some("bugfix".to_string())
    } else if refactor_words.iter().any(|w| contains_word(&lower, w)) {
        Some("refactor".to_string())
    } else if testing_words.iter().any(|w| contains_word(&lower, w)) {
        Some("testing".to_string())
    } else if plan_words.iter().any(|w| contains_word(&lower, w)) {
        Some("feature".to_string())
    } else if review_words.iter().any(|w| contains_word(&lower, w)) {
        Some("review".to_string())
    } else if ops_words.iter().any(|w| contains_word(&lower, w)) {
        Some("ops".to_string())
    } else if question_words.iter().any(|w| contains_word(&lower, w)) || lower.ends_with('?') {
        Some("question".to_string())
    } else if writing_words.iter().any(|w| contains_word(&lower, w)) {
        Some("writing".to_string())
    } else if feature_words.iter().any(|w| contains_word(&lower, w)) {
        Some("feature".to_string())
    } else {
        None
    }
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
}
