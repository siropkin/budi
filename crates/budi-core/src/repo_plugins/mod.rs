mod common;
pub(crate) mod django;
pub(crate) mod express;
pub(crate) mod fastapi;
pub(crate) mod flask;
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

pub(crate) struct ContextPackRequest<'a> {
    pub lower_query: &'a str,
    pub runtime: &'a RuntimeIndex,
    pub snippets: &'a [QueryResultItem],
}

pub(crate) struct ContextPackPlugin {
    pub synthetic_reason: &'static str,
    pub build_card: fn(&ContextPackRequest<'_>) -> Option<QueryResultItem>,
}

impl ContextPackPlugin {
    pub const fn new(
        synthetic_reason: &'static str,
        build_card: fn(&ContextPackRequest<'_>) -> Option<QueryResultItem>,
    ) -> Self {
        Self {
            synthetic_reason,
            build_card,
        }
    }
}

pub(crate) struct ChunkMatchContext<'a> {
    pub lower_path: &'a str,
    pub language: &'a str,
    pub lower_text: &'a str,
}

#[derive(Clone, Copy)]
pub(crate) struct ChunkKeywordSignals {
    pub languages: &'static [&'static str],
    pub path_needles: &'static [&'static str],
    pub path_suffixes: &'static [&'static str],
    pub text_needles: &'static [&'static str],
}

impl ChunkKeywordSignals {
    pub const fn new(
        languages: &'static [&'static str],
        path_needles: &'static [&'static str],
        path_suffixes: &'static [&'static str],
        text_needles: &'static [&'static str],
    ) -> Self {
        Self {
            languages,
            path_needles,
            path_suffixes,
            text_needles,
        }
    }
}

pub(crate) enum ChunkMatcher {
    Signals(ChunkKeywordSignals),
    Custom(fn(&ChunkMatchContext<'_>) -> bool),
}

pub(crate) enum QueryMatcher {
    Keywords(&'static [&'static str]),
    Custom(fn(&str) -> bool),
}

pub(crate) struct RepoPlugin {
    pub tag: &'static str,
    pub implied_tags: &'static [&'static str],
    pub chunk_matcher: ChunkMatcher,
    pub query_matcher: QueryMatcher,
    pub context_pack: Option<ContextPackPlugin>,
}

impl RepoPlugin {
    pub const fn simple(
        tag: &'static str,
        implied_tags: &'static [&'static str],
        chunk_signals: ChunkKeywordSignals,
        query_keywords: &'static [&'static str],
    ) -> Self {
        Self {
            tag,
            implied_tags,
            chunk_matcher: ChunkMatcher::Signals(chunk_signals),
            query_matcher: QueryMatcher::Keywords(query_keywords),
            context_pack: None,
        }
    }

    pub const fn custom(
        tag: &'static str,
        implied_tags: &'static [&'static str],
        match_chunk: fn(&ChunkMatchContext<'_>) -> bool,
        match_query: fn(&str) -> bool,
    ) -> Self {
        Self {
            tag,
            implied_tags,
            chunk_matcher: ChunkMatcher::Custom(match_chunk),
            query_matcher: QueryMatcher::Custom(match_query),
            context_pack: None,
        }
    }

    pub const fn with_context_pack(mut self, context_pack: ContextPackPlugin) -> Self {
        self.context_pack = Some(context_pack);
        self
    }

    fn matches_chunk(&self, context: &ChunkMatchContext<'_>) -> bool {
        match self.chunk_matcher {
            ChunkMatcher::Signals(signals) => chunk_keyword_signals_match(signals, context),
            ChunkMatcher::Custom(match_chunk) => match_chunk(context),
        }
    }

    fn matches_query(&self, lower_query: &str) -> bool {
        match self.query_matcher {
            QueryMatcher::Keywords(keywords) => contains_any(lower_query, keywords),
            QueryMatcher::Custom(match_query) => match_query(lower_query),
        }
    }
}

const BUILTIN_REPO_PLUGINS: &[RepoPlugin] = &[
    nextjs::PLUGIN,
    react::PLUGIN,
    flask::PLUGIN,
    django::PLUGIN,
    fastapi::PLUGIN,
    express::PLUGIN,
];

pub fn ecosystem_tags_for_chunk(file_path: &str, language: &str, text: &str) -> Vec<String> {
    let lower_path = file_path.to_ascii_lowercase();
    let lower_text = text.to_ascii_lowercase();
    let context = ChunkMatchContext {
        lower_path: &lower_path,
        language,
        lower_text: &lower_text,
    };
    let mut tags = Vec::new();
    for plugin in BUILTIN_REPO_PLUGINS {
        if plugin.matches_chunk(&context) {
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
        if plugin.matches_query(&lower) {
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
    let lower_query = query.to_ascii_lowercase();
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
        let request = ContextPackRequest {
            lower_query: &lower_query,
            runtime,
            snippets: snippets.as_slice(),
        };
        let Some(card) = (context_pack.build_card)(&request) else {
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

fn chunk_keyword_signals_match(
    signals: ChunkKeywordSignals,
    context: &ChunkMatchContext<'_>,
) -> bool {
    let language_matches =
        signals.languages.is_empty() || signals.languages.contains(&context.language);
    if !language_matches {
        return false;
    }
    path_matches_any(context.lower_path, signals.path_needles)
        || ends_with_any(context.lower_path, signals.path_suffixes)
        || contains_any_literal(context.lower_text, signals.text_needles)
}

fn ends_with_any(input: &str, suffixes: &[&str]) -> bool {
    suffixes.iter().any(|suffix| input.ends_with(suffix))
}

#[cfg(test)]
mod tests {
    use super::{ChunkKeywordSignals, ChunkMatchContext, RepoPlugin};

    #[test]
    fn simple_plugin_matches_suffix_and_query_keywords() {
        const TEST_PLUGIN: RepoPlugin = RepoPlugin::simple(
            "flask",
            &[],
            ChunkKeywordSignals::new(
                &["python"],
                &["flask/", "/flask/"],
                &["/wsgi.py"],
                &["from flask", "blueprint("],
            ),
            &["flask", "blueprint"],
        );

        let context = ChunkMatchContext {
            lower_path: "src/demo/wsgi.py",
            language: "python",
            lower_text: "def create_app():\n    return app",
        };
        assert!(TEST_PLUGIN.matches_chunk(&context));
        assert!(TEST_PLUGIN.matches_query("how does flask blueprint registration work?"));
    }

    #[test]
    fn simple_plugin_respects_language_filter() {
        const TEST_PLUGIN: RepoPlugin = RepoPlugin::simple(
            "express",
            &[],
            ChunkKeywordSignals::new(&["javascript", "typescript"], &[], &[], &["from 'express'"]),
            &["express"],
        );

        let wrong_language_context = ChunkMatchContext {
            lower_path: "src/server.py",
            language: "python",
            lower_text: "from 'express'",
        };
        assert!(!TEST_PLUGIN.matches_chunk(&wrong_language_context));
    }
}
