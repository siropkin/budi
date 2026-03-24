//! Git utilities: lightweight helpers for AI commit detection.
//!
//! Heavy git enrichment (subprocess spawning, batch processing) has been removed.
//! The commits table schema is preserved for future use. AI commit detection is
//! now handled via tags during the normal sync pipeline.

/// Known AI tool name patterns found in Co-Authored-By commit trailers.
/// Matched case-insensitively against the value after "Co-authored-by:".
pub const AI_COAUTHOR_PATTERNS: &[&str] = &[
    "claude",
    "cursor",
    "copilot",
    "cline",
    "aider",
    "gemini",
    "devin",
    "windsurf",
];

/// Check if a commit message contains a Co-Authored-By trailer from a known AI tool.
pub fn has_ai_coauthor(message: &str) -> bool {
    for line in message.lines() {
        let trimmed = line.trim().to_lowercase();
        if let Some(value) = trimmed.strip_prefix("co-authored-by:") {
            let value = value.trim();
            for &pattern in AI_COAUTHOR_PATTERNS {
                if value.contains(pattern) {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coauthor_claude() {
        assert!(has_ai_coauthor(
            "Add feature\n\nCo-Authored-By: Claude <noreply@anthropic.com>"
        ));
    }

    #[test]
    fn coauthor_claude_opus() {
        assert!(has_ai_coauthor(
            "Fix bug\n\nCo-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
        ));
    }

    #[test]
    fn coauthor_copilot() {
        assert!(has_ai_coauthor(
            "Update readme\n\nCo-authored-by: GitHub Copilot <noreply@github.com>"
        ));
    }

    #[test]
    fn coauthor_case_insensitive() {
        assert!(has_ai_coauthor(
            "Fix\n\nco-authored-by: CLAUDE <noreply@anthropic.com>"
        ));
    }

    #[test]
    fn coauthor_not_ai() {
        assert!(!has_ai_coauthor(
            "Pair programming\n\nCo-authored-by: Alice <alice@example.com>"
        ));
    }

    #[test]
    fn coauthor_no_trailer() {
        assert!(!has_ai_coauthor("Just a regular commit message"));
        assert!(!has_ai_coauthor(""));
    }

    #[test]
    fn coauthor_multiple_trailers() {
        let msg = "Feature\n\nCo-authored-by: Bob <bob@test.com>\nCo-authored-by: Claude <noreply@anthropic.com>";
        assert!(has_ai_coauthor(msg));
    }
}
