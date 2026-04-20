//! Defensive tag-emission helpers for first-class analytics dimensions.
//!
//! ## Contract (#335)
//!
//! Every first-class dimension tag (`activity`, `ticket_id`, `file_path`,
//! `tool_outcome`) carries a **sibling source tag** (`*_source`) and — for
//! `activity` / `file_path` / `tool_outcome` — a sibling confidence tag
//! (`*_confidence`). Analytics surfaces such as `budi stats --activities`
//! rely on the pair being present to render explainable per-group labels;
//! a headline tag without its siblings silently degrades those surfaces
//! to `src=?` / `confidence=?`.
//!
//! Historically this invariant was maintained "by construction" at each
//! emission site using `if let Some(...)` around the sibling pushes, and
//! kept honest by `propagate_session_context` filling defaults upstream.
//! That worked but the invariant lived entirely in reviewer discipline —
//! a future contributor extending the pipeline could quietly emit a
//! headline tag alone.
//!
//! The helpers below make that impossible at the type level: the
//! sibling values are non-`Option` parameters, so the compiler rejects
//! any call site that tries to emit the headline without also providing
//! source and (where applicable) confidence.

use crate::analytics::Tag;
use crate::tag_keys as tk;

/// Emit the `ticket_id` + `ticket_prefix` + `ticket_source` triplet.
///
/// The prefix is derived from `ticket` so callers cannot accidentally
/// ship a mismatched pair. Numeric-only tickets (ADR-0082 §9) do not
/// carry a `-` separator and therefore skip the `ticket_prefix` tag,
/// matching the R1.3 contract recorded in the numeric-fallback test.
pub(crate) fn ticket(tags: &mut Vec<Tag>, ticket: &str, source: &str) {
    tags.push(Tag {
        key: tk::TICKET_ID.to_string(),
        value: ticket.to_string(),
    });
    if let Some(dash) = ticket.find('-') {
        tags.push(Tag {
            key: tk::TICKET_PREFIX.to_string(),
            value: ticket[..dash].to_string(),
        });
    }
    tags.push(Tag {
        key: tk::TICKET_SOURCE.to_string(),
        value: source.to_string(),
    });
}

/// Emit the `activity` + `activity_source` + `activity_confidence`
/// triplet. Callers must supply defaults (`hooks::SOURCE_RULE` /
/// `hooks::CONF_MEDIUM`) when upstream enrichers left them unset.
pub(crate) fn activity(tags: &mut Vec<Tag>, activity: &str, source: &str, confidence: &str) {
    tags.push(Tag {
        key: tk::ACTIVITY.to_string(),
        value: activity.to_string(),
    });
    tags.push(Tag {
        key: tk::ACTIVITY_SOURCE.to_string(),
        value: source.to_string(),
    });
    tags.push(Tag {
        key: tk::ACTIVITY_CONFIDENCE.to_string(),
        value: confidence.to_string(),
    });
}

/// Emit a batch of `file_path` tags plus the shared
/// `file_path_source` / `file_path_confidence` siblings.
///
/// `paths` must be non-empty — the caller is responsible for
/// short-circuiting when attribution produced nothing. This keeps the
/// helper aligned with the ADR-0083 invariant that file siblings are
/// only meaningful when at least one path was accepted.
pub(crate) fn file_paths(tags: &mut Vec<Tag>, paths: Vec<String>, source: &str, confidence: &str) {
    debug_assert!(
        !paths.is_empty(),
        "emit::file_paths called with empty paths — siblings would be meaningless"
    );
    tags.reserve(paths.len() + 2);
    for path in paths {
        tags.push(Tag {
            key: tk::FILE_PATH.to_string(),
            value: path,
        });
    }
    tags.push(Tag {
        key: tk::FILE_PATH_SOURCE.to_string(),
        value: source.to_string(),
    });
    tags.push(Tag {
        key: tk::FILE_PATH_CONFIDENCE.to_string(),
        value: confidence.to_string(),
    });
}

/// Emit one `tool_outcome` tag per distinct outcome plus the shared
/// `tool_outcome_source` / `tool_outcome_confidence` siblings.
///
/// `outcomes` must be non-empty. Callers already guard on that — the
/// assertion here exists to keep the sibling-always-present invariant
/// true even if a future refactor forgets.
pub(crate) fn tool_outcomes<I>(tags: &mut Vec<Tag>, outcomes: I, source: &str, confidence: &str)
where
    I: IntoIterator,
    I::Item: AsRef<str>,
{
    let before = tags.len();
    for outcome in outcomes {
        tags.push(Tag {
            key: tk::TOOL_OUTCOME.to_string(),
            value: outcome.as_ref().to_string(),
        });
    }
    debug_assert!(
        tags.len() > before,
        "emit::tool_outcomes called with empty outcomes — siblings would be meaningless"
    );
    tags.push(Tag {
        key: tk::TOOL_OUTCOME_SOURCE.to_string(),
        value: source.to_string(),
    });
    tags.push(Tag {
        key: tk::TOOL_OUTCOME_CONFIDENCE.to_string(),
        value: confidence.to_string(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticket_emits_triplet_for_alphanumeric() {
        let mut tags = Vec::new();
        ticket(&mut tags, "PAVA-2057", "branch");
        let kv: Vec<(&str, &str)> = tags
            .iter()
            .map(|t| (t.key.as_str(), t.value.as_str()))
            .collect();
        assert_eq!(
            kv,
            vec![
                ("ticket_id", "PAVA-2057"),
                ("ticket_prefix", "PAVA"),
                ("ticket_source", "branch"),
            ]
        );
    }

    #[test]
    fn ticket_skips_prefix_for_numeric_only() {
        let mut tags = Vec::new();
        ticket(&mut tags, "1234", "branch_numeric");
        let kv: Vec<(&str, &str)> = tags
            .iter()
            .map(|t| (t.key.as_str(), t.value.as_str()))
            .collect();
        assert_eq!(
            kv,
            vec![("ticket_id", "1234"), ("ticket_source", "branch_numeric"),]
        );
    }

    #[test]
    fn activity_emits_full_triplet() {
        let mut tags = Vec::new();
        activity(&mut tags, "coding", "rule", "medium");
        let kv: Vec<(&str, &str)> = tags
            .iter()
            .map(|t| (t.key.as_str(), t.value.as_str()))
            .collect();
        assert_eq!(
            kv,
            vec![
                ("activity", "coding"),
                ("activity_source", "rule"),
                ("activity_confidence", "medium"),
            ]
        );
    }

    #[test]
    fn file_paths_emits_paths_then_siblings() {
        let mut tags = Vec::new();
        file_paths(
            &mut tags,
            vec!["src/a.rs".into(), "src/b.rs".into()],
            "tool_arg",
            "high",
        );
        let kv: Vec<(&str, &str)> = tags
            .iter()
            .map(|t| (t.key.as_str(), t.value.as_str()))
            .collect();
        assert_eq!(
            kv,
            vec![
                ("file_path", "src/a.rs"),
                ("file_path", "src/b.rs"),
                ("file_path_source", "tool_arg"),
                ("file_path_confidence", "high"),
            ]
        );
    }

    #[test]
    #[should_panic(expected = "emit::file_paths called with empty paths")]
    fn file_paths_rejects_empty_input_in_debug() {
        let mut tags = Vec::new();
        file_paths(&mut tags, Vec::new(), "tool_arg", "high");
    }

    #[test]
    fn tool_outcomes_emits_each_outcome_then_siblings() {
        let mut tags = Vec::new();
        tool_outcomes(
            &mut tags,
            ["success", "error"].iter().copied(),
            "jsonl_tool_result",
            "high",
        );
        let kv: Vec<(&str, &str)> = tags
            .iter()
            .map(|t| (t.key.as_str(), t.value.as_str()))
            .collect();
        assert_eq!(
            kv,
            vec![
                ("tool_outcome", "success"),
                ("tool_outcome", "error"),
                ("tool_outcome_source", "jsonl_tool_result"),
                ("tool_outcome_confidence", "high"),
            ]
        );
    }

    #[test]
    #[should_panic(expected = "emit::tool_outcomes called with empty outcomes")]
    fn tool_outcomes_rejects_empty_input_in_debug() {
        let mut tags = Vec::new();
        let empty: [&str; 0] = [];
        tool_outcomes(
            &mut tags,
            empty.iter().copied(),
            "jsonl_tool_result",
            "high",
        );
    }
}
