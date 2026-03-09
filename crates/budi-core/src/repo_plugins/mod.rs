mod common;
pub(crate) mod nextjs;
pub(crate) mod react;

use crate::index::RuntimeIndex;
use crate::rpc::QueryResultItem;

use common::{contains_any, contains_any_literal, path_matches_any};

pub(crate) use common::{
    extract_chunk_line_with_needle, find_symbol_chunk, push_compact_evidence_line,
};

// Built-in repo plugins keep framework/language-specific heuristics and condensers
// out of the generic retrieval pipeline. Add new plugins here instead of
// extending `retrieval.rs` or `chunking.rs` with more framework branches.

pub(crate) struct ContextPackPlugin {
    pub synthetic_reason: &'static str,
    pub build_card: fn(&str, &RuntimeIndex, &[QueryResultItem]) -> Option<QueryResultItem>,
}

pub(crate) struct RepoPlugin {
    pub tag: &'static str,
    pub implied_tags: &'static [&'static str],
    pub match_chunk: fn(&str, &str, &str) -> bool,
    pub match_query: fn(&str) -> bool,
    pub context_pack: Option<ContextPackPlugin>,
}

const FLASK_PLUGIN: RepoPlugin = RepoPlugin {
    tag: "flask",
    implied_tags: &[],
    match_chunk: matches_flask_chunk,
    match_query: matches_flask_query,
    context_pack: None,
};

const DJANGO_PLUGIN: RepoPlugin = RepoPlugin {
    tag: "django",
    implied_tags: &[],
    match_chunk: matches_django_chunk,
    match_query: matches_django_query,
    context_pack: None,
};

const FASTAPI_PLUGIN: RepoPlugin = RepoPlugin {
    tag: "fastapi",
    implied_tags: &[],
    match_chunk: matches_fastapi_chunk,
    match_query: matches_fastapi_query,
    context_pack: None,
};

const EXPRESS_PLUGIN: RepoPlugin = RepoPlugin {
    tag: "express",
    implied_tags: &[],
    match_chunk: matches_express_chunk,
    match_query: matches_express_query,
    context_pack: None,
};

const BUILTIN_REPO_PLUGINS: &[RepoPlugin] = &[
    nextjs::PLUGIN,
    react::PLUGIN,
    FLASK_PLUGIN,
    DJANGO_PLUGIN,
    FASTAPI_PLUGIN,
    EXPRESS_PLUGIN,
];

pub fn ecosystem_tags_for_chunk(file_path: &str, language: &str, text: &str) -> Vec<String> {
    let lower_path = file_path.to_ascii_lowercase();
    let lower_text = text.to_ascii_lowercase();
    let mut tags = Vec::new();
    for plugin in BUILTIN_REPO_PLUGINS {
        if (plugin.match_chunk)(&lower_path, language, &lower_text) {
            push_unique_tag(&mut tags, plugin.tag);
            for implied in plugin.implied_tags {
                push_unique_tag(&mut tags, implied);
            }
        }
    }
    tags
}

pub fn detect_query_ecosystems(query: &str) -> Vec<String> {
    let lower = query.to_ascii_lowercase();
    let mut ecosystems = Vec::new();
    for plugin in BUILTIN_REPO_PLUGINS {
        if (plugin.match_query)(&lower) {
            push_unique_tag(&mut ecosystems, plugin.tag);
            for implied in plugin.implied_tags {
                push_unique_tag(&mut ecosystems, implied);
            }
        }
    }
    ecosystems
}

pub fn inject_context_plugins(
    query: &str,
    runtime: &RuntimeIndex,
    snippets: &mut Vec<QueryResultItem>,
    target_limit: usize,
) {
    if snippets.is_empty() {
        return;
    }
    for plugin in BUILTIN_REPO_PLUGINS {
        let Some(context_pack) = &plugin.context_pack else {
            continue;
        };
        if snippets.iter().any(|snippet| {
            snippet
                .reasons
                .iter()
                .any(|reason| reason == context_pack.synthetic_reason)
        }) {
            continue;
        }
        let Some(card) = (context_pack.build_card)(query, runtime, snippets.as_slice()) else {
            continue;
        };
        snippets.insert(0, card);
        if snippets.len() > target_limit {
            snippets.truncate(target_limit);
        }
    }
}

fn push_unique_tag(tags: &mut Vec<String>, tag: &str) {
    if !tags.iter().any(|existing| existing == tag) {
        tags.push(tag.to_string());
    }
}

fn matches_flask_chunk(lower_path: &str, language: &str, lower_text: &str) -> bool {
    language == "python"
        && (path_matches_any(lower_path, &["flask/", "/flask/"])
            || lower_path.ends_with("/wsgi.py")
            || contains_any_literal(
                lower_text,
                &[
                    "from flask",
                    "import flask",
                    "flask(__name__",
                    "blueprint(",
                    "@app.route(",
                    "@bp.route(",
                    "@blueprint.route(",
                    "current_app",
                    "wsgi_app(",
                ],
            ))
}

fn matches_flask_query(lower: &str) -> bool {
    contains_any(
        lower,
        &[
            "flask",
            "blueprint",
            "jinja",
            "wsgi_app",
            "current_app",
            "app.route",
        ],
    )
}

fn matches_django_chunk(lower_path: &str, language: &str, lower_text: &str) -> bool {
    language == "python"
        && (path_matches_any(lower_path, &["django/", "/django/"])
            || lower_path.ends_with("/manage.py")
            || lower_path.ends_with("/settings.py")
            || lower_path.ends_with("/urls.py")
            || contains_any_literal(
                lower_text,
                &[
                    "from django",
                    "import django",
                    "models.model",
                    "urlpatterns",
                    "from django.urls",
                    "from django.db",
                    "from django.http",
                    "from django.shortcuts",
                    "as_view(",
                ],
            ))
}

fn matches_django_query(lower: &str) -> bool {
    contains_any(
        lower,
        &[
            "django",
            "urlpatterns",
            "as_view",
            "manage.py",
            "settings.py",
        ],
    )
}

fn matches_fastapi_chunk(lower_path: &str, language: &str, lower_text: &str) -> bool {
    language == "python"
        && (path_matches_any(lower_path, &["fastapi/", "/fastapi/"])
            || contains_any_literal(
                lower_text,
                &[
                    "from fastapi",
                    "import fastapi",
                    "fastapi(",
                    "apirouter(",
                    "from starlette",
                ],
            ))
}

fn matches_fastapi_query(lower: &str) -> bool {
    contains_any(lower, &["fastapi", "apirouter", "pydantic"])
}

fn matches_express_chunk(_lower_path: &str, language: &str, lower_text: &str) -> bool {
    (language == "javascript" || language == "typescript")
        && contains_any_literal(
            lower_text,
            &[
                "from 'express'",
                "from \"express\"",
                "require('express')",
                "require(\"express\")",
                "express.router(",
            ],
        )
}

fn matches_express_query(lower: &str) -> bool {
    contains_any(lower, &["express", "express router", "express middleware"])
}
