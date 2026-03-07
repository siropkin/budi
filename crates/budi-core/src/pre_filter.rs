/// Returns `true` only when the prompt is unambiguously non-code AND contains
/// no codebase-anchor words.  The bar is intentionally high so legitimate
/// code questions are never filtered out.
pub fn is_obviously_non_code(prompt: &str) -> bool {
    let lower = prompt.to_lowercase();

    // Creative-writing requests are filtered unless the prompt explicitly asks
    // about a code artifact (function, class, error handling code, etc.).
    let creative_patterns = ["write a poem", "write me a poem", "tell me a joke"];
    let strong_code_anchors = [
        "code",
        "function",
        "class",
        "implementation",
        "import",
        "error",
    ];
    if creative_patterns.iter().any(|p| lower.contains(p)) {
        return !strong_code_anchors.iter().any(|a| lower.contains(a));
    }

    let positive_non_code = [
        "movie recommendation",
        "translate this text",
        "weather forecast",
        "recipe for",
        "what's 2+2",
        "what's 2 + 2",
        "what is 2+2",
        "what is 2 + 2",
        "how do i make pasta",
        "make pasta",
    ];
    let repo_anchors = [
        "repo",
        "codebase",
        "project",
        "file",
        "function",
        "component",
        "class",
        "module",
        "hook",
        "route",
        "api",
        "service",
        "import",
        "build",
        "test",
        "code",
        "implementation",
        "library",
        "package",
        "error",
        "bug",
    ];

    let has_non_code = positive_non_code.iter().any(|p| lower.contains(p));
    let has_anchor = repo_anchors.iter().any(|a| lower.contains(a));
    has_non_code && !has_anchor
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_poem_request() {
        assert!(is_obviously_non_code("write a poem about summer"));
    }

    #[test]
    fn rejects_write_me_a_poem() {
        assert!(is_obviously_non_code("write me a poem about React hooks"));
    }

    #[test]
    fn rejects_math_with_spaces() {
        assert!(is_obviously_non_code("what's 2 + 2?"));
    }

    #[test]
    fn allows_code_question() {
        assert!(!is_obviously_non_code("how does the reconciler work"));
    }

    #[test]
    fn allows_poem_with_anchor() {
        assert!(!is_obviously_non_code(
            "write a poem about the error handling code"
        ));
    }

    #[test]
    fn rejects_pasta_recipe() {
        assert!(is_obviously_non_code("how do i make pasta"));
    }

    #[test]
    fn allows_fiber_question() {
        assert!(!is_obviously_non_code(
            "how does React fiber scheduler work"
        ));
    }
}
