use std::collections::HashSet;

use crate::config::BudiConfig;
use crate::reason_codes::{
    SKIP_REASON_FORCED_SKIP, SKIP_REASON_LOW_CONFIDENCE, SKIP_REASON_NON_CODE_INTENT,
    normalize_skip_reason,
};
use crate::rpc::{QueryDiagnostics, QueryResultItem};

#[derive(Debug, Clone, Copy, Default)]
pub struct PromptDirectives {
    pub force_skip: bool,
    pub force_inject: bool,
}

pub fn parse_prompt_directives(prompt: &str) -> PromptDirectives {
    let mut directives = PromptDirectives::default();
    for raw in prompt.split_whitespace() {
        let normalized = raw.trim_matches(|c: char| {
            matches!(
                c,
                ',' | '.' | ';' | ':' | '!' | '?' | '"' | '\'' | '`' | '(' | ')' | '[' | ']'
            )
        });
        if normalized.eq_ignore_ascii_case("@nobudi") {
            directives.force_skip = true;
        } else if normalized.eq_ignore_ascii_case("@forcebudi") {
            directives.force_inject = true;
        }
    }
    if directives.force_inject {
        directives.force_skip = false;
    }
    directives
}

pub fn sanitize_prompt_for_query(prompt: &str) -> String {
    let mut cleaned = Vec::new();
    for raw in prompt.split_whitespace() {
        let normalized = raw.trim_matches(|c: char| {
            matches!(
                c,
                ',' | '.' | ';' | ':' | '!' | '?' | '"' | '\'' | '`' | '(' | ')' | '[' | ']'
            )
        });
        if normalized.eq_ignore_ascii_case("@nobudi")
            || normalized.eq_ignore_ascii_case("@forcebudi")
        {
            continue;
        }
        cleaned.push(raw);
    }
    let sanitized = cleaned.join(" ").trim().to_string();
    if sanitized.is_empty() {
        prompt.to_string()
    } else {
        sanitized
    }
}

pub fn evaluate_context_skip(
    config: &BudiConfig,
    directives: &PromptDirectives,
    diagnostics: &QueryDiagnostics,
) -> Option<String> {
    if directives.force_skip {
        return Some(SKIP_REASON_FORCED_SKIP.to_string());
    }
    if directives.force_inject {
        return None;
    }
    if !config.smart_skip_enabled {
        return None;
    }
    if !diagnostics_available(diagnostics) {
        return None;
    }
    if config.skip_non_code_prompts && diagnostics.intent == "non-code" {
        return Some(SKIP_REASON_NON_CODE_INTENT.to_string());
    }
    if !diagnostics.recommended_injection {
        if let Some(reason) = &diagnostics.skip_reason {
            return Some(normalize_skip_reason(reason));
        }
        return Some(SKIP_REASON_LOW_CONFIDENCE.to_string());
    }
    None
}

pub fn excerpt(text: &str, config: &BudiConfig) -> String {
    if config.debug_io_full_text {
        return text.to_string();
    }
    if config.debug_io_max_chars == 0 {
        return String::new();
    }
    let max = config.debug_io_max_chars.max(64);
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max).collect::<String>()
}

fn diagnostics_available(diagnostics: &QueryDiagnostics) -> bool {
    !diagnostics.intent.is_empty()
        || diagnostics.top_score > 0.0
        || diagnostics.margin > 0.0
        || !diagnostics.signals.is_empty()
        || diagnostics.skip_reason.is_some()
}

fn runtime_guard_is_non_prod_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.starts_with("test/")
        || lower.starts_with("tests/")
        || lower.contains("/test/")
        || lower.contains("/tests/")
        || lower.starts_with("examples/")
        || lower.contains("/examples/")
        || lower.starts_with("example/")
        || lower.contains("/example/")
        || lower.contains("/fixtures/")
        || lower.contains("/fixture/")
}

pub fn build_runtime_guard_context(snippets: &[QueryResultItem]) -> String {
    let mut selected = Vec::new();
    let mut seen = HashSet::new();
    for snippet in snippets {
        let path = snippet.path.trim();
        if path.is_empty() {
            continue;
        }
        if runtime_guard_is_non_prod_path(path) {
            continue;
        }
        let has_runtime_signal = snippet.reasons.iter().any(|r| {
            r.starts_with("runtime-")
                || r.starts_with("symbol-hit")
                || r.starts_with("path-hit")
                || r.starts_with("graph-hit")
                || r.starts_with("lexical-hit")
        });
        if !has_runtime_signal {
            continue;
        }
        if !seen.insert(path.to_string()) {
            continue;
        }
        selected.push(path.to_string());
        if selected.len() >= 5 {
            break;
        }
    }
    if selected.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    out.push_str("[budi runtime guard]\n");
    out.push_str("rules:\n");
    out.push_str("- Use only the file paths listed below.\n");
    out.push_str(
        "- Prefer core source files; do not include tests/examples unless explicitly asked.\n",
    );
    out.push_str("- If unsure about function names, return file paths only.\n");
    out.push_str("verified_runtime_paths:\n");
    for path in selected {
        out.push_str("- ");
        out.push_str(&path);
        out.push('\n');
    }
    out
}
