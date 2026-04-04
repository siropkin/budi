//! Identity normalization helpers shared across providers and storage layers.
//!
//! We normalize historical provider-prefixed session IDs (e.g. `cursor-<uuid>`)
//! into canonical plain UUIDs (`<uuid>`).

const SESSION_ID_PREFIXES: &[&str] = &[
    "cursor-",
    "claude-",
    "claude_code-",
    "windsurf-",
    "copilot-",
    "codex-",
];

/// Normalize a session ID into canonical form.
///
/// - Returns lowercased UUID when input is a UUID.
/// - Strips known provider prefix when the suffix is a UUID.
/// - Otherwise returns trimmed input unchanged.
pub fn normalize_session_id(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    if is_uuid_like(trimmed) {
        return trimmed.to_ascii_lowercase();
    }

    for prefix in SESSION_ID_PREFIXES {
        if let Some(rest) = trimmed.strip_prefix(prefix)
            && is_uuid_like(rest)
        {
            return rest.to_ascii_lowercase();
        }
    }

    trimmed.to_string()
}

/// Normalize an optional session ID.
/// Empty values normalize to None.
pub fn normalize_optional_session_id(raw: Option<&str>) -> Option<String> {
    raw.map(normalize_session_id).filter(|s| !s.is_empty())
}

fn is_uuid_like(s: &str) -> bool {
    if s.len() != 36 {
        return false;
    }
    for (idx, b) in s.as_bytes().iter().enumerate() {
        let is_dash = matches!(idx, 8 | 13 | 18 | 23);
        if is_dash {
            if *b != b'-' {
                return false;
            }
            continue;
        }
        if !b.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::{normalize_optional_session_id, normalize_session_id};

    #[test]
    fn keeps_plain_uuid() {
        assert_eq!(
            normalize_session_id("D99DFE22-D05C-4C78-8698-015D06E5DABB"),
            "d99dfe22-d05c-4c78-8698-015d06e5dabb"
        );
    }

    #[test]
    fn strips_known_provider_prefix_for_uuid() {
        assert_eq!(
            normalize_session_id("cursor-d99dfe22-d05c-4c78-8698-015d06e5dabb"),
            "d99dfe22-d05c-4c78-8698-015d06e5dabb"
        );
        assert_eq!(
            normalize_session_id("claude-d99dfe22-d05c-4c78-8698-015d06e5dabb"),
            "d99dfe22-d05c-4c78-8698-015d06e5dabb"
        );
    }

    #[test]
    fn does_not_strip_non_uuid_suffix() {
        assert_eq!(
            normalize_session_id("cursor-synth-1756319902000"),
            "cursor-synth-1756319902000"
        );
        assert_eq!(
            normalize_session_id("onboarding-smoke-20260401-130910"),
            "onboarding-smoke-20260401-130910"
        );
    }

    #[test]
    fn optional_normalization_drops_empty() {
        assert_eq!(normalize_optional_session_id(Some("   ")), None);
        assert_eq!(normalize_optional_session_id(None), None);
    }
}
