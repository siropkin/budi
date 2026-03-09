use std::collections::HashSet;

use crate::index::{ChunkRecord, RuntimeIndex};
use crate::rpc::{QueryChannelScores, QueryResultItem};

use super::common::{
    contains_any, contains_any_literal, extract_chunk_line_with_needle,
    extract_first_meaningful_line, path_matches_any, push_compact_evidence_line,
};
use super::{ChunkMatchContext, ContextPackPlugin, ContextPackRequest, RepoPlugin};

pub(crate) const PLUGIN: RepoPlugin = RepoPlugin::custom(
    "nextjs",
    &["react"],
    matches_nextjs_chunk,
    matches_nextjs_query,
)
.with_context_pack(ContextPackPlugin::new(
    "nextjs-app-router-pack",
    build_nextjs_context_pack,
));

fn matches_nextjs_chunk(context: &ChunkMatchContext<'_>) -> bool {
    if context.language != "javascript" && context.language != "typescript" {
        return false;
    }
    let next_config = context.lower_path.ends_with("next.config.js")
        || context.lower_path.ends_with("next.config.mjs")
        || context.lower_path.ends_with("next.config.cjs")
        || context.lower_path.ends_with("next.config.ts");
    let in_app_dir = context.lower_path.starts_with("app/") || context.lower_path.contains("/app/");
    let app_router_file = in_app_dir
        && (context.lower_path.ends_with("/page.js")
            || context.lower_path.ends_with("/page.jsx")
            || context.lower_path.ends_with("/page.ts")
            || context.lower_path.ends_with("/page.tsx")
            || context.lower_path.ends_with("/layout.js")
            || context.lower_path.ends_with("/layout.jsx")
            || context.lower_path.ends_with("/layout.ts")
            || context.lower_path.ends_with("/layout.tsx")
            || context.lower_path.ends_with("/loading.js")
            || context.lower_path.ends_with("/loading.tsx")
            || context.lower_path.ends_with("/error.js")
            || context.lower_path.ends_with("/error.tsx")
            || context.lower_path.ends_with("/route.js")
            || context.lower_path.ends_with("/route.ts"));
    let pages_router_file = context.lower_path.contains("/pages/");
    next_config
        || path_matches_any(
            context.lower_path,
            &["next/", "/next/", "nextjs/", "/nextjs/"],
        )
        || app_router_file
        || contains_any_literal(
            context.lower_text,
            &[
                "from 'next/",
                "from \"next/",
                "\"use client\"",
                "'use client'",
                "\"use server\"",
                "'use server'",
                "getserversideprops",
                "getstaticprops",
                "getstaticpaths",
                "generatemetadata",
                "generatestaticparams",
                "next/navigation",
                "next/link",
                "next/router",
                "next/server",
                "next/image",
            ],
        )
        || (pages_router_file
            && contains_any_literal(context.lower_text, &["next/", "getserversideprops"]))
}

fn matches_nextjs_query(lower: &str) -> bool {
    contains_any(
        lower,
        &[
            "next.js",
            "nextjs",
            "app router",
            "server component",
            "route handler",
            "page.tsx",
            "layout.tsx",
            "use client",
            "use server",
            "getserversideprops",
            "getstaticprops",
        ],
    )
}

fn build_nextjs_context_pack(request: &ContextPackRequest<'_>) -> Option<QueryResultItem> {
    if request.snippets.is_empty()
        || !is_nextjs_app_router_query(request.lower_query, request.snippets)
    {
        return None;
    }
    let top_score = request
        .snippets
        .first()
        .map(|snippet| snippet.score)
        .unwrap_or(0.40);
    build_nextjs_app_router_card(request.runtime, top_score * 0.96)
}

fn is_nextjs_app_router_query(lower_query: &str, snippets: &[QueryResultItem]) -> bool {
    let has_nextjs_signal = matches_nextjs_query(lower_query)
        || snippets.iter().any(|snippet| {
            let lower_path = snippet.path.to_ascii_lowercase();
            let lower_text = snippet.text.to_ascii_lowercase();
            matches_nextjs_chunk(&ChunkMatchContext {
                lower_path: &lower_path,
                language: &snippet.language,
                lower_text: &lower_text,
            })
        });
    if !has_nextjs_signal {
        return false;
    }
    contains_any(
        lower_query,
        &[
            "app router",
            "route boundary",
            "route boundaries",
            "route handler",
            "layout.tsx",
            "page.tsx",
            "route.ts",
            "segment",
            "segments",
            "nested route",
            "route ownership",
        ],
    ) || (contains_any(lower_query, &["layout", "page"])
        && contains_any(lower_query, &["route", "router", "api", "boundary"]))
}

fn build_nextjs_app_router_card(runtime: &RuntimeIndex, score: f32) -> Option<QueryResultItem> {
    let mut paths = runtime
        .all_chunks()
        .iter()
        .filter_map(|chunk| {
            if is_nextjs_app_router_file_path(&chunk.path) {
                Some(chunk.path.clone())
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();

    let mut entries = Vec::new();
    for path in paths {
        let Some(role) = nextjs_app_router_role_for_path(&path) else {
            continue;
        };
        let Some(segment) = nextjs_app_router_segment(&path) else {
            continue;
        };
        let Some(chunk) = best_nextjs_app_router_chunk(runtime, &path, role) else {
            continue;
        };
        entries.push((segment, role, path, chunk));
    }
    if entries.len() < 2 {
        return None;
    }
    entries.sort_by(
        |(segment_a, role_a, path_a, _), (segment_b, role_b, path_b, _)| {
            nextjs_segment_sort_key(segment_a)
                .cmp(&nextjs_segment_sort_key(segment_b))
                .then_with(|| nextjs_role_sort_key(role_a).cmp(&nextjs_role_sort_key(role_b)))
                .then_with(|| path_a.cmp(path_b))
        },
    );

    let mut text_lines = vec!["app-router inventory:".to_string()];
    let mut seen = HashSet::new();
    for (segment, role, path, chunk) in &entries {
        if *role == "route" {
            text_lines.push(format!("handler {segment} => route {path}"));
        } else {
            text_lines.push(format!("segment {segment} => {role} {path}"));
        }
        push_compact_evidence_line(
            &mut text_lines,
            &mut seen,
            path,
            nextjs_app_router_evidence(chunk, role),
        );
    }

    let first_chunk = entries.first()?.3;
    let start_line = entries
        .iter()
        .map(|(_, _, _, chunk)| chunk.start_line)
        .min()
        .unwrap_or(first_chunk.start_line);
    let end_line = entries
        .iter()
        .map(|(_, _, _, chunk)| chunk.end_line)
        .max()
        .unwrap_or(first_chunk.end_line);
    Some(QueryResultItem {
        path: first_chunk.path.clone(),
        start_line,
        end_line,
        language: first_chunk.language.clone(),
        score,
        reasons: vec!["nextjs-app-router-pack".to_string()],
        channel_scores: QueryChannelScores::default(),
        text: text_lines.join("\n"),
        slm_relevance_note: Some("Next.js app-router boundary summary".to_string()),
    })
}

fn best_nextjs_app_router_chunk<'a>(
    runtime: &'a RuntimeIndex,
    path: &str,
    role: &str,
) -> Option<&'a ChunkRecord> {
    let primary_needles = nextjs_primary_role_needles(role);
    let mut fallback: Option<&ChunkRecord> = None;
    for chunk in runtime
        .all_chunks()
        .iter()
        .filter(|chunk| chunk.path == path)
    {
        if fallback.is_none_or(|existing| chunk.start_line < existing.start_line) {
            fallback = Some(chunk);
        }
        if extract_chunk_line_with_needle(chunk, primary_needles).is_some() {
            return Some(chunk);
        }
    }
    fallback
}

fn nextjs_app_router_evidence(chunk: &ChunkRecord, role: &str) -> Option<(usize, String)> {
    extract_chunk_line_with_needle(chunk, nextjs_primary_role_needles(role))
        .or_else(|| extract_chunk_line_with_needle(chunk, nextjs_secondary_role_needles(role)))
        .or_else(|| extract_first_meaningful_line(chunk))
}

fn nextjs_primary_role_needles(role: &str) -> &'static [&'static str] {
    match role {
        "layout" | "page" | "loading" | "error" => {
            &["export default function", "export default async function"]
        }
        "route" => &[
            "export async function GET",
            "export function GET",
            "export async function POST",
            "export function POST",
            "export async function PUT",
            "export function PUT",
            "export async function DELETE",
            "export function DELETE",
        ],
        _ => &["export default function"],
    }
}

fn nextjs_secondary_role_needles(role: &str) -> &'static [&'static str] {
    match role {
        "layout" | "page" | "loading" | "error" => {
            &["export const metadata", "\"use client\"", "'use client'"]
        }
        "route" => &["Response.json("],
        _ => &[],
    }
}

fn nextjs_segment_sort_key(segment: &str) -> (usize, String) {
    let rank = if segment == "/" { 0 } else { 1 };
    (rank, segment.to_string())
}

fn nextjs_role_sort_key(role: &str) -> usize {
    match role {
        "layout" => 0,
        "page" => 1,
        "loading" => 2,
        "error" => 3,
        "route" => 4,
        _ => 5,
    }
}

fn nextjs_app_router_relative_path(path: &str) -> Option<&str> {
    path.strip_prefix("app/")
        .or_else(|| path.split_once("/app/").map(|(_, rest)| rest))
}

fn is_nextjs_app_router_file_path(path: &str) -> bool {
    let Some(rel) = nextjs_app_router_relative_path(path) else {
        return false;
    };
    nextjs_app_router_role_from_relative_path(rel).is_some()
}

fn nextjs_app_router_role_for_path(path: &str) -> Option<&'static str> {
    let rel = nextjs_app_router_relative_path(path)?;
    nextjs_app_router_role_from_relative_path(rel)
}

fn nextjs_app_router_role_from_relative_path(rel: &str) -> Option<&'static str> {
    if rel == "layout.js"
        || rel == "layout.jsx"
        || rel == "layout.ts"
        || rel == "layout.tsx"
        || rel.ends_with("/layout.js")
        || rel.ends_with("/layout.jsx")
        || rel.ends_with("/layout.ts")
        || rel.ends_with("/layout.tsx")
    {
        Some("layout")
    } else if rel == "page.js"
        || rel == "page.jsx"
        || rel == "page.ts"
        || rel == "page.tsx"
        || rel.ends_with("/page.js")
        || rel.ends_with("/page.jsx")
        || rel.ends_with("/page.ts")
        || rel.ends_with("/page.tsx")
    {
        Some("page")
    } else if rel == "loading.js"
        || rel == "loading.jsx"
        || rel == "loading.ts"
        || rel == "loading.tsx"
        || rel.ends_with("/loading.js")
        || rel.ends_with("/loading.jsx")
        || rel.ends_with("/loading.ts")
        || rel.ends_with("/loading.tsx")
    {
        Some("loading")
    } else if rel == "error.js"
        || rel == "error.jsx"
        || rel == "error.ts"
        || rel == "error.tsx"
        || rel.ends_with("/error.js")
        || rel.ends_with("/error.jsx")
        || rel.ends_with("/error.ts")
        || rel.ends_with("/error.tsx")
    {
        Some("error")
    } else if rel == "route.js"
        || rel == "route.ts"
        || rel.ends_with("/route.js")
        || rel.ends_with("/route.ts")
    {
        Some("route")
    } else {
        None
    }
}

fn nextjs_app_router_segment(path: &str) -> Option<String> {
    let rel = nextjs_app_router_relative_path(path)?;
    let (dir, _) = rel.rsplit_once('/').unwrap_or(("", rel));
    if dir.is_empty() {
        Some("/".to_string())
    } else {
        Some(format!("/{dir}"))
    }
}
