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

/// Repo-level shape hint: detect a framework from manifest files (package.json, Cargo.toml, etc.)
/// and their dependency lists, so the ecosystem activates from project structure rather than
/// requiring chunk-level text matches.
pub(crate) struct RepoShapeHint {
    /// File paths (relative to repo root) that indicate the manifest, e.g. "package.json", "go.mod".
    /// Matched against indexed file paths using ends_with (to handle subdirectory manifests).
    pub manifest_paths: &'static [&'static str],
    /// Needles searched inside the manifest file content. If any needle matches, the plugin
    /// activates at repo level. For example `"\"react\""` in package.json.
    pub dependency_needles: &'static [&'static str],
    /// Structural paths: if any indexed file path matches (starts_with or contains), the
    /// plugin activates even without a manifest file. For example `"app/page."` for Next.js.
    pub structural_paths: &'static [&'static str],
}

impl RepoShapeHint {
    pub const fn new(
        manifest_paths: &'static [&'static str],
        dependency_needles: &'static [&'static str],
        structural_paths: &'static [&'static str],
    ) -> Self {
        Self {
            manifest_paths,
            dependency_needles,
            structural_paths,
        }
    }
}

pub(crate) struct RepoPlugin {
    pub tag: &'static str,
    pub implied_tags: &'static [&'static str],
    pub chunk_matcher: ChunkMatcher,
    pub query_matcher: QueryMatcher,
    pub context_pack: Option<ContextPackPlugin>,
    pub repo_shape: Option<RepoShapeHint>,
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
            repo_shape: None,
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
            repo_shape: None,
        }
    }

    pub const fn with_context_pack(mut self, context_pack: ContextPackPlugin) -> Self {
        self.context_pack = Some(context_pack);
        self
    }

    pub const fn with_repo_shape(mut self, repo_shape: RepoShapeHint) -> Self {
        self.repo_shape = Some(repo_shape);
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

/// Detect repo-level ecosystems from the repo root directory.
/// Scans manifest files on disk (package.json, pyproject.toml, etc.) and checks
/// indexed file paths for structural signals (app/page.tsx → nextjs).
/// Runs once at `RuntimeIndex` construction.
pub fn detect_repo_ecosystems(
    repo_root: &std::path::Path,
    files: &[crate::index::FileRecord],
) -> Vec<String> {
    let mut tags: Vec<String> = Vec::new();
    for plugin in BUILTIN_REPO_PLUGINS {
        let Some(shape) = &plugin.repo_shape else {
            continue;
        };
        if repo_shape_matches(repo_root, shape, files) {
            push_unique_tag(&mut tags, plugin.tag);
            for implied in plugin.implied_tags {
                push_unique_tag(&mut tags, implied);
            }
        }
    }
    tags
}

fn repo_shape_matches(
    repo_root: &std::path::Path,
    shape: &RepoShapeHint,
    files: &[crate::index::FileRecord],
) -> bool {
    // Check structural paths (from indexed file list — cheap, no disk I/O).
    if !shape.structural_paths.is_empty() {
        let structural_hit = files.iter().any(|f| {
            let lower = f.path.to_ascii_lowercase();
            shape
                .structural_paths
                .iter()
                .any(|pattern| lower.contains(pattern))
        });
        if structural_hit {
            return true;
        }
    }

    // Check manifest files on disk. These are typically not indexed (toml, json, txt)
    // but they're small and fast to read.
    if shape.manifest_paths.is_empty() {
        return false;
    }
    for manifest in shape.manifest_paths {
        let manifest_path = repo_root.join(manifest);
        if !manifest_path.is_file() {
            continue;
        }
        if shape.dependency_needles.is_empty() {
            // Manifest exists, no dependency check needed
            return true;
        }
        // Read the manifest (cap at 64 KB to avoid accidentally reading huge files)
        let Ok(content) = std::fs::read_to_string(&manifest_path) else {
            continue;
        };
        if content.len() > 65_536 {
            continue;
        }
        let lower_content = content.to_ascii_lowercase();
        if shape
            .dependency_needles
            .iter()
            .any(|needle| lower_content.contains(needle))
        {
            return true;
        }
    }
    false
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
    use super::{ChunkKeywordSignals, ChunkMatchContext, RepoPlugin, detect_repo_ecosystems};
    use crate::index::FileRecord;

    fn stub_file(path: &str) -> FileRecord {
        FileRecord {
            path: path.to_string(),
            hash: String::new(),
            size_bytes: 0,
            modified_unix_ms: 0,
        }
    }

    fn temp_repo(name: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("budi-repo-shape-{name}-{unique}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn detect_react_from_package_json_dependency() {
        let root = temp_repo("react");
        std::fs::write(
            root.join("package.json"),
            r#"{ "dependencies": { "react": "^18.2.0", "react-dom": "^18.2.0" } }"#,
        )
        .unwrap();
        let files = vec![stub_file("package.json"), stub_file("src/App.tsx")];
        let ecosystems = detect_repo_ecosystems(&root, &files);
        assert!(
            ecosystems.iter().any(|e| e == "react"),
            "expected react in {ecosystems:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_nextjs_from_next_config_structural_path() {
        let root = temp_repo("nextjs");
        // next.config.js structural path triggers nextjs even without reading the file
        let files = vec![stub_file("next.config.js"), stub_file("app/page.tsx")];
        let ecosystems = detect_repo_ecosystems(&root, &files);
        assert!(
            ecosystems.iter().any(|e| e == "nextjs"),
            "expected nextjs in {ecosystems:?}"
        );
        assert!(
            ecosystems.iter().any(|e| e == "react"),
            "expected implied react in {ecosystems:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_flask_from_pyproject_toml() {
        let root = temp_repo("flask");
        std::fs::write(
            root.join("pyproject.toml"),
            "[project]\nname = \"flask\"\ndependencies = [\"flask>=2.0\"]",
        )
        .unwrap();
        let files = vec![stub_file("pyproject.toml"), stub_file("src/flask/app.py")];
        let ecosystems = detect_repo_ecosystems(&root, &files);
        assert!(
            ecosystems.iter().any(|e| e == "flask"),
            "expected flask in {ecosystems:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_flask_from_structural_path() {
        let root = temp_repo("flask-struct");
        let files = vec![
            stub_file("src/flask/app.py"),
            stub_file("src/flask/__init__.py"),
        ];
        let ecosystems = detect_repo_ecosystems(&root, &files);
        assert!(
            ecosystems.iter().any(|e| e == "flask"),
            "expected flask from structural path in {ecosystems:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn no_false_positives_for_unrelated_repo() {
        let root = temp_repo("rust");
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"my-cli\"\nversion = \"1.0.0\"",
        )
        .unwrap();
        let files = vec![stub_file("Cargo.toml"), stub_file("src/main.rs")];
        let ecosystems = detect_repo_ecosystems(&root, &files);
        assert!(
            ecosystems.is_empty(),
            "expected no ecosystems for a plain Rust repo, got {ecosystems:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_express_from_package_json() {
        let root = temp_repo("express");
        std::fs::write(
            root.join("package.json"),
            r#"{ "dependencies": { "express": "^4.18.0" } }"#,
        )
        .unwrap();
        let files = vec![stub_file("package.json"), stub_file("src/server.js")];
        let ecosystems = detect_repo_ecosystems(&root, &files);
        assert!(
            ecosystems.iter().any(|e| e == "express"),
            "expected express in {ecosystems:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn implied_tags_are_included() {
        let root = temp_repo("nextjs-implied");
        std::fs::write(
            root.join("package.json"),
            r#"{ "dependencies": { "next": "^14.0.0" } }"#,
        )
        .unwrap();
        let files = vec![stub_file("package.json")];
        let ecosystems = detect_repo_ecosystems(&root, &files);
        assert!(ecosystems.iter().any(|e| e == "nextjs"));
        assert!(ecosystems.iter().any(|e| e == "react"));
        let _ = std::fs::remove_dir_all(&root);
    }

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
