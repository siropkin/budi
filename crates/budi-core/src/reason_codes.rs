pub const SKIP_REASON_FORCED_SKIP: &str = "forced_skip";
pub const SKIP_REASON_NON_CODE_INTENT: &str = "non-code-intent";
pub const SKIP_REASON_LOW_CONFIDENCE: &str = "low-confidence";

pub const HOOK_REASON_SKIP_PREFIX: &str = "skip:";
pub const HOOK_REASON_OK: &str = "ok";
pub const HOOK_REASON_DAEMON_UNAVAILABLE: &str = "daemon_unavailable";
pub const HOOK_REASON_QUERY_TIMEOUT: &str = "query_timeout";
pub const HOOK_REASON_QUERY_TRANSPORT_ERROR: &str = "query_transport_error";
pub const HOOK_REASON_QUERY_HTTP_ERROR: &str = "query_http_error";
pub const HOOK_REASON_QUERY_ERROR: &str = "query_error";
pub const HOOK_REASON_RESPONSE_PARSE_ERROR: &str = "response_parse_error";
pub const HOOK_REASON_UPDATE_TIMEOUT: &str = "request_timeout";
pub const HOOK_REASON_UPDATE_CONNECT_ERROR: &str = "request_connect_error";
pub const HOOK_REASON_UPDATE_HTTP_ERROR: &str = "request_http_error";
pub const HOOK_REASON_UPDATE_FAILED: &str = "request_failed";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReasonKind {
    ForcedSkip,
    NonCodeIntent,
    LowConfidence,
    Other,
}

pub fn classify_skip_reason(raw: &str) -> SkipReasonKind {
    let value = raw.trim();
    if value.is_empty() {
        return SkipReasonKind::Other;
    }
    if value == SKIP_REASON_FORCED_SKIP {
        return SkipReasonKind::ForcedSkip;
    }
    if value == SKIP_REASON_NON_CODE_INTENT {
        return SkipReasonKind::NonCodeIntent;
    }
    if value.starts_with(SKIP_REASON_LOW_CONFIDENCE) {
        return SkipReasonKind::LowConfidence;
    }
    SkipReasonKind::Other
}

pub fn normalize_skip_reason(raw: &str) -> String {
    let value = raw
        .trim()
        .strip_prefix(HOOK_REASON_SKIP_PREFIX)
        .unwrap_or(raw.trim());
    match classify_skip_reason(value) {
        SkipReasonKind::ForcedSkip => SKIP_REASON_FORCED_SKIP.to_string(),
        SkipReasonKind::NonCodeIntent => SKIP_REASON_NON_CODE_INTENT.to_string(),
        SkipReasonKind::LowConfidence => SKIP_REASON_LOW_CONFIDENCE.to_string(),
        SkipReasonKind::Other => value.to_string(),
    }
}

pub fn format_low_confidence_skip_reason(confidence: f32) -> String {
    format!("{SKIP_REASON_LOW_CONFIDENCE}:{confidence:.3}")
}

pub fn format_skip_hook_reason(skip_reason: &str) -> String {
    format!(
        "{HOOK_REASON_SKIP_PREFIX}{}",
        normalize_skip_reason(skip_reason)
    )
}

pub fn normalize_hook_reason(reason: &str) -> String {
    if let Some(rest) = reason.strip_prefix(HOOK_REASON_SKIP_PREFIX) {
        return format_skip_hook_reason(rest);
    }
    reason.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn low_confidence_skip_reason_normalizes() {
        assert_eq!(
            normalize_skip_reason("low-confidence:0.387"),
            SKIP_REASON_LOW_CONFIDENCE
        );
    }

    #[test]
    fn skip_hook_reason_normalizes_prefixed_values() {
        assert_eq!(
            normalize_hook_reason("skip:low-confidence:0.321"),
            "skip:low-confidence"
        );
    }

    #[test]
    fn skip_hook_reason_does_not_double_prefix_custom_codes() {
        assert_eq!(
            normalize_hook_reason("skip:skip:runtime-intent-abstain"),
            "skip:runtime-intent-abstain"
        );
    }
}
