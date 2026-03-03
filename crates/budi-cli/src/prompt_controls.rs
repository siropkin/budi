use budi_core::config::BudiConfig;
use budi_core::rpc::QueryDiagnostics;

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
        return Some("forced_skip".to_string());
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
        return Some("non-code-intent".to_string());
    }
    if !diagnostics.recommended_injection {
        if let Some(reason) = &diagnostics.skip_reason {
            return Some(reason.clone());
        }
        return Some("low-confidence".to_string());
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
