use std::collections::HashSet;

use crate::index::{ChunkRecord, RuntimeIndex};
use crate::rpc::{QueryChannelScores, QueryResultItem};

use super::common::{
    contains_any, contains_any_literal, extract_chunk_line_with_needle, find_symbolish_chunk,
    path_matches_any, push_compact_evidence_line,
};
use super::{ContextPackPlugin, RepoPlugin};

pub(crate) const PLUGIN: RepoPlugin = RepoPlugin {
    tag: "react",
    implied_tags: &[],
    match_chunk: matches_react_chunk,
    match_query: matches_react_query,
    context_pack: Some(ContextPackPlugin {
        synthetic_reason: "react-effect-lifecycle-pack",
        build_card: build_react_context_pack,
    }),
};

fn matches_react_chunk(lower_path: &str, language: &str, lower_text: &str) -> bool {
    (language == "javascript" || language == "typescript")
        && (path_matches_any(lower_path, &["react/", "/react/", "react-", "/react-"])
            || lower_path.ends_with(".jsx")
            || lower_path.ends_with(".tsx")
            || contains_any_literal(
                lower_text,
                &[
                    "from 'react'",
                    "from \"react\"",
                    "require('react')",
                    "require(\"react\")",
                    "react.",
                    "usestate(",
                    "useeffect(",
                    "usememo(",
                    "usecallback(",
                    "useref(",
                    "usecontext(",
                    "usereducer(",
                    "createcontext(",
                    "forwardref(",
                ],
            ))
}

fn matches_react_query(lower: &str) -> bool {
    contains_any(
        lower,
        &[
            "react",
            "jsx",
            "tsx",
            "component",
            "components",
            "hook",
            "hooks",
            "useeffect",
            "usestate",
            "usememo",
            "usecallback",
            "useref",
            "context provider",
        ],
    )
}

fn build_react_context_pack(
    query: &str,
    runtime: &RuntimeIndex,
    snippets: &[QueryResultItem],
) -> Option<QueryResultItem> {
    if snippets.is_empty() || !is_react_effect_lifecycle_query(query) {
        return None;
    }
    let Some(layout_unmount_chunk) =
        find_symbolish_chunk(runtime, None, "commitHookLayoutUnmountEffects")
    else {
        return None;
    };
    let Some(layout_mount_chunk) = find_symbolish_chunk(runtime, None, "commitHookLayoutEffects")
    else {
        return None;
    };
    let Some(flush_layout_chunk) = find_symbolish_chunk(runtime, None, "flushLayoutEffects") else {
        return None;
    };
    let Some(flush_passive_chunk) = find_symbolish_chunk(runtime, None, "flushPassiveEffects")
    else {
        return None;
    };
    let Some(passive_unmount_chunk) =
        find_symbolish_chunk(runtime, None, "commitPassiveUnmountEffects")
    else {
        return None;
    };
    let Some(passive_mount_chunk) =
        find_symbolish_chunk(runtime, None, "commitPassiveMountEffects")
    else {
        return None;
    };
    let Some(hook_mount_chunk) = find_symbolish_chunk(runtime, None, "commitHookEffectListMount")
    else {
        return None;
    };
    let Some(hook_unmount_chunk) =
        find_symbolish_chunk(runtime, None, "commitHookEffectListUnmount")
    else {
        return None;
    };
    let top_score = snippets
        .first()
        .map(|snippet| snippet.score)
        .unwrap_or(0.40);
    build_react_effect_lifecycle_card(
        layout_unmount_chunk,
        layout_mount_chunk,
        flush_layout_chunk,
        flush_passive_chunk,
        passive_unmount_chunk,
        passive_mount_chunk,
        hook_unmount_chunk,
        hook_mount_chunk,
        top_score * 0.97,
    )
}

fn is_react_effect_lifecycle_query(query: &str) -> bool {
    let lower = query.to_ascii_lowercase();
    matches_react_query(&lower)
        && contains_any(
            &lower,
            &[
                "lifecycle",
                "mount",
                "unmount",
                "cleanup",
                "effect order",
                "layout effect",
                "passive effect",
                "useeffect",
                "uselayouteffect",
            ],
        )
        && contains_any(&lower, &["component", "hook", "hooks", "effect", "effects"])
}

pub(crate) fn build_react_effect_lifecycle_card(
    layout_unmount_chunk: &ChunkRecord,
    layout_mount_chunk: &ChunkRecord,
    flush_layout_chunk: &ChunkRecord,
    flush_passive_chunk: &ChunkRecord,
    passive_unmount_chunk: &ChunkRecord,
    passive_mount_chunk: &ChunkRecord,
    hook_unmount_chunk: &ChunkRecord,
    hook_mount_chunk: &ChunkRecord,
    score: f32,
) -> Option<QueryResultItem> {
    let layout_destroy_comment = extract_chunk_line_with_needle(
        layout_unmount_chunk,
        &["Layout effects are destroyed during the mutation phase"],
    );
    let layout_phase_line =
        extract_chunk_line_with_needle(flush_layout_chunk, &["commitLayoutEffects("]);
    let layout_mount_comment = extract_chunk_line_with_needle(
        layout_mount_chunk,
        &["layout effects have already been destroyed"],
    );
    let passive_unmount_call = extract_chunk_line_with_needle(
        flush_passive_chunk,
        &["commitPassiveUnmountEffects(root.current)"],
    );
    let passive_mount_call =
        extract_chunk_line_with_needle(flush_passive_chunk, &["commitPassiveMountEffects("]);
    let passive_unmount_delegate = extract_chunk_line_with_needle(
        passive_unmount_chunk,
        &["commitPassiveUnmountOnFiber(finishedWork)"],
    );
    let passive_mount_delegate =
        extract_chunk_line_with_needle(passive_mount_chunk, &["commitPassiveMountOnFiber("]);
    let hook_destroy_line =
        extract_chunk_line_with_needle(hook_unmount_chunk, &["safelyCallDestroy("]);
    let hook_create_line = extract_chunk_line_with_needle(hook_mount_chunk, &["destroy = create("])
        .or_else(|| extract_chunk_line_with_needle(hook_mount_chunk, &["inst.destroy = destroy"]));

    let summary =
        "order: mutation/layout cleanup -> layout mounts -> passive cleanup -> passive mounts";
    let mut text_lines = vec![summary.to_string()];
    let mut seen = HashSet::new();
    push_compact_evidence_line(
        &mut text_lines,
        &mut seen,
        "commitHookLayoutUnmountEffects",
        layout_destroy_comment,
    );
    push_compact_evidence_line(
        &mut text_lines,
        &mut seen,
        "flushLayoutEffects",
        layout_phase_line,
    );
    push_compact_evidence_line(
        &mut text_lines,
        &mut seen,
        "commitHookLayoutEffects",
        layout_mount_comment,
    );
    push_compact_evidence_line(
        &mut text_lines,
        &mut seen,
        "commitHookEffectListUnmount",
        hook_destroy_line,
    );
    push_compact_evidence_line(
        &mut text_lines,
        &mut seen,
        "flushPassiveEffects",
        passive_unmount_call,
    );
    push_compact_evidence_line(
        &mut text_lines,
        &mut seen,
        "commitPassiveUnmountEffects",
        passive_unmount_delegate,
    );
    push_compact_evidence_line(
        &mut text_lines,
        &mut seen,
        "flushPassiveEffects",
        passive_mount_call,
    );
    push_compact_evidence_line(
        &mut text_lines,
        &mut seen,
        "commitPassiveMountEffects",
        passive_mount_delegate,
    );
    push_compact_evidence_line(
        &mut text_lines,
        &mut seen,
        "commitHookEffectListMount",
        hook_create_line,
    );
    if text_lines.len() < 4 {
        return None;
    }
    let start_line = layout_unmount_chunk
        .start_line
        .min(layout_mount_chunk.start_line)
        .min(flush_layout_chunk.start_line)
        .min(flush_passive_chunk.start_line)
        .min(passive_unmount_chunk.start_line)
        .min(passive_mount_chunk.start_line)
        .min(hook_unmount_chunk.start_line)
        .min(hook_mount_chunk.start_line);
    let end_line = layout_unmount_chunk
        .end_line
        .max(layout_mount_chunk.end_line)
        .max(flush_layout_chunk.end_line)
        .max(flush_passive_chunk.end_line)
        .max(passive_unmount_chunk.end_line)
        .max(passive_mount_chunk.end_line)
        .max(hook_unmount_chunk.end_line)
        .max(hook_mount_chunk.end_line);
    Some(QueryResultItem {
        path: flush_passive_chunk.path.clone(),
        start_line,
        end_line,
        language: flush_passive_chunk.language.clone(),
        score,
        reasons: vec!["react-effect-lifecycle-pack".to_string()],
        channel_scores: QueryChannelScores::default(),
        text: text_lines.join("\n"),
        slm_relevance_note: Some("React effect lifecycle summary".to_string()),
    })
}
