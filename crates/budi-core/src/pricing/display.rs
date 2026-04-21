//! Display-name normalization for raw provider model ids (#443).
//!
//! The pricing manifest (ADR-0091) is the single source of truth for
//! **cost** — but it is sourced from LiteLLM and carries no display
//! metadata. Providers emit their own raw model strings
//! (`claude-opus-4-7`, `claude-4.5-opus-high-thinking`, `gpt-5.3-codex`,
//! `default`, etc.) and the same model family surfaces under different
//! names per provider, which makes `budi stats --models` look like
//! several different models when it is one.
//!
//! [`resolve`] translates a raw string into a [`DisplayModel`] with:
//!
//! - `display_name` — a human-readable family + version label shared
//!   across providers (e.g. `Claude Opus 4.7`).
//! - `effort` — an optional thinking / effort suffix rendered as its
//!   own column rather than concatenated into the name.
//! - `placeholder` — distinguishes a real resolved name from the two
//!   common "no model attributed" cases: Cursor's `default` (Auto
//!   mode) and Budi's `(untagged)` sentinel row.
//!
//! The overlay is intentionally conservative — families we do not
//! recognise fall through to the raw string unchanged, so a new model
//! name appearing upstream is never silently relabelled.
//!
//! # UTF-8 safety
//!
//! All slicing / `strip_*` / pattern matching operates on `&str`
//! methods that are char-boundary-safe by construction; no byte-index
//! math runs against `raw`. Multi-byte model ids round-trip through
//! `resolve` unchanged (see `utf8_safety_on_non_ascii_raw`).

/// Why a raw string did not resolve to a real model name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placeholder {
    /// Real model — `display_name` is authoritative.
    None,
    /// Cursor emits `default` for sessions where the user selected Auto
    /// and the routed model is not disclosed. Rendered as
    /// `Cursor Auto` so the row still reads truthfully.
    CursorAuto,
    /// Budi's `(untagged)` sentinel for messages where no model field
    /// was captured. Rendered as `(model not yet attributed)` — the
    /// `budi stats --models` text view suppresses zero-cost rows of
    /// this class by default (a known Cursor-lag transient).
    NotAttributed,
}

/// Normalised display metadata for one raw provider model id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayModel {
    /// The exact string emitted by the provider — unchanged for
    /// scripting callers that filter on `model`.
    pub raw: String,
    /// Human-readable name (e.g. `Claude Opus 4.7`). Falls back to
    /// `raw` when the family is not recognised.
    pub display_name: String,
    /// Effort / thinking modifier (e.g. `thinking-high`, `xhigh`).
    /// `None` when the raw string carries no effort suffix.
    pub effort: Option<String>,
    /// Placeholder class, or `Placeholder::None` for a real resolved
    /// model.
    pub placeholder: Placeholder,
}

impl DisplayModel {
    /// `display_name` + `effort` rendered as a single column (effort
    /// parenthesised). Used by the breakdown text view when the
    /// caller does not want a separate effort column.
    pub fn combined_label(&self) -> String {
        match &self.effort {
            Some(e) => format!("{} · {}", self.display_name, e),
            None => self.display_name.clone(),
        }
    }
}

/// The two Budi-owned placeholder sentinel strings. Kept in one place
/// so callers building JSON / text rows can reuse the exact wording.
pub const UNATTRIBUTED_LABEL: &str = "(model not yet attributed)";
pub const CURSOR_AUTO_LABEL: &str = "Cursor Auto";

/// Resolve a raw provider model id into its display metadata.
///
/// Unknown families fall through — `display_name == raw`, `effort ==
/// None`, `placeholder == None`. This is deliberate: a new model
/// appearing upstream is never silently relabelled to something it
/// is not.
pub fn resolve(raw: &str) -> DisplayModel {
    // --- Placeholders -----------------------------------------------
    if raw == crate::analytics::UNTAGGED_DIMENSION {
        return DisplayModel {
            raw: raw.to_string(),
            display_name: UNATTRIBUTED_LABEL.to_string(),
            effort: None,
            placeholder: Placeholder::NotAttributed,
        };
    }
    if raw == "default" {
        return DisplayModel {
            raw: raw.to_string(),
            display_name: CURSOR_AUTO_LABEL.to_string(),
            effort: None,
            placeholder: Placeholder::CursorAuto,
        };
    }

    // --- Anthropic family -------------------------------------------
    // Canonical Anthropic API: `claude-<tier>-<major>-<minor>[-<date>]`
    // Cursor (newer):           `claude-<tier>-<major>-<minor>-<effort>`
    //                           e.g. `claude-opus-4-7-thinking-high`
    // Cursor (older):           `claude-<major>.<minor>-<tier>-<effort>`
    //                           e.g. `claude-4.5-opus-high-thinking`
    if let Some(m) = parse_anthropic(raw) {
        return m;
    }

    // --- OpenAI / Codex family --------------------------------------
    // Codex CLI:  `gpt-5.3-codex`
    // Cursor:     `gpt-5.3-codex-high`, `gpt-5.3-codex-xhigh`
    // OpenAI:     `gpt-4o`, `gpt-4.1-mini`, `o1-preview`, …
    if let Some(m) = parse_openai(raw) {
        return m;
    }

    // --- Fallback: unknown family ------------------------------------
    DisplayModel {
        raw: raw.to_string(),
        display_name: raw.to_string(),
        effort: None,
        placeholder: Placeholder::None,
    }
}

/// The compact alias catalogue surfaced by `budi pricing status`.
/// Each entry is `(raw_example, display_name, effort)`. Curated from
/// the fresh-user smoke pass (2026-04-20, ticket #443 body) so the
/// output answers "what do the raw names in my transcript resolve
/// to?" without enumerating every LiteLLM id.
pub fn known_aliases() -> &'static [(&'static str, &'static str, Option<&'static str>)] {
    &[
        // Anthropic — canonical API names
        ("claude-opus-4-7", "Claude Opus 4.7", None),
        ("claude-opus-4-6", "Claude Opus 4.6", None),
        ("claude-opus-4-5", "Claude Opus 4.5", None),
        ("claude-sonnet-4-6", "Claude Sonnet 4.6", None),
        ("claude-sonnet-4-5", "Claude Sonnet 4.5", None),
        ("claude-haiku-4-5-20251001", "Claude Haiku 4.5", None),
        // Anthropic via Cursor (newer naming)
        (
            "claude-opus-4-7-thinking-high",
            "Claude Opus 4.7",
            Some("thinking-high"),
        ),
        // Anthropic via Cursor (older naming — transposed)
        (
            "claude-4.6-opus-high-thinking",
            "Claude Opus 4.6",
            Some("thinking-high"),
        ),
        (
            "claude-4.5-opus-high-thinking",
            "Claude Opus 4.5",
            Some("thinking-high"),
        ),
        // OpenAI / Codex
        ("gpt-5.3-codex", "GPT-5.3 Codex", None),
        ("gpt-5.3-codex-high", "GPT-5.3 Codex", Some("high")),
        ("gpt-5.3-codex-xhigh", "GPT-5.3 Codex", Some("xhigh")),
        // Cursor placeholders
        ("default", CURSOR_AUTO_LABEL, None),
    ]
}

// ---------------------------------------------------------------------------
// Family parsers
// ---------------------------------------------------------------------------

fn parse_anthropic(raw: &str) -> Option<DisplayModel> {
    // Newer Cursor + canonical API: starts with `claude-<tier>-`
    const TIERS: &[&str] = &["opus", "sonnet", "haiku"];
    for tier in TIERS {
        let prefix = format!("claude-{tier}-");
        if let Some(rest) = raw.strip_prefix(&prefix) {
            // Version is the leading `<major>-<minor>` digits (numeric
            // tokens only; anything after is effort or a date).
            let (version, effort) = split_anthropic_version_and_effort(rest);
            if version.is_empty() {
                continue;
            }
            return Some(DisplayModel {
                raw: raw.to_string(),
                display_name: format!("Claude {} {}", title_case(tier), version),
                effort,
                placeholder: Placeholder::None,
            });
        }
    }

    // Older Cursor: `claude-<major>.<minor>-<tier>-<effort>` (effort
    // tokens transposed; for example `claude-4.5-opus-high-thinking`).
    if let Some(rest) = raw.strip_prefix("claude-") {
        // Take the leading version token (digits + dots + dashes until
        // we hit a tier keyword).
        let (version, after_version) = rest.split_once('-')?;
        if !is_numeric_version(version) {
            return None;
        }
        for tier in TIERS {
            let tier_prefix = format!("{tier}-");
            if let Some(after_tier) = after_version.strip_prefix(&tier_prefix) {
                let effort = normalise_effort(after_tier);
                return Some(DisplayModel {
                    raw: raw.to_string(),
                    display_name: format!("Claude {} {}", title_case(tier), version),
                    effort,
                    placeholder: Placeholder::None,
                });
            }
            // Edge case: `claude-4.5-opus` with no trailing effort.
            if after_version == *tier {
                return Some(DisplayModel {
                    raw: raw.to_string(),
                    display_name: format!("Claude {} {}", title_case(tier), version),
                    effort: None,
                    placeholder: Placeholder::None,
                });
            }
        }
    }
    None
}

/// Given the tail after `claude-<tier>-` (canonical Anthropic /
/// newer Cursor), split into the dotted-version label and the
/// optional effort suffix.
///
/// - `4-7`                    → (`"4.7"`, `None`)
/// - `4-7-thinking-high`      → (`"4.7"`, `Some("thinking-high")`)
/// - `4-5-20251001`           → (`"4.5"`, `None`)  — date stripped
/// - `4-5-20251001-thinking`  → (`"4.5"`, `Some("thinking")`)
fn split_anthropic_version_and_effort(tail: &str) -> (String, Option<String>) {
    // Collect leading numeric tokens; the first non-numeric token
    // closes the version.
    let mut version_tokens: Vec<&str> = Vec::new();
    let mut remaining_tokens: Vec<&str> = Vec::new();
    let mut past_version = false;
    for tok in tail.split('-') {
        if !past_version {
            if tok.chars().all(|c| c.is_ascii_digit()) {
                version_tokens.push(tok);
                continue;
            }
            past_version = true;
        }
        remaining_tokens.push(tok);
    }

    // Anthropic version is the first two numeric tokens (`major` +
    // `minor`); any additional numeric token is the release date and
    // is dropped from the display.
    let version = match version_tokens.as_slice() {
        [] => String::new(),
        [major] => (*major).to_string(),
        [major, minor, ..] => format!("{major}.{minor}"),
    };
    let effort = normalise_effort(&remaining_tokens.join("-"));
    (version, effort)
}

fn parse_openai(raw: &str) -> Option<DisplayModel> {
    // `gpt-5.3-codex[-<effort>]` / `gpt-5.3-codex-xhigh` — Codex via
    // Cursor; strip the effort suffix into its own column.
    if let Some(rest) = raw.strip_prefix("gpt-") {
        // Leading version token (allow digits + dots).
        let (version, after_version) = match rest.split_once('-') {
            Some((v, rest)) => (v, rest),
            None => (rest, ""),
        };
        if !is_numeric_version(version) {
            return None;
        }
        if after_version.is_empty() {
            return Some(DisplayModel {
                raw: raw.to_string(),
                display_name: format!("GPT-{version}"),
                effort: None,
                placeholder: Placeholder::None,
            });
        }
        // `codex[-<effort>]` → `GPT-<version> Codex` + effort column.
        if let Some(after_codex) = after_version.strip_prefix("codex") {
            let effort = if after_codex.is_empty() {
                None
            } else if let Some(e) = after_codex.strip_prefix('-') {
                normalise_effort(e)
            } else {
                // Fell through to a `codex<suffix>` shape we do not
                // recognise; keep the suffix as the effort so the
                // caller can see it.
                Some(after_codex.to_string())
            };
            return Some(DisplayModel {
                raw: raw.to_string(),
                display_name: format!("GPT-{version} Codex"),
                effort,
                placeholder: Placeholder::None,
            });
        }
        // Any other `gpt-<ver>-<suffix>` we keep as-is but split the
        // suffix into `effort` so the table column stays consistent.
        return Some(DisplayModel {
            raw: raw.to_string(),
            display_name: format!("GPT-{version}"),
            effort: normalise_effort(after_version),
            placeholder: Placeholder::None,
        });
    }
    None
}

/// Canonicalise an effort label. Returns `None` for the empty string
/// and for the `"default"` Cursor places on rows that carry no effort
/// routing.
fn normalise_effort(raw: &str) -> Option<String> {
    let trimmed = raw.trim_matches('-');
    if trimmed.is_empty() || trimmed == "default" {
        return None;
    }
    // `high-thinking` → `thinking-high` (older Cursor transposed the
    // order; newer form is the canonical one).
    if trimmed == "high-thinking" {
        return Some("thinking-high".to_string());
    }
    if trimmed == "low-thinking" {
        return Some("thinking-low".to_string());
    }
    Some(trimmed.to_string())
}

/// `true` when `s` looks like a version token (digits and dots only,
/// with at least one digit).
fn is_numeric_version(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit() || c == '.')
}

fn title_case(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_canonical_anthropic_names() {
        let r = resolve("claude-opus-4-7");
        assert_eq!(r.display_name, "Claude Opus 4.7");
        assert_eq!(r.effort, None);
        assert_eq!(r.placeholder, Placeholder::None);

        let r = resolve("claude-sonnet-4-6");
        assert_eq!(r.display_name, "Claude Sonnet 4.6");
        assert_eq!(r.effort, None);

        // Dated Anthropic release id — date stripped from display.
        let r = resolve("claude-haiku-4-5-20251001");
        assert_eq!(r.display_name, "Claude Haiku 4.5");
        assert_eq!(r.effort, None);
    }

    #[test]
    fn resolve_cursor_new_style_anthropic() {
        let r = resolve("claude-opus-4-7-thinking-high");
        assert_eq!(r.display_name, "Claude Opus 4.7");
        assert_eq!(r.effort.as_deref(), Some("thinking-high"));
        assert_eq!(r.placeholder, Placeholder::None);
    }

    #[test]
    fn resolve_cursor_old_style_anthropic_transposes_effort() {
        // Same family (`claude-4.5-opus-high-thinking`) must render
        // the same display name as the canonical form; effort is
        // the `thinking-high` canonical order, not the transposed
        // `high-thinking` Cursor emits.
        let r = resolve("claude-4.5-opus-high-thinking");
        assert_eq!(r.display_name, "Claude Opus 4.5");
        assert_eq!(r.effort.as_deref(), Some("thinking-high"));

        let r = resolve("claude-4.6-opus-high-thinking");
        assert_eq!(r.display_name, "Claude Opus 4.6");
        assert_eq!(r.effort.as_deref(), Some("thinking-high"));
    }

    #[test]
    fn resolve_gpt_codex_family() {
        let r = resolve("gpt-5.3-codex");
        assert_eq!(r.display_name, "GPT-5.3 Codex");
        assert_eq!(r.effort, None);

        let r = resolve("gpt-5.3-codex-high");
        assert_eq!(r.display_name, "GPT-5.3 Codex");
        assert_eq!(r.effort.as_deref(), Some("high"));

        let r = resolve("gpt-5.3-codex-xhigh");
        assert_eq!(r.display_name, "GPT-5.3 Codex");
        assert_eq!(r.effort.as_deref(), Some("xhigh"));
    }

    #[test]
    fn resolve_cursor_auto_placeholder() {
        let r = resolve("default");
        assert_eq!(r.display_name, "Cursor Auto");
        assert_eq!(r.effort, None);
        assert_eq!(r.placeholder, Placeholder::CursorAuto);
    }

    #[test]
    fn resolve_untagged_placeholder() {
        let r = resolve(crate::analytics::UNTAGGED_DIMENSION);
        assert_eq!(r.display_name, UNATTRIBUTED_LABEL);
        assert_eq!(r.effort, None);
        assert_eq!(r.placeholder, Placeholder::NotAttributed);
    }

    #[test]
    fn resolve_unknown_family_falls_through_to_raw() {
        // A new Google / Gemini name we have not mapped yet — keep the
        // raw string rather than silently relabelling.
        let r = resolve("gemini-2.5-pro-experimental");
        assert_eq!(r.display_name, "gemini-2.5-pro-experimental");
        assert_eq!(r.effort, None);
        assert_eq!(r.placeholder, Placeholder::None);
    }

    #[test]
    fn combined_label_joins_effort_with_middot() {
        let r = resolve("claude-opus-4-7-thinking-high");
        assert_eq!(r.combined_label(), "Claude Opus 4.7 · thinking-high");

        let r = resolve("claude-opus-4-7");
        assert_eq!(r.combined_label(), "Claude Opus 4.7");
    }

    #[test]
    fn utf8_safety_on_non_ascii_raw() {
        // Multi-byte characters in the raw id must not panic.
        let r = resolve("café-model-7");
        assert_eq!(r.display_name, "café-model-7");
        assert_eq!(r.placeholder, Placeholder::None);
    }

    #[test]
    fn known_aliases_every_entry_roundtrips_through_resolve() {
        for (raw, expected_display, expected_effort) in known_aliases() {
            let r = resolve(raw);
            assert_eq!(
                r.display_name, *expected_display,
                "display mismatch for raw={raw}",
            );
            assert_eq!(
                r.effort.as_deref(),
                *expected_effort,
                "effort mismatch for raw={raw}",
            );
        }
    }
}
