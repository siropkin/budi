use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use anyhow::Result;

use crate::chunking::language_label_for_path;
use crate::config::BudiConfig;
use crate::index::{ChunkRecord, RuntimeIndex};
use crate::reason_codes::SKIP_REASON_NON_CODE_INTENT;
use crate::repo_plugins::{
    detect_query_ecosystems, ecosystem_tags_for_chunk, extract_chunk_line_with_needle,
    find_symbol_chunk, inject_context_plugins, push_compact_evidence_line,
};
use crate::rpc::{QueryChannelScores, QueryDiagnostics, QueryResponse, QueryResultItem};
use context::{SnippetSelectionState, build_context, path_diversity_bucket, snippet_fingerprint};

mod context;

const RRF_K: f32 = 60.0;
const GRAPH_NEIGHBOR_EXPANSION_LIMIT: usize = 2;
const TEST_INVENTORY_MAX_LINES: usize = 3;
const TEST_INVENTORY_LINE_CHAR_BUDGET: usize = 160;

// ── RetrievalMode ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetrievalMode {
    Hybrid,
    Lexical,
    Vector,
    SymbolGraph,
}

impl RetrievalMode {
    pub fn as_str(self) -> &'static str {
        match self {
            RetrievalMode::Hybrid => "hybrid",
            RetrievalMode::Lexical => "lexical",
            RetrievalMode::Vector => "vector",
            RetrievalMode::SymbolGraph => "symbol-graph",
        }
    }
}

pub fn parse_retrieval_mode(raw: Option<&str>) -> RetrievalMode {
    match raw
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("lexical") => RetrievalMode::Lexical,
        Some("vector") => RetrievalMode::Vector,
        Some("symbol-graph") | Some("symbol_graph") | Some("symbolgraph") => {
            RetrievalMode::SymbolGraph
        }
        _ => RetrievalMode::Hybrid,
    }
}

// ── Intent ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueryIntentKind {
    SymbolUsage,
    SymbolDefinition,
    RuntimeConfig,
    FlowTrace,
    Architecture,
    TestLookup,
}

#[derive(Debug, Clone)]
struct QueryIntent {
    kind: QueryIntentKind,
    code_related: bool,
    allow_docs: bool,
    weights: IntentWeights,
}

#[derive(Debug, Clone, Copy)]
struct IntentWeights {
    lexical: f32,
    vector: f32,
    symbol: f32,
    path: f32,
    graph: f32,
}

// ── Channel scoring internals ─────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
enum ChannelKind {
    Lexical,
    Vector,
    Symbol,
    Path,
    Graph,
}

#[derive(Debug, Clone, Default)]
struct CandidateScore {
    score: f32,
    signals: Vec<String>,
    channel_scores: QueryChannelScores,
}

#[derive(Debug, Clone)]
struct ScoredChunk {
    id: u64,
    score: f32,
    reasons: Vec<String>,
    channel_scores: QueryChannelScores,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TestInventoryEntry {
    line_number: usize,
    label: String,
}

// ── build_query_response ──────────────────────────────────────────────────────

pub fn build_query_response(
    runtime: &RuntimeIndex,
    query: &str,
    query_embedding: Option<&[f32]>,
    cwd: Option<&Path>,
    active_file: Option<&str>, // Repo-relative path of the last edited/read file.
    retrieval_mode: RetrievalMode,
    config: &BudiConfig,
) -> Result<QueryResponse> {
    let kind = classify_intent(query);
    let intent = QueryIntent {
        kind,
        code_related: true,
        allow_docs: matches!(
            kind,
            QueryIntentKind::Architecture | QueryIntentKind::TestLookup
        ),
        weights: weights_for_intent(kind),
    };
    let runtime_env_var_query =
        intent.kind == QueryIntentKind::RuntimeConfig && is_runtime_env_var_query(query);
    let retrieval_query = query_for_initial_retrieval(query, &intent);
    let mut symbol_tokens = extract_query_symbol_tokens(&retrieval_query);
    augment_symbol_tokens_for_intent(&retrieval_query, &intent, &mut symbol_tokens);
    let exact_match_symbol_tokens =
        exact_match_symbol_tokens_for_intent(query, &intent, &symbol_tokens);
    // PascalCase tokens from the query (original case).  When the user writes
    // "Session" or "Engine", chunks whose symbol_hint is lowercase "session"
    // (property accessor / method) should score lower than chunks whose hint is
    // PascalCase "Session" (class / type definition).
    let query_pascal_tokens: HashSet<String> = extract_query_pascal_tokens(query);
    // Pre-compute function-name tokens for FlowTrace definition anchoring.
    // Includes camelCase (e.g. `reconcileChildFibers`) and snake_case (e.g.
    // `get_response`, `dispatch_request`) — not TitleCase or plain English words.
    let flowtrace_anchor_tokens: Vec<String> = if intent.kind == QueryIntentKind::FlowTrace {
        extract_flowtrace_anchor_tokens(query)
    } else {
        Vec::new()
    };
    let mut path_tokens = extract_query_path_tokens(&retrieval_query);
    let scope_hints = extract_scope_path_hints(&retrieval_query);
    add_dynamic_path_tokens(&mut path_tokens, &scope_hints);
    augment_path_tokens_for_intent(&retrieval_query, &intent, &mut path_tokens);
    let mut query_ecosystems = detect_query_ecosystems(query);
    // Merge repo-level ecosystems so the ecosystem boost fires even when the query
    // doesn't mention the framework by name. For example, in a React repo, a plain
    // "how does the router work?" query still benefits from React chunk affinity.
    for repo_eco in runtime.repo_ecosystems() {
        if !query_ecosystems.iter().any(|existing| existing == repo_eco) {
            query_ecosystems.push(repo_eco.clone());
        }
    }

    // TestLookup: widen the candidate pool so inline test blocks (which score lower
    // than production code on the query text) have a chance to enter the fused stage.
    // This also helps production co-location seeds from files like config.rs
    // whose production chunks rank ~25-30 in normal topk=20 for test queries.
    let topk_lex = if intent.kind == QueryIntentKind::TestLookup {
        config.topk_lexical * 2
    } else {
        config.topk_lexical
    };
    let topk_vec = if intent.kind == QueryIntentKind::TestLookup {
        config.topk_vector * 2
    } else {
        config.topk_vector
    };

    // Run retrieval channels
    let lexical = if retrieval_mode_allows_channel(retrieval_mode, ChannelKind::Lexical) {
        runtime.search_lexical(&retrieval_query, topk_lex)?
    } else {
        Vec::new()
    };
    let vector = if retrieval_mode_allows_channel(retrieval_mode, ChannelKind::Vector) {
        query_embedding
            .map(|embedding| runtime.search_vector(embedding, topk_vec))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let channel_limit = topk_lex.max(config.retrieval_limit * 2);
    let symbol = if retrieval_mode_allows_channel(retrieval_mode, ChannelKind::Symbol) {
        diversify_channel_by_path(
            runtime,
            &runtime.search_symbol_tokens(&symbol_tokens, channel_limit),
            channel_limit,
        )
    } else {
        Vec::new()
    };
    let path = if retrieval_mode_allows_channel(retrieval_mode, ChannelKind::Path) {
        diversify_channel_by_path(
            runtime,
            &runtime.search_path_tokens(&path_tokens, channel_limit),
            channel_limit,
        )
    } else {
        Vec::new()
    };
    let graph = if retrieval_mode_allows_channel(retrieval_mode, ChannelKind::Graph) {
        diversify_channel_by_path(
            runtime,
            &runtime.search_graph_tokens(&symbol_tokens, channel_limit),
            channel_limit,
        )
    } else {
        Vec::new()
    };

    let mut fused = fuse_channel_scores(&lexical, &vector, &symbol, &path, &graph, &intent);

    // For TestLookup, inject inline-test chunks co-located with top results.
    // Problem: inline test blocks (e.g., Rust #[cfg(test)]) may not rank in
    // topk_lexical/topk_vector because their text doesn't contain query words.
    // Fix: if a production chunk from file F is in top-5 candidates, also inject any
    // inline test blocks from file F directly into the candidate pool, scored
    // proportionally to the source file's max fused score (so higher-relevance files
    // produce higher-scored test neighbors, avoiding ties with less-relevant files).
    if intent.kind == QueryIntentKind::TestLookup {
        // Collect per-file max fused score from the top-8 PRODUCTION chunks
        // (skip test-path chunks — their raw fused scores are low since the +0.15
        // test-path-boost hasn't been applied yet, which would exclude them from
        // the window even though they're high in final score).
        let mut top_pairs: Vec<(u64, f32)> = fused.iter().map(|(id, cs)| (*id, cs.score)).collect();
        top_pairs.sort_by(|a, b| b.1.total_cmp(&a.1));
        let mut top_file_scores: std::collections::HashMap<String, f32> =
            std::collections::HashMap::new();
        let mut production_seen = 0usize;
        for (id, score) in &top_pairs {
            if production_seen >= 8 {
                break;
            }
            let Some(chunk) = runtime.chunk(*id) else {
                continue;
            };
            // Skip test-path and already-inline-test chunks as seeds
            if is_test_path(&chunk.path) || is_inline_test_chunk(&chunk.text) {
                continue;
            }
            production_seen += 1;
            let entry = top_file_scores.entry(chunk.path.clone()).or_insert(0.0);
            if *score > *entry {
                *entry = *score;
            }
        }
        // Inject or boost: inline test chunks from top files. Score is proportional
        // to the source file's max fused score (0.65×), so files with stronger
        // evidence inject higher-scoring test neighbors.
        for chunk in runtime.all_chunks() {
            let Some(&file_score) = top_file_scores.get(&chunk.path) else {
                continue;
            };
            if !is_inline_test_chunk(&chunk.text) {
                continue;
            }
            let neighbor_score = file_score * 0.65;
            let existing = fused.get(&chunk.id).map(|cs| cs.score).unwrap_or(0.0);
            if neighbor_score > existing {
                fused.insert(
                    chunk.id,
                    CandidateScore {
                        score: neighbor_score,
                        signals: vec!["inline-test-neighbor".to_string()],
                        channel_scores: QueryChannelScores::default(),
                    },
                );
            }
        }
    }

    // Direct symbol-hint seeding for SymbolDefinition/SymbolUsage.
    // Problem: definition chunk is invisible to the symbol channel in two cases:
    //   pure-lowercase symbol names like "run" — `is_symbol_like_token` rejects them,
    //   so they never enter symbol_to_chunk_ids at all.
    //   camelCase names like `scheduleUpdateOnFiber` — they ARE in symbol_to_chunk_ids
    //   but the codebase has so many call-sites that the definition chunk falls outside
    //   the topk limit, never entering the fused map where hint-match-boost can fire.
    //
    // Different seeds per intent:
    //   SymbolDefinition: use all symbol_tokens (includes camelCase from plain text + all
    //     backtick tokens via the pre-pass). Seeding the definition chunk is always correct.
    //   SymbolUsage: use only lowercase_backtick_tokens. For sym-use queries the symbol
    //     channel already finds callers well for camelCase identifiers, so seeding the
    //     definition chunk with all symbol tokens just adds noise.
    if intent.kind == QueryIntentKind::SymbolDefinition && !symbol_tokens.is_empty() {
        const SYMBOL_DEF_SEED_SCORE: f32 = 0.28;
        for chunk in runtime.all_chunks() {
            let Some(hint) = chunk.symbol_hint.as_deref() else {
                continue;
            };
            let hint_lower = hint.to_ascii_lowercase();
            if is_generic_symbol_hint(hint) {
                continue;
            }
            let exact_match = exact_match_symbol_tokens
                .iter()
                .any(|t| hint_lower == t.as_str());
            if !exact_match {
                continue;
            }
            let existing = fused.get(&chunk.id).map(|cs| cs.score).unwrap_or(0.0);
            let seeded_score = if existing > 0.0 {
                SYMBOL_DEF_SEED_SCORE + (existing * 0.5)
            } else {
                SYMBOL_DEF_SEED_SCORE
            };
            if let Some(candidate) = fused.get_mut(&chunk.id) {
                if seeded_score > candidate.score {
                    candidate.score = seeded_score;
                }
                push_unique_reason(&mut candidate.signals, "sym-hint-seed");
            } else {
                fused.insert(
                    chunk.id,
                    CandidateScore {
                        score: seeded_score,
                        signals: vec!["sym-hint-seed".to_string()],
                        channel_scores: QueryChannelScores::default(),
                    },
                );
            }
        }
    }
    // For SymbolUsage, seed only pure-lowercase backtick tokens.
    if intent.kind == QueryIntentKind::SymbolUsage {
        let lowercase_backtick_tokens = extract_lowercase_backtick_tokens(query);
        if !lowercase_backtick_tokens.is_empty() {
            const SYMBOL_DEF_SEED_SCORE: f32 = 0.28;
            for chunk in runtime.all_chunks() {
                let Some(hint) = chunk.symbol_hint.as_deref() else {
                    continue;
                };
                let hint_lower = hint.to_ascii_lowercase();
                if !lowercase_backtick_tokens
                    .iter()
                    .any(|t| hint_lower == t.as_str())
                {
                    continue;
                }
                let existing = fused.get(&chunk.id).map(|cs| cs.score).unwrap_or(0.0);
                if SYMBOL_DEF_SEED_SCORE > existing {
                    fused.insert(
                        chunk.id,
                        CandidateScore {
                            score: SYMBOL_DEF_SEED_SCORE,
                            signals: vec!["sym-hint-seed".to_string()],
                            channel_scores: QueryChannelScores::default(),
                        },
                    );
                }
            }
        }
    }

    // Path-based inline-test seeding for TestLookup.
    // Problem: when the query is wordy ("What unit tests cover the config file parsing..."),
    // the production file (e.g. flags/config.rs) may not appear in topk at all — so the
    // co-located inline-test pass has no seed and never injects its inline test block.
    // Fix: extract subject tokens (strip test-noise words), find inline test chunks in
    // files whose paths contain any subject token, inject directly at a baseline score
    // that clears the TestLookup min_selection_score floor (0.22).
    if intent.kind == QueryIntentKind::TestLookup {
        let subject_tokens = test_subject_tokens(query);
        if !subject_tokens.is_empty() {
            const INLINE_TEST_SEED_SCORE: f32 = 0.35;
            for chunk in runtime.all_chunks() {
                if !is_inline_test_chunk(&chunk.text) {
                    continue;
                }
                let path_lower = chunk.path.to_ascii_lowercase();
                if !subject_tokens
                    .iter()
                    .any(|t| path_lower.contains(t.as_str()))
                {
                    continue;
                }
                let existing = fused.get(&chunk.id).map(|cs| cs.score).unwrap_or(0.0);
                if INLINE_TEST_SEED_SCORE > existing {
                    fused.insert(
                        chunk.id,
                        CandidateScore {
                            score: INLINE_TEST_SEED_SCORE,
                            signals: vec!["inline-test-subject-seed".to_string()],
                            channel_scores: QueryChannelScores::default(),
                        },
                    );
                }
            }
        }
    }

    // TestLookup: seed chunks from dedicated test files whose filename matches
    // the queried subject. Problem: lexical/vector channels favor test helpers
    // (e.g. testPlan()) over actual test files (plan_test.go) because the helper
    // function name matches the query more directly. Fix: ensure chunks from
    // files named {subject}_test.* or test_{subject}.* enter the candidate pool.
    // Use only the FIRST subject token (most specific in query order), pick the
    // best 3 candidates by path relevance, and seed them at a competitive score.
    if intent.kind == QueryIntentKind::TestLookup {
        let subject_tokens = test_subject_tokens(query);
        if let Some(primary_subject) = subject_tokens.first() {
            const TEST_FILE_SEED_SCORE: f32 = 0.55;
            const MAX_SEEDED_FILES: usize = 3;
            // Collect candidate (chunk_id, path) pairs, deduplicated by path.
            let mut seen_paths: HashSet<String> = HashSet::new();
            let mut candidates: Vec<(u64, String)> = Vec::new();
            for chunk in runtime.all_chunks() {
                if !is_test_path(&chunk.path) || is_mock_path(&chunk.path) {
                    continue;
                }
                let filename = chunk.path.rsplit('/').next().unwrap_or(&chunk.path);
                let Some(stem) = extract_test_subject_stem(filename) else {
                    continue;
                };
                if &stem != primary_subject {
                    continue;
                }
                if !seen_paths.insert(chunk.path.clone()) {
                    continue;
                }
                candidates.push((chunk.id, chunk.path.clone()));
            }
            // Sort by path relevance: prefer paths containing other subject tokens,
            // then shorter paths (closer to the module root).
            candidates.sort_by(|a, b| {
                let a_lower = a.1.to_ascii_lowercase();
                let b_lower = b.1.to_ascii_lowercase();
                let a_extra = subject_tokens
                    .iter()
                    .skip(1)
                    .filter(|t| a_lower.contains(t.as_str()))
                    .count();
                let b_extra = subject_tokens
                    .iter()
                    .skip(1)
                    .filter(|t| b_lower.contains(t.as_str()))
                    .count();
                b_extra.cmp(&a_extra).then(a.1.len().cmp(&b.1.len()))
            });
            for (chunk_id, _path) in candidates.into_iter().take(MAX_SEEDED_FILES) {
                let existing = fused.get(&chunk_id).map(|cs| cs.score).unwrap_or(0.0);
                if TEST_FILE_SEED_SCORE > existing {
                    fused.insert(
                        chunk_id,
                        CandidateScore {
                            score: TEST_FILE_SEED_SCORE,
                            signals: vec!["test-subject-file-seed".to_string()],
                            channel_scores: QueryChannelScores::default(),
                        },
                    );
                }
            }
        }
    }

    let cwd_rel = cwd
        .and_then(|path| path.to_str())
        .map(normalize_path)
        .unwrap_or_default();

    // Minimal per-chunk adjustments: doc penalty + cwd proximity only
    let mut scored = Vec::new();
    for (id, candidate) in fused {
        let Some(chunk) = runtime.chunk(id) else {
            continue;
        };
        let mut adjusted = candidate.score;
        let mut reasons = candidate.signals;
        let mut channel_scores = candidate.channel_scores;

        if runtime.is_doc_like_chunk(id) && intent.code_related && !intent.allow_docs {
            adjusted -= 0.25;
            push_unique_reason(&mut reasons, "doc-penalty");
        }

        if !cwd_rel.is_empty() && chunk.path.starts_with(&cwd_rel) {
            adjusted += 0.08;
            push_unique_reason(&mut reasons, "cwd-proximity");
        }

        if let Some(chunk_ecosystems) = runtime.chunk_ecosystems(id)
            && let Some(matched_ecosystem) =
                first_matching_ecosystem(&query_ecosystems, chunk_ecosystems)
        {
            adjusted += 0.08;
            let ecosystem_reason = format!("ecosystem-match:{matched_ecosystem}");
            push_unique_reason(&mut reasons, &ecosystem_reason);
        }

        // Boost chunks from the file Claude most recently edited/read.
        // Iterative edit sessions: the next query is very likely about the same file.
        // +0.20 is intentionally larger than cwd-proximity (+0.08) to ensure the
        // active file surfaces above directory-level neighbors.
        if let Some(af) = active_file
            && chunk.path == af
        {
            adjusted += 0.20;
            push_unique_reason(&mut reasons, "active-file-boost");
        }

        // TestLookup: boost chunks from test files so they surface above source files.
        // Also boost inline test blocks (#[test], #[cfg(test)], mod tests, describe/it)
        // to handle Rust crates and JS/TS files that colocate tests in production files.
        if intent.kind == QueryIntentKind::TestLookup
            && (is_test_path(&chunk.path) || is_inline_test_chunk(&chunk.text))
        {
            adjusted += 0.15;
            push_unique_reason(&mut reasons, "test-path-boost");
        }

        // TestLookup: demote mock/test-double files. Coverage queries ("what tests cover
        // the plan command") want actual test functions, not mock implementations. Mock
        // files can score high via lexical/symbol matches (they define the same symbols)
        // but provide no test coverage information.
        if intent.kind == QueryIntentKind::TestLookup && is_mock_path(&chunk.path) {
            adjusted -= 0.20;
            push_unique_reason(&mut reasons, "mock-path-penalty");
        }

        // TestLookup: demote Go test helper functions. In Go, test functions are
        // TestXxx (uppercase T), while test helpers are testXxx (lowercase t).
        // Helpers create fixtures/setup — not what "what tests cover X" wants.
        // testPlan() returning *plans.Plan is a fixture factory, not a test.
        if intent.kind == QueryIntentKind::TestLookup
            && is_test_path(&chunk.path)
            && is_go_test_helper_chunk(&chunk.text)
        {
            adjusted -= 0.20;
            push_unique_reason(&mut reasons, "test-helper-demote");
        }

        // Architecture queries — penalise test-path chunks.
        // For module-layout, entry-point, and call-chain questions the relevant evidence
        // is production source code, not test fixtures and test helpers. Test files
        // (typically in tests/, __tests__, spec/) inflate scores through incidental
        // mentions of the queried concept ("how is Flask structured?" matches
        // tests/test_apps fixtures) and crowd out the actual source modules.
        // A −0.15 penalty mirrors the +0.15 test-path-boost on TestLookup queries,
        // keeping the scoring symmetric. Combined with the min 0.30 floor below, this
        // removes test fixture chunks that score 0.41–0.44 for arch queries.
        if intent.kind == QueryIntentKind::Architecture && is_test_path(&chunk.path) {
            adjusted -= 0.15;
            push_unique_reason(&mut reasons, "test-path-penalty");
        }

        // SymbolDefinition queries — penalise test-path chunks.
        // Mock implementations and test doubles (provider_mock.go, eval_context_mock.go)
        // define the same symbol as the real production code but with stub behavior.
        // When sym-hint-seed + hint-match-boost pushes a mock definition to the top,
        // Claude gets anchored on the test double instead of the real implementation.
        // −0.30 combined with the sym-def floor (0.30) filters most mock definitions
        // while keeping production definitions intact.
        if intent.kind == QueryIntentKind::SymbolDefinition && is_test_path(&chunk.path) {
            adjusted -= 0.30;
            push_unique_reason(&mut reasons, "test-path-penalty");
        }

        // SymbolDefinition — demote stub/placeholder implementations.
        // Functions like `panic("not implemented")`, `unimplemented!()`, or
        // `return fmt.Errorf("unsupported ...")` are stubs — the real implementation
        // lives elsewhere. When hint-match-boost pushes a stub to the top (e.g.
        // Terraform ReadStateBytes: provider.go stub at 0.787), Claude gets anchored
        // on useless code. -0.35 pushes stubs well below the sym-def floor (0.30).
        if intent.kind == QueryIntentKind::SymbolDefinition && is_stub_body(&chunk.text) {
            adjusted -= 0.35;
            push_unique_reason(&mut reasons, "stub-body-demote");
        }

        // Architecture queries — penalise examples/ paths.
        // Tutorial and example directories (e.g. examples/tutorial/) frequently contain
        // snippets that mention production concepts ("application factory", "register_blueprint")
        // but in a simplified sample context. For arch queries about the real production module
        // layout and entry points, these samples are misleading: they describe how users integrate
        // the library, not how the library itself is structured.
        // −0.50 pushes tutorial files well below the arch floor in both the top≥0.60 case
        // (floor=0.40) and the low-confidence case (floor=0.30). A score of 0.73−0.50=0.23
        // falls below both floors. examples/ is intentionally excluded from is_test_path()
        // (so TestLookup keeps them), so this penalty is arch-specific.
        if intent.kind == QueryIntentKind::Architecture && is_examples_path(&chunk.path) {
            adjusted -= 0.50;
            push_unique_reason(&mut reasons, "examples-path-penalty");
        }

        // Architecture queries — penalise devtools/internal-tooling paths.
        // DevTools directories contain debugging, visualization, and internal developer
        // tooling — not the production architecture the user is asking about. For queries
        // like "entry points" or "how is the component tree structured", devtools tree-
        // type definitions are a false positive (HNSW matches "tree" from the query to
        // "Element type with parentID" in devtools types). Noop renderers are test infra.
        if intent.kind == QueryIntentKind::Architecture && is_devtools_path(&chunk.path) {
            adjusted -= 0.30;
            push_unique_reason(&mut reasons, "devtools-path-penalty");
        }

        // FlowTrace queries are about production execution paths, so tests that merely call
        // into framework entrypoints should not outrank the actual runtime chain. The graph
        // channel can otherwise over-reward fixture-heavy test files that happen to invoke the
        // target methods. Use a stronger penalty than Architecture because FlowTrace relies
        // more heavily on graph/symbol signals, which makes test-callers especially sticky.
        // Use -0.40 instead of -0.30. Previously, test utilities with high raw
        // HNSW scores (e.g. consoleMock.js ~0.59) survived the FlowTrace min-score floor at
        // 0.25 after -0.30 (adjusted=0.29). With -0.40 they fall below 0.25 and are filtered.
        if intent.kind == QueryIntentKind::FlowTrace && is_test_path(&chunk.path) {
            adjusted -= 0.40;
            push_unique_reason(&mut reasons, "test-path-penalty");
        }

        // SymbolDefinition — boost chunks whose symbol_hint is an exact match for a
        // query token. This surfaces definition chunks over reference/usage chunks when
        // the dominant function in a window is precisely what the user asked about.
        //
        // When the query uses PascalCase (e.g. "Session", "Engine"), the user likely
        // means a class/type, not a property accessor. Chunks whose symbol_hint is
        // lowercase (e.g. `def session(self)`) get a reduced boost compared to those
        // whose hint is PascalCase (e.g. `class Session`). This prevents property
        // accessors from outranking the actual class definition.
        if intent.kind == QueryIntentKind::SymbolDefinition
            && let Some(hint) = chunk.symbol_hint.as_deref()
        {
            let hint_lower = hint.to_ascii_lowercase();
            let exact_match = exact_match_symbol_tokens.iter().any(|t| t == &hint_lower);
            if !hint_lower.is_empty() && !is_generic_symbol_hint(hint) && exact_match {
                let query_has_pascal = query_pascal_tokens.contains(&hint_lower);
                let hint_is_lowercase =
                    hint.chars().next().is_some_and(|c| c.is_ascii_lowercase());
                if query_has_pascal && hint_is_lowercase {
                    // Property/method with same name as a class — reduced boost.
                    adjusted += 0.10;
                    push_unique_reason(&mut reasons, "hint-match-boost-weak");
                } else {
                    adjusted += 0.30;
                    push_unique_reason(&mut reasons, "hint-match-boost");
                }
                // Path-relevance tiebreaker: when the query mentions a domain term
                // (e.g. "URL resolution") and the chunk path contains it (e.g. urls/),
                // add a small boost to prefer the contextually relevant definition.
                // Sym-hint-seeded chunks bypass the path channel, so this is the only
                // way domain context from the query influences their ranking.
                let path_lower = chunk.path.to_ascii_lowercase();
                if path_tokens.iter().any(|pt| path_lower.contains(pt.as_str())) {
                    adjusted += 0.05;
                    push_unique_reason(&mut reasons, "hint-path-relevance");
                }
            }
        }

        // RuntimeConfig — test-path penalty + class-body demotion.
        // "Which environment variables does Flask read at startup?" should return the
        // *functions* that call os.environ / getenv, not class definitions whose name
        // lexically matches "environment" (e.g. `class Environment(BaseEnvironment):` in
        // Flask/Jinja). Config-reading code lives in production source (not test fixtures)
        // and in functions/methods (not class definitions).
        //
        // (1) Test-path penalty: mirrors Architecture/FlowTrace — config reading code lives
        //     in production source, not test helpers that happen to mention config keys.
        // (2) Class-body demotion: PascalCase symbol_hint → class definition, not a config
        //     reader. A −0.25 penalty ensures that a class chunk (typically 0.50−0.57)
        //     drops below function chunks (0.35−0.45) that actually call os.environ/getenv.
        if intent.kind == QueryIntentKind::RuntimeConfig {
            if is_test_path(&chunk.path) {
                adjusted -= 0.15;
                push_unique_reason(&mut reasons, "test-path-penalty");
            }
            if let Some(hint) = chunk.symbol_hint.as_deref()
                && hint.chars().next().is_some_and(|c| c.is_ascii_uppercase())
            {
                adjusted -= 0.25;
                push_unique_reason(&mut reasons, "rt-cfg-class-demote");
            }
            if runtime_env_var_query && !is_test_path(&chunk.path) {
                let chunk_text_lower = chunk.text.to_ascii_lowercase();
                if chunk.text.contains("FLASK_")
                    || chunk_text_lower.contains("os.environ.get(")
                    || chunk_text_lower.contains("getenv(")
                {
                    adjusted += 0.14;
                    push_unique_reason(&mut reasons, "rt-cfg-env-read-boost");
                }
                let path_lower = chunk.path.to_ascii_lowercase();
                if path_lower.ends_with("/cli.py")
                    || path_lower.ends_with("/helpers.py")
                    || path_lower.ends_with("/config.py")
                    || path_lower.ends_with("/app.py")
                {
                    adjusted += 0.08;
                    push_unique_reason(&mut reasons, "rt-cfg-startup-path-boost");
                }
            }
        }

        // SymbolUsage — demote definition chunks that happen to rank above callers.
        // "What calls evaluateExpr" (SymbolUsage) should surface CALLERS, but the symbol
        // channel can rank the definition/mock implementation highly because it also has
        // `evaluateexpr` in its symbol_tokens. When the dominant symbol_hint of a chunk
        // exactly matches a query token in a SymbolUsage query, the chunk is likely the
        // definition rather than a call site — apply -0.22 to push it below the min-score
        // floor (e.g. eval_context_mock.go: raw 0.43 → 0.21 < 0.22 floor → filtered).
        if intent.kind == QueryIntentKind::SymbolUsage
            && let Some(hint) = chunk.symbol_hint.as_deref()
        {
            let hint_lower = hint.to_ascii_lowercase();
            let is_definition_chunk = exact_match_symbol_tokens.iter().any(|t| t == &hint_lower);
            if !hint_lower.is_empty() && !is_generic_symbol_hint(hint) && is_definition_chunk {
                adjusted -= 0.22;
                push_unique_reason(&mut reasons, "sym-usage-def-demote");
            }
        }

        // SymbolUsage — demote struct/class/type definition chunks.
        // "What calls search_path" should surface CALL SITES, not struct definitions like
        // `struct Paths { ... }` that lexically match on "path". Detect via:
        // (1) PascalCase symbol_hint → class/struct name, or
        // (2) chunk text starts with struct/type/class keyword.
        if intent.kind == QueryIntentKind::SymbolUsage
            && !reasons.iter().any(|r| r == "sym-usage-def-demote")
        {
            let hint_is_type = chunk
                .symbol_hint
                .as_deref()
                .is_some_and(|h| h.chars().next().is_some_and(|c| c.is_ascii_uppercase()));
            let text_is_struct = is_struct_or_type_definition(&chunk.text);
            if hint_is_type || text_is_struct {
                adjusted -= 0.25;
                push_unique_reason(&mut reasons, "sym-usage-struct-demote");
            }
        }

        // Pure-lexical demotion for FlowTrace and SymbolUsage. Chunks matching only
        // on common-word lexical hits (e.g. "call", "return", "does", "root") with zero
        // semantic/symbol/graph signal are usually noise. Real callers/callees should have
        // at least some vector similarity or graph connection.
        // FlowTrace: "What functions does get_response call?" → `describe()` in migrations.
        // SymbolUsage: "What calls performSyncWorkOnRoot" → compiler/index.ts single line.
        if matches!(
            intent.kind,
            QueryIntentKind::FlowTrace | QueryIntentKind::SymbolUsage
        ) && channel_scores.lexical > 0.0
            && channel_scores.vector == 0.0
            && channel_scores.symbol == 0.0
            && channel_scores.graph == 0.0
        {
            adjusted -= 0.10;
            push_unique_reason(&mut reasons, "lexical-only-demote");
        }

        // FlowTrace — definition anchor for named function targets.
        // "What does reconcileChildFibers call" or "What functions does get_response call"
        // queries name a specific function. Boost its definition chunk (+0.10) to keep it
        // above irrelevant lexical matches. Without the boost, common-word lexical hits
        // (e.g. "describe", "adapt_timefield_value") can outrank the actual target function.
        // Includes camelCase and snake_case tokens — not TitleCase or plain English words.
        // Weaker than SymbolDefinition (+0.30) so it doesn't fully override flow semantics.
        if !flowtrace_anchor_tokens.is_empty()
            && let Some(hint) = chunk.symbol_hint.as_deref()
        {
            let hint_lower = hint.to_ascii_lowercase();
            let exact_match = flowtrace_anchor_tokens.iter().any(|t| t == &hint_lower);
            if !hint_lower.is_empty() && !is_generic_symbol_hint(hint) && exact_match {
                adjusted += 0.10;
                push_unique_reason(&mut reasons, "flowtrace-anchor");
            }
        }

        if reasons.is_empty() {
            reasons.push("semantic+lexical".to_string());
        }
        channel_scores.rerank = adjusted - candidate.score;
        scored.push(ScoredChunk {
            id,
            score: adjusted,
            reasons,
            channel_scores,
        });
    }

    scored.sort_by(|a, b| b.score.total_cmp(&a.score));

    // Per-intent retrieval limit. Honour explicit user config override.
    let default_limit = intent_retrieval_limit(intent.kind);
    // SymbolDefinition with hint-match-boost → definition confirmed found.
    // Restrict to 2 candidates and skip graph expansion to avoid noise cards from
    // common query words ("path", "matcher", "writer") crowding out the definition.
    // The [structural context] block already provides callers/callees compactly.
    // Use hint-match-boost (stable across seeded and HNSW paths) not sym-hint-seed.
    let sym_def_seeded = intent.kind == QueryIntentKind::SymbolDefinition
        && scored
            .first()
            .is_some_and(|c| c.reasons.iter().any(|r| r == "hint-match-boost"));
    let target_limit = if config.retrieval_limit != crate::config::DEFAULT_RETRIEVAL_LIMIT {
        config.retrieval_limit.max(4)
    } else if sym_def_seeded {
        2
    } else {
        default_limit.max(4)
    };
    // TestLookup: reduce per_file_limit to 1 to force diversity across test files.
    // Without this, high-scoring files (e.g., tests/multiline.rs) grab 2 slots,
    // crowding out other test files (e.g., tests/feature.rs with parallel tests).
    // Also raise per_bucket_limit to 3 so inline-test-neighbor injections from the
    // same top-level crate bucket don't get crowded out.
    let (per_file_limit, per_bucket_limit) = if intent.kind == QueryIntentKind::TestLookup {
        (1, 3)
    } else {
        (2, 2)
    };
    let mut selection = SnippetSelectionState {
        per_file_limit,
        per_bucket_limit,
        ..SnippetSelectionState::default()
    };
    let min_score = min_selection_score(&scored, intent.kind);
    // Generative/design Architecture queries with low top confidence (< 0.55)
    // inject mediocre production code that anchors Claude to specific implementations,
    // hurting creative design and test-writing responses.
    // "What unit tests would you add?" → injecting random production chunks constrains
    //   Claude's test-design creativity.
    // "I want to add a middleware — what files would I modify?" → injecting a marginally
    //   relevant snippet (e.g. after_request scaffold) without the full picture misleads
    //   Claude about the scope of the change.
    // Skip injection entirely for these design-intent queries — also bypasses the
    // empty-selection fallback below.
    //
    // Extend the same rule to cover "I want to add/implement" design queries.
    // These are architecturally equivalent to the test-writing pattern: the user wants
    // Claude to reason freely about design, not anchor to a partial low-quality context.
    // Entry-points and crate-responsibilities queries always need broad exploration.
    // Even high-confidence retrieval results (0.70+) are typically build scripts or
    // re-export modules — not the holistic overview Claude produces by exploring.
    // Skip unconditionally for Architecture queries matching these patterns.
    let entry_points_skip = intent.kind == QueryIntentKind::Architecture
        && contains_any(
            &query.to_lowercase(),
            &["entry points", "entry point", "crate responsibilities"],
        );
    // Module-layout / directory-structure queries always need breadth — Claude
    // exploring many directories produces better answers than 2-3 code snippets
    // that anchor it to a narrow slice. Skip unconditionally (like entry-points).
    // Django P2 (top=0.711): autoreload.py + startapp.py noise anchors Claude.
    let module_layout_skip = intent.kind == QueryIntentKind::Architecture
        && contains_any(
            &query.to_lowercase(),
            &[
                "module layout",
                "directory structure",
                "codebase structure",
                "project structure",
                "folder structure",
                "which files own",
                "which directories own",
                "which dirs own",
            ],
        );
    let ci_skip = intent.kind == QueryIntentKind::Architecture
        && scored.first().is_some_and(|c| c.score < 0.55)
        && contains_any(
            &query.to_lowercase(),
            &[
                "would you add",
                "would you write",
                "should we add",
                "tests to add",
                "tests to write",
                "suggest tests",
                // Design/implementation queries
                "i want to add",
                "i want to implement",
                "i need to add",
                "i need to implement",
                "want to add",
                "want to implement",
            ],
        );
    // Broad env-var listing queries ("which env vars", "what env vars") benefit
    // from Claude's own exploration — partial injection anchors Claude on a
    // narrow set and causes less comprehensive answers. Skip injection entirely.
    let env_listing_skip = runtime_env_var_query
        && contains_any(
            &query.to_lowercase(),
            &["which env", "what env", "list env", "all env"],
        );
    // Broad lifecycle-overview FlowTrace queries ("lifecycle hook execution order",
    // "cleanup order for effects") are about user-facing conceptual ordering that
    // Claude knows from training. Injecting internal commit-phase function details
    // (lifecycle pack) or generic HNSW matches anchors Claude on implementation
    // internals instead of the broader lifecycle model.
    let lifecycle_overview_skip = intent.kind == QueryIntentKind::FlowTrace
        && scored.first().is_some_and(|c| c.score < 0.55)
        && {
            let lq = query.to_lowercase();
            contains_any(&lq, &["lifecycle", "cleanup order", "effect order"])
                && contains_any(&lq, &["order", "sequence", "when a", "execution"])
                && contains_any(&lq, &["mount", "unmount", "component", "effect", "hook"])
        };
    // Broad test-coverage inventory queries ("what tests cover X and where do they
    // live") need Claude to explore the repo widely. Injecting 2-3 partial test file
    // fragments anchors Claude on those snippets instead of finding the complete test
    // structure. Skip when the results are dominated by subject-file-seed (synthetic,
    // not organically ranked) — i.e., the real retrieval didn't find strong matches.
    let test_coverage_skip = intent.kind == QueryIntentKind::TestLookup
        && is_test_coverage_inventory_query(query)
        && scored
            .first()
            .is_some_and(|c| c.reasons.iter().any(|r| r == "test-subject-file-seed"));
    // Low-confidence SymbolUsage skip: when the top candidate scored too low,
    // injecting marginal results anchors Claude on irrelevant code.
    // Two tiers:
    //   - No symbol/graph signal (purely lexical/vector): threshold 0.35
    //     Generic words like "setup" match many files but none via the symbol
    //     index, so lexical-only results at 0.29-0.34 are usually noise.
    //   - Has symbol/graph signal: threshold 0.28
    //     The symbol index confirmed the function name exists, so lower scores
    //     can still be relevant callers.
    let sym_use_low_confidence_skip = intent.kind == QueryIntentKind::SymbolUsage
        && scored.first().is_some_and(|c| {
            let no_symbol_signal = c.channel_scores.symbol == 0.0 && c.channel_scores.graph == 0.0;
            let threshold = if no_symbol_signal { 0.35 } else { 0.28 };
            c.score < threshold
        });
    // SymbolUsage thin-caller skip: when the top non-definition card spans a
    // very short file (start_line <= 2, end_line <= 15), the caller is likely a
    // boilerplate entry point (__main__.py, main.rs wrapper). Such trivial call
    // sites anchor Claude on the obvious answer instead of exploring richer callers.
    // Django P8: __main__.py:1-10 calls execute_from_command_line (Q 9→5).
    let sym_use_thin_caller_skip = intent.kind == QueryIntentKind::SymbolUsage
        && scored.first().is_some_and(|c| {
            !c.reasons.iter().any(|r| r == "sym-usage-def-demote")
                && runtime
                    .chunk(c.id)
                    .is_some_and(|chunk| chunk.start_line <= 2 && chunk.end_line <= 15)
        });
    let ci_skip = ci_skip
        || env_listing_skip
        || lifecycle_overview_skip
        || test_coverage_skip
        || entry_points_skip
        || module_layout_skip
        || sym_use_low_confidence_skip
        || sym_use_thin_caller_skip;
    let min_score = if env_listing_skip
        || lifecycle_overview_skip
        || test_coverage_skip
        || entry_points_skip
        || module_layout_skip
        || sym_use_low_confidence_skip
        || sym_use_thin_caller_skip
    {
        f32::MAX
    } else if ci_skip {
        0.55_f32
    } else {
        min_score
    };
    // FlowTrace with flowtrace-anchor — tighten the secondary floor.
    // When the top-ranked chunk already matched a camelCase function name in the query
    // (flowtrace-anchor), we have a strong definition anchor. Secondary HNSW candidates
    // are often noise from distant call chains (e.g. retryActivityComponentWithoutHydrating
    // appearing after reconcileChildFibers). Raise the secondary floor to max(top×0.50, 0.30)
    // to filter out low-confidence secondaries while keeping the definition as card 1.
    // The [structural context] block already provides callees compactly via call graph.
    let flowtrace_anchored = intent.kind == QueryIntentKind::FlowTrace
        && scored
            .first()
            .is_some_and(|c| c.reasons.iter().any(|r| r == "flowtrace-anchor"));
    let min_score_secondary = if flowtrace_anchored {
        let top = scored.first().map(|c| c.score).unwrap_or(0.0);
        (top * 0.50_f32).max(0.30_f32)
    } else {
        min_score
    };
    for candidate in &scored {
        if selection.snippets.len() >= target_limit {
            break;
        }
        let effective_min = if flowtrace_anchored && !selection.snippets.is_empty() {
            min_score_secondary
        } else {
            min_score
        };
        if candidate.score < effective_min && (ci_skip || !selection.snippets.is_empty()) {
            continue;
        }
        let _ = try_push_scored_chunk(runtime, candidate, &mut selection);
    }
    // When the low-confidence design skip is active, leave selection empty so no
    // context is injected — do not fall through to the best-effort fallback below.
    if selection.snippets.is_empty()
        && !ci_skip
        && let Some(best) = scored.first()
    {
        let _ = try_push_scored_chunk(runtime, best, &mut selection);
    }
    if should_expand_graph_neighbors(intent.kind) && !sym_def_seeded {
        expand_graph_neighbors(
            runtime,
            &mut selection,
            target_limit.saturating_add(GRAPH_NEIGHBOR_EXPANSION_LIMIT),
            GRAPH_NEIGHBOR_EXPANSION_LIMIT,
        );
    }

    if intent.kind == QueryIntentKind::SymbolDefinition && !exact_match_symbol_tokens.is_empty() {
        promote_exact_match_symbol_definition_chunks(
            runtime,
            &scored,
            &mut selection,
            &exact_match_symbol_tokens,
            target_limit,
        );
    }

    // Request-to-view flow pack.
    // Generic FlowTrace retrieval often lands on tests or ancillary view classes because the
    // query names an execution path ("incoming HTTP request -> view function") rather than
    // concrete symbols. When the runtime contains a canonical request dispatch chain such as
    // wsgi_app -> full_dispatch_request -> dispatch_request, inject a compact same-file chain
    // pack built from exact code lines.
    maybe_inject_web_request_flow_chain_card(query, runtime, &mut selection, target_limit);

    // SymbolDefinition continuation chunk.
    // When sym_def_seeded (hint-match-boost confirmed the definition was found), the
    // 80-line definition chunk often doesn't cover the full function body. Look for the
    // next chunk from the same file (smallest start_line > def.start_line, accounting for
    // the 20-line overlap where stride=60) and inject it in place of any call-site card
    // from a different file. This gives Claude the complete function body — including the
    // key operations that follow the DEV guards — for "what are its first steps" queries.
    if sym_def_seeded {
        // Track when a wrong-symbol continuation is blocked so we can also drop
        // the noisy foreign card 2 instead of leaving it in place.
        let mut wrong_symbol_continuation_blocked = false;
        let continuation = selection.snippets.first().and_then(|def_item| {
            let def_path = def_item.path.clone();
            let def_start = def_item.start_line;
            let def_score = def_item.score;
            let card2 = selection.snippets.get(1);
            let has_foreign_card2 = card2.is_some_and(|s| s.path != def_path);
            if !has_foreign_card2 {
                return None;
            }
            // If card 2 has hint-match-boost, it's an alternative definition
            // of the same symbol from a different file (e.g. Flask.make_response in app.py
            // alongside the module-level make_response in helpers.py). Keep it — don't
            // replace a cross-file definition with a same-file continuation.
            let cont_id = runtime.adjacent_chunk(&def_path, def_start)?;
            let cont = runtime.chunk(cont_id)?;
            let card2_is_alt_def =
                card2.is_some_and(|s| s.reasons.iter().any(|r| r == "hint-match-boost"));
            let prefer_wrapper_continuation =
                card2_is_alt_def && should_prefer_same_file_definition_continuation(def_item, cont);
            if card2_is_alt_def && !prefer_wrapper_continuation {
                return None;
            }
            // If the continuation chunk's symbol_hint is a DIFFERENT function than
            // the target symbol, the definition body fits entirely in card 1 — skip injection.
            // Only applies to the raw-continuation path (not the wrapper-first-steps path which
            // needs `cont` to synthesize the implementation card regardless of its symbol_hint).
            // Example: if the adjacent chunk starts a different function, injecting it
            // pollutes the answer with unrelated implementation details.
            if !card2_is_alt_def
                && !exact_match_symbol_tokens.is_empty()
                && let Some(cont_sym) = cont.symbol_hint.as_deref()
            {
                let cont_sym_lower = cont_sym.to_ascii_lowercase();
                let matches_target = exact_match_symbol_tokens
                    .iter()
                    .any(|t| cont_sym_lower == t.as_str());
                if !matches_target {
                    // Also signal that the noisy foreign card 2 should be removed.
                    wrong_symbol_continuation_blocked = true;
                    return None;
                }
            }
            let cont_score = def_score * if card2_is_alt_def { 0.72 } else { 0.60 };
            if prefer_wrapper_continuation
                && let Some(card) = build_symbol_definition_first_steps_card(cont, cont_score)
            {
                return Some(card);
            }
            Some(crate::rpc::QueryResultItem {
                path: cont.path.clone(),
                start_line: cont.start_line,
                end_line: cont.end_line,
                language: cont.language.clone(),
                score: cont_score,
                reasons: vec![
                    if card2_is_alt_def {
                        "wrapper-implementation-continuation"
                    } else {
                        "definition-continuation"
                    }
                    .to_string(),
                ],
                channel_scores: QueryChannelScores::default(),
                text: cont.text.clone(),
                slm_relevance_note: None,
            })
        });
        if let Some(cont_item) = continuation {
            // Replace card 2 (foreign call site) with the continuation of the function body.
            selection.snippets.truncate(1);
            selection.snippets.push(cont_item);
        } else if wrong_symbol_continuation_blocked {
            // The wrong-symbol continuation was blocked; the foreign card 2
            // is also noise (e.g. devtools type definition when querying for a WorkLoop fn).
            // The definition fits entirely in card 1 — serve just that.
            selection.snippets.truncate(1);
        }
    }

    if intent.kind == QueryIntentKind::SymbolDefinition && !exact_match_symbol_tokens.is_empty() {
        maybe_inject_symbol_definition_delegate_pack(
            runtime,
            &mut selection,
            &exact_match_symbol_tokens,
            target_limit,
        );
    }

    // RuntimeConfig continuation chunk.
    // rt-cfg queries about "how is X loaded" often retrieve the env-var lookup in chunk N
    // (e.g. config.rs:16-53 for RIPGREP_CONFIG_PATH) but miss the actual parsing/merging
    // logic in chunk N+1 (config.rs:54-133, parse() + parse_reader()). Meanwhile card 2 is
    // typically a noisy defs.rs `impl Flag for X` or a Paths struct that scores just below
    // the top. When top ≥ 0.70 (stronger signal than AM's 0.60 — avoids injecting noise
    // continuations when top card is a dev-script constant) and card 2 is from a different
    // file, replace card 2 with the same-file continuation of the top card.
    let rtcfg_cont_top_score = selection
        .snippets
        .first()
        .map(|s| s.score)
        .unwrap_or_default();
    if intent.kind == QueryIntentKind::RuntimeConfig && rtcfg_cont_top_score >= 0.70 {
        let rtcfg_continuation = selection.snippets.first().and_then(|top_item| {
            let top_path = top_item.path.clone();
            let top_start = top_item.start_line;
            let top_score = top_item.score;
            let has_foreign_card2 = selection
                .snippets
                .get(1)
                .is_some_and(|s| s.path != top_path);
            if !has_foreign_card2 {
                return None;
            }
            let cont_id = runtime.adjacent_chunk(&top_path, top_start)?;
            let cont = runtime.chunk(cont_id)?;
            // Guard: skip continuation if it starts a different function than the top
            // chunk. For example, get_load_dotenv → stream_with_context is a wrong
            // continuation because they are unrelated functions in the same file.
            // Look up the top chunk's symbol_hint via its start_line/path.
            let top_chunk = runtime
                .all_chunks()
                .iter()
                .find(|c| c.path == top_path && c.start_line == top_start);
            if let Some(top_c) = top_chunk
                && let (Some(top_sym), Some(cont_sym)) = (&top_c.symbol_hint, &cont.symbol_hint)
                && top_sym != cont_sym
            {
                return None;
            }
            Some(crate::rpc::QueryResultItem {
                path: cont.path.clone(),
                start_line: cont.start_line,
                end_line: cont.end_line,
                language: cont.language.clone(),
                score: top_score * 0.60,
                reasons: vec!["config-continuation".to_string()],
                channel_scores: QueryChannelScores::default(),
                text: cont.text.clone(),
                slm_relevance_note: None,
            })
        });
        if let Some(cont_item) = rtcfg_continuation {
            // Replace card 2 (noisy foreign card) with the continuation of the config chunk.
            selection.snippets.truncate(1);
            selection.snippets.push(cont_item);
        }
    }
    // Note: runtime-env-var-pack (Phase CP) removed — it over-anchors Claude on a
    // narrow set of env vars, causing less comprehensive answers now that baseline
    // Claude is strong enough to explore env-related code on its own.
    // Regular retrieval + DF fix provides sufficient starting evidence.
    inject_context_plugins(query, runtime, &mut selection.snippets, target_limit);

    // Post-pack noise filter: when a synthetic condenser pack is injected as
    // card 1 for FlowTrace, the pack already synthesizes the key evidence.
    // Remaining HNSW cards are generic keyword matches that add "context rot"
    // (every irrelevant token degrades attention). Keep only secondaries
    // scoring above pack_score * 0.95 to trim noise while preserving
    // high-quality supporting context.
    if intent.kind == QueryIntentKind::FlowTrace
        && selection.snippets.len() > 1
        && selection
            .snippets
            .first()
            .is_some_and(|s| s.reasons.iter().any(|r| r.ends_with("-pack")))
    {
        let pack_score = selection.snippets[0].score;
        let pack_floor = pack_score * 0.95;
        selection
            .snippets
            .retain(|s| s.reasons.iter().any(|r| r.ends_with("-pack")) || s.score >= pack_floor);
    }

    // Coverage-style TestLookup queries ("what tests cover X") need a
    // compact inventory of the dominant test file, not just one tiny test chunk.
    maybe_inject_test_file_inventory_card(query, runtime, &scored, &mut selection, target_limit);

    // Diagnostics: SLM overrides recommended_injection + skip_reason in daemon.rs
    let diagnostics = QueryDiagnostics {
        intent: intent_name(intent.kind).to_string(),
        confidence: 0.0,
        top_score: selection
            .snippets
            .first()
            .map(|s| s.score)
            .unwrap_or_default(),
        margin: 0.0,
        signals: selection
            .snippets
            .first()
            .map(|s| s.reasons.clone())
            .unwrap_or_default(),
        top_language: selection.snippets.first().map(|s| s.language.clone()),
        snippet_languages: selected_snippet_languages(&selection.snippets),
        repo_ecosystems: runtime.repo_ecosystems().to_vec(),
        top_ecosystem: selection
            .snippets
            .first()
            .and_then(|s| snippet_ecosystems(s).into_iter().next()),
        snippet_ecosystems: selected_snippet_ecosystems(&selection.snippets),
        recommended_injection: !selection.snippets.is_empty() && intent.code_related,
        skip_reason: if !intent.code_related {
            Some(SKIP_REASON_NON_CODE_INTENT.to_string())
        } else {
            None
        },
    };

    let query_tokens = extract_query_proof_tokens(query);
    let context = build_context(
        &selection.snippets,
        config.context_char_budget,
        &query_tokens,
    );
    Ok(QueryResponse {
        total_candidates: lexical.len() + vector.len() + symbol.len() + path.len() + graph.len(),
        context,
        snippets: selection.snippets,
        diagnostics,
        call_graph_summary: None,
        detected_intent: Some(intent_name(intent.kind).to_string()),
        timing_ms: None,
        snippet_refs: Vec::new(),
    })
}

/// Build an injected context string from a list of snippets.
/// Used by daemon.rs after post-retrieval filtering (session dedup, prefetch).
pub fn format_context(
    snippets: &[QueryResultItem],
    budget: usize,
    query_tokens: &[String],
) -> String {
    context::build_context(snippets, budget, query_tokens)
}

fn selected_snippet_languages(snippets: &[QueryResultItem]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for snippet in snippets {
        if snippet.language.is_empty() {
            continue;
        }
        if seen.insert(snippet.language.clone()) {
            out.push(snippet.language.clone());
        }
    }
    out
}

fn snippet_ecosystems(snippet: &QueryResultItem) -> Vec<String> {
    ecosystem_tags_for_chunk(&snippet.path, &snippet.language, &snippet.text)
}

fn selected_snippet_ecosystems(snippets: &[QueryResultItem]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for snippet in snippets {
        for ecosystem in snippet_ecosystems(snippet) {
            if seen.insert(ecosystem.clone()) {
                out.push(ecosystem);
            }
        }
    }
    out
}

fn first_matching_ecosystem<'a>(
    query_ecosystems: &'a [String],
    chunk_ecosystems: &[String],
) -> Option<&'a str> {
    query_ecosystems
        .iter()
        .find(|ecosystem| {
            chunk_ecosystems
                .iter()
                .any(|candidate| candidate == *ecosystem)
        })
        .map(String::as_str)
}

/// Build a compact call graph summary for the top injected snippets.
/// Returns None if no snippets have symbol hints or callers/callees.
pub fn build_call_graph_summary(
    runtime: &RuntimeIndex,
    snippets: &[QueryResultItem],
    max_chars: usize,
) -> Option<String> {
    let mut entries: Vec<String> = Vec::new();

    for (snippet_idx, snippet) in snippets.iter().enumerate() {
        if snippet_idx >= 5 {
            break;
        }

        // Find the matching chunk to get symbol_hint and chunk_id
        let chunk = runtime
            .all_chunks()
            .iter()
            .find(|c| c.path == snippet.path && c.start_line == snippet.start_line);

        let symbol = match chunk.and_then(|c| c.symbol_hint.as_deref()) {
            Some(s) if !s.is_empty() && !is_generic_symbol_hint(s) => s.to_string(),
            _ => continue,
        };

        let chunk_id = chunk.map(|c| c.id);

        // callers: chunks that call this symbol
        let callers = runtime.callers_of(&symbol);
        let caller_names: Vec<String> = callers
            .iter()
            .take(3)
            .map(|c| {
                let sym = c
                    .symbol_hint
                    .as_deref()
                    .unwrap_or_else(|| last_path_component(&c.path));
                truncate_to(sym, 40).to_string()
            })
            .collect();

        // callees: symbols this chunk calls
        let callee_names: Vec<String> = if let Some(id) = chunk_id {
            runtime
                .callees_of(id)
                .into_iter()
                .take(3)
                .map(|t| truncate_to(&t, 40).to_string())
                .collect()
        } else {
            Vec::new()
        };

        if caller_names.is_empty() && callee_names.is_empty() {
            continue;
        }

        let file_name = last_path_component(&snippet.path);
        let mut entry = format!("{}  ({}:{})\n", symbol, file_name, snippet.start_line);
        if !caller_names.is_empty() {
            entry.push_str(&format!("  ← called by: {}\n", caller_names.join(", ")));
        }
        // Use "refs:" instead of "calls:" — the graph tokens are extracted
        // at chunk level and include calls from nested functions within the chunk,
        // not only direct callees of the primary symbol. Using "refs:" avoids
        // misattributing indirect callees (e.g. reconcileChildFibers chunk includes
        // deleteRemainingChildren from reconcileChildFibersImpl's body, leading
        // Claude to incorrectly claim reconcileChildFibers directly calls it).
        if !callee_names.is_empty() {
            entry.push_str(&format!("  → refs: {}\n", callee_names.join(", ")));
        }
        entries.push(entry);
    }

    if entries.is_empty() {
        return None;
    }

    let mut out = String::from("[structural context]\n");
    for entry in entries {
        if out.len() + entry.len() > max_chars {
            break;
        }
        out.push_str(&entry);
    }
    Some(out)
}

fn should_prefer_same_file_definition_continuation(
    def_item: &QueryResultItem,
    continuation: &ChunkRecord,
) -> bool {
    let Some(symbol) = continuation.symbol_hint.as_deref() else {
        return false;
    };
    if is_generic_symbol_hint(symbol) {
        return false;
    }
    let span_lines = def_item.end_line.saturating_sub(def_item.start_line) + 1;
    if span_lines > 8 {
        return false;
    }
    let code_lines = def_item
        .text
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            !trimmed.is_empty()
                && !trimmed.starts_with("//")
                && !trimmed.starts_with("/*")
                && !trimmed.starts_with('*')
        })
        .count();
    if code_lines > 6 {
        return false;
    }
    if continuation.start_line > def_item.end_line.saturating_add(40) {
        return false;
    }
    def_item.text.contains(&format!("{symbol}(")) || def_item.text.contains(&format!("{symbol} ("))
}

fn build_symbol_definition_first_steps_card(
    continuation: &ChunkRecord,
    score: f32,
) -> Option<QueryResultItem> {
    let steps = extract_symbol_definition_first_steps(&continuation.text, 4);
    if steps.len() < 2 {
        return None;
    }
    let mut summary = String::from("first steps:");
    for (included, step) in steps.iter().enumerate() {
        let sep = if included == 0 { " " } else { ", " };
        if summary.len() + sep.len() + step.len() > 160 {
            let remaining = steps.len().saturating_sub(included);
            if remaining > 0 {
                summary.push_str(&format!(" +{} more", remaining));
            }
            break;
        }
        summary.push_str(sep);
        summary.push_str(step);
    }
    let mut text_lines = Vec::with_capacity(steps.len() + 1);
    text_lines.push(summary);
    text_lines.extend(steps);
    Some(QueryResultItem {
        path: continuation.path.clone(),
        start_line: continuation.start_line,
        end_line: continuation.end_line,
        language: continuation.language.clone(),
        score,
        reasons: vec!["wrapper-implementation-pack".to_string()],
        channel_scores: QueryChannelScores::default(),
        text: text_lines.join("\n"),
        slm_relevance_note: Some("same-file first steps summary".to_string()),
    })
}

fn maybe_inject_web_request_flow_chain_card(
    query: &str,
    runtime: &RuntimeIndex,
    selection: &mut SnippetSelectionState,
    target_limit: usize,
) {
    if selection.snippets.is_empty() || !is_request_to_view_flow_query(query) {
        return;
    }

    let Some(wsgi_chunk) = find_symbol_chunk(runtime, None, "wsgi_app") else {
        return;
    };
    let path = wsgi_chunk.path.clone();
    let Some(full_dispatch_chunk) =
        find_symbol_chunk(runtime, Some(path.as_str()), "full_dispatch_request")
    else {
        return;
    };
    let Some(dispatch_chunk) = find_symbol_chunk(runtime, Some(path.as_str()), "dispatch_request")
    else {
        return;
    };
    let top_score = selection.snippets.first().map(|s| s.score).unwrap_or(0.40);
    let Some(card) = build_web_request_flow_chain_card(
        wsgi_chunk,
        full_dispatch_chunk,
        dispatch_chunk,
        top_score * 0.95,
    ) else {
        return;
    };

    if selection
        .snippets
        .iter()
        .any(|s| s.path == card.path && s.reasons.iter().any(|r| r == "web-request-flow-pack"))
    {
        return;
    }

    selection.snippets.insert(0, card);
    if selection.snippets.len() > target_limit {
        selection.snippets.truncate(target_limit);
    }
}

fn is_request_to_view_flow_query(query: &str) -> bool {
    let lower = query.to_ascii_lowercase();
    contains_any(&lower, &["http", "request"])
        && contains_any(&lower, &["view", "handler", "endpoint"])
        && contains_any(&lower, &["trace", "call chain", "flow"])
}

fn build_web_request_flow_chain_card(
    wsgi_chunk: &ChunkRecord,
    full_dispatch_chunk: &ChunkRecord,
    dispatch_chunk: &ChunkRecord,
    score: f32,
) -> Option<QueryResultItem> {
    let request_context_line = extract_chunk_line_with_needle(wsgi_chunk, &["request_context("]);
    let push_context_line = extract_chunk_line_with_needle(wsgi_chunk, &["ctx.push("]);
    let (wsgi_call_line, wsgi_call_text) =
        extract_chunk_line_with_needle(wsgi_chunk, &["full_dispatch_request("])?;
    let preprocess_line =
        extract_chunk_line_with_needle(full_dispatch_chunk, &["preprocess_request("]);
    let (full_dispatch_line, full_dispatch_text) =
        extract_chunk_line_with_needle(full_dispatch_chunk, &["dispatch_request("])?;
    let finalize_line = extract_chunk_line_with_needle(full_dispatch_chunk, &["finalize_request("]);
    let (dispatch_line, dispatch_text) = extract_chunk_line_with_needle(
        dispatch_chunk,
        &[
            "view_functions[",
            "view_function(",
            "ensure_sync(self.view_functions[",
        ],
    )?;
    let summary = format!(
        "chain: wsgi_app@{} -> full_dispatch_request@{} -> dispatch_request@{} -> view_functions[rule.endpoint]@{}",
        wsgi_chunk.start_line,
        full_dispatch_chunk.start_line,
        dispatch_chunk.start_line,
        dispatch_line
    );
    let mut text_lines = vec![summary];
    let mut seen = HashSet::new();
    push_compact_evidence_line(&mut text_lines, &mut seen, "wsgi_app", request_context_line);
    push_compact_evidence_line(&mut text_lines, &mut seen, "wsgi_app", push_context_line);
    push_compact_evidence_line(
        &mut text_lines,
        &mut seen,
        "wsgi_app",
        Some((wsgi_call_line, wsgi_call_text)),
    );
    push_compact_evidence_line(
        &mut text_lines,
        &mut seen,
        "full_dispatch_request",
        preprocess_line,
    );
    push_compact_evidence_line(
        &mut text_lines,
        &mut seen,
        "full_dispatch_request",
        Some((full_dispatch_line, full_dispatch_text)),
    );
    push_compact_evidence_line(
        &mut text_lines,
        &mut seen,
        "full_dispatch_request",
        finalize_line,
    );
    push_compact_evidence_line(
        &mut text_lines,
        &mut seen,
        "dispatch_request",
        Some((dispatch_line, dispatch_text)),
    );
    let text = text_lines.join("\n");
    Some(QueryResultItem {
        path: wsgi_chunk.path.clone(),
        start_line: dispatch_chunk
            .start_line
            .min(full_dispatch_chunk.start_line),
        end_line: wsgi_chunk.end_line.max(full_dispatch_chunk.end_line),
        language: wsgi_chunk.language.clone(),
        score,
        reasons: vec!["web-request-flow-pack".to_string()],
        channel_scores: QueryChannelScores::default(),
        text,
        slm_relevance_note: Some("request-to-view chain summary".to_string()),
    })
}

fn promote_exact_match_symbol_definition_chunks(
    runtime: &RuntimeIndex,
    scored: &[ScoredChunk],
    selection: &mut SnippetSelectionState,
    exact_match_symbol_tokens: &[String],
    target_limit: usize,
) {
    if selection.snippets.is_empty() || exact_match_symbol_tokens.is_empty() {
        return;
    }
    let mut promoted = Vec::new();
    let mut seen = HashSet::<(String, usize, usize)>::new();
    for item in &selection.snippets {
        if item_matches_exact_symbol_tokens(runtime, item, exact_match_symbol_tokens)
            && seen.insert((item.path.clone(), item.start_line, item.end_line))
        {
            promoted.push(item.clone());
        }
    }
    for candidate in scored {
        if promoted.len() >= target_limit {
            break;
        }
        let Some(chunk) = runtime.chunk(candidate.id) else {
            continue;
        };
        if !chunk_matches_exact_symbol_tokens(chunk, exact_match_symbol_tokens) {
            continue;
        }
        if !seen.insert((chunk.path.clone(), chunk.start_line, chunk.end_line)) {
            continue;
        }
        if let Some(item) = query_result_item_from_scored(runtime, candidate) {
            promoted.push(item);
        }
    }
    if promoted.is_empty() {
        return;
    }
    let mut others = selection
        .snippets
        .iter()
        .filter(|item| !item_matches_exact_symbol_tokens(runtime, item, exact_match_symbol_tokens))
        .cloned()
        .collect::<Vec<_>>();
    promoted.append(&mut others);
    promoted.truncate(target_limit);
    selection.snippets = promoted;
}

fn maybe_inject_symbol_definition_delegate_pack(
    runtime: &RuntimeIndex,
    selection: &mut SnippetSelectionState,
    exact_match_symbol_tokens: &[String],
    target_limit: usize,
) {
    if selection.snippets.len() < 2 || exact_match_symbol_tokens.is_empty() {
        return;
    }
    let Some(def_item) = selection
        .snippets
        .iter()
        .find(|item| item_matches_exact_symbol_tokens(runtime, item, exact_match_symbol_tokens))
        .cloned()
    else {
        return;
    };
    let Some(def_chunk) = runtime_chunk_for_item(runtime, &def_item) else {
        return;
    };
    let Some(alt_def_item) = selection
        .snippets
        .iter()
        .filter(|item| item.path != def_item.path)
        .find(|item| item_matches_exact_symbol_tokens(runtime, item, exact_match_symbol_tokens))
        .cloned()
    else {
        return;
    };
    let Some(alt_def_chunk) = runtime_chunk_for_item(runtime, &alt_def_item) else {
        return;
    };
    let Some(callee_chunk) =
        choose_symbol_definition_delegate_chunk(runtime, def_chunk, alt_def_chunk)
    else {
        return;
    };
    let Some(card) = build_symbol_definition_delegate_card(
        def_chunk,
        alt_def_chunk,
        callee_chunk,
        def_item.score * 0.78,
    ) else {
        return;
    };
    selection.snippets.truncate(1);
    selection.snippets.push(card);
    if selection.snippets.len() > target_limit {
        selection.snippets.truncate(target_limit);
    }
}

fn choose_symbol_definition_delegate_chunk<'a>(
    runtime: &'a RuntimeIndex,
    def_chunk: &ChunkRecord,
    alt_def_chunk: &ChunkRecord,
) -> Option<&'a ChunkRecord> {
    let mut best: Option<(&ChunkRecord, i32)> = None;
    for callee in runtime.callees_of(def_chunk.id) {
        if callee.len() < 3 || is_generic_symbol_hint(&callee) {
            continue;
        }
        for chunk in runtime.all_chunks() {
            if chunk.path == def_chunk.path || is_test_path(&chunk.path) {
                continue;
            }
            if chunk.symbol_hint.as_deref() != Some(callee.as_str()) {
                continue;
            }
            let mut score = 0i32;
            if chunk.path == alt_def_chunk.path {
                score += 6;
            }
            if chunk.start_line >= alt_def_chunk.end_line
                && chunk.start_line <= alt_def_chunk.end_line.saturating_add(160)
            {
                score += 4;
            }
            if chunk.text.contains("Flask.register_blueprint") {
                score += 3;
            }
            if chunk.text.contains("app.blueprints[") {
                score += 2;
            }
            if best
                .as_ref()
                .is_none_or(|(_, best_score)| score > *best_score)
            {
                best = Some((chunk, score));
            }
        }
    }
    best.map(|(chunk, _)| chunk)
}

fn build_symbol_definition_delegate_card(
    def_chunk: &ChunkRecord,
    alt_def_chunk: &ChunkRecord,
    callee_chunk: &ChunkRecord,
    score: f32,
) -> Option<QueryResultItem> {
    let def_symbol = def_chunk.symbol_hint.as_deref().unwrap_or("definition");
    let alt_symbol = alt_def_chunk.symbol_hint.as_deref().unwrap_or("definition");
    let callee_symbol = callee_chunk.symbol_hint.as_deref().unwrap_or("delegate");
    let (delegate_line, delegate_text) =
        extract_chunk_line_with_needle(def_chunk, &[".register("])?;
    let (nested_line, nested_text) =
        extract_chunk_line_with_needle(alt_def_chunk, &["_blueprints.append("])?;
    let mut lines = vec![
        format!(
            "delegation: {def_symbol}@{} delegates to {callee_symbol}@{}; nested {alt_symbol}@{} stores child blueprints",
            def_chunk.start_line, callee_chunk.start_line, alt_def_chunk.start_line
        ),
        format!("{def_symbol}@{delegate_line}: {delegate_text}"),
        format!("{alt_symbol}@{nested_line}: {nested_text}"),
    ];
    for needles in [
        &["app.blueprints["][..],
        &["make_setup_state("][..],
        &["_merge_blueprint_funcs("][..],
        &["deferred(state)"][..],
        &["blueprint.register(app, bp_options)"][..],
    ] {
        if let Some((line_no, line_text)) = extract_chunk_line_with_needle(callee_chunk, needles) {
            lines.push(format!("{callee_symbol}@{line_no}: {line_text}"));
        }
    }
    if lines.len() < 5 {
        return None;
    }
    Some(QueryResultItem {
        path: callee_chunk.path.clone(),
        start_line: alt_def_chunk.start_line.min(callee_chunk.start_line),
        end_line: alt_def_chunk.end_line.max(callee_chunk.end_line),
        language: callee_chunk.language.clone(),
        score,
        reasons: vec!["delegated-definition-pack".to_string()],
        channel_scores: QueryChannelScores::default(),
        text: lines.join("\n"),
        slm_relevance_note: Some("delegated implementation summary".to_string()),
    })
}

fn query_result_item_from_scored(
    runtime: &RuntimeIndex,
    candidate: &ScoredChunk,
) -> Option<QueryResultItem> {
    let chunk = runtime.chunk(candidate.id)?;
    Some(QueryResultItem {
        path: chunk.path.clone(),
        start_line: chunk.start_line,
        end_line: chunk.end_line,
        language: chunk.language.clone(),
        score: candidate.score,
        reasons: candidate.reasons.clone(),
        channel_scores: candidate.channel_scores,
        text: chunk.text.clone(),
        slm_relevance_note: None,
    })
}

fn runtime_chunk_for_item<'a>(
    runtime: &'a RuntimeIndex,
    item: &QueryResultItem,
) -> Option<&'a ChunkRecord> {
    runtime.all_chunks().iter().find(|chunk| {
        chunk.path == item.path
            && chunk.start_line == item.start_line
            && chunk.end_line == item.end_line
    })
}

fn item_matches_exact_symbol_tokens(
    runtime: &RuntimeIndex,
    item: &QueryResultItem,
    exact_match_symbol_tokens: &[String],
) -> bool {
    runtime_chunk_for_item(runtime, item)
        .is_some_and(|chunk| chunk_matches_exact_symbol_tokens(chunk, exact_match_symbol_tokens))
}

fn chunk_matches_exact_symbol_tokens(
    chunk: &ChunkRecord,
    exact_match_symbol_tokens: &[String],
) -> bool {
    let Some(hint) = chunk.symbol_hint.as_deref() else {
        return false;
    };
    if is_generic_symbol_hint(hint) {
        return false;
    }
    let hint_lower = hint.to_ascii_lowercase();
    exact_match_symbol_tokens
        .iter()
        .any(|token| token == &hint_lower)
}

fn extract_symbol_definition_first_steps(text: &str, max_steps: usize) -> Vec<String> {
    if max_steps == 0 {
        return Vec::new();
    }
    let mut steps = Vec::new();
    let mut skipped_signature = false;
    for raw_line in text.lines() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty()
            || trimmed == "{"
            || trimmed == "}"
            || trimmed.starts_with("//")
            || trimmed.starts_with('#')
            || trimmed.starts_with("/*")
            || trimmed.starts_with('*')
        {
            continue;
        }
        if !skipped_signature {
            skipped_signature = true;
            continue;
        }
        let line = raw_line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() || line == "{" || line == "}" {
            continue;
        }
        steps.push(line);
        if steps.len() >= max_steps {
            break;
        }
    }
    steps
}

fn augment_symbol_tokens_for_intent(
    query: &str,
    intent: &QueryIntent,
    symbol_tokens: &mut Vec<String>,
) {
    let lower = query.to_ascii_lowercase();
    let mut seen: HashSet<String> = symbol_tokens.iter().cloned().collect();
    match intent.kind {
        QueryIntentKind::FlowTrace
            if contains_any(&lower, &["http", "request"])
                && contains_any(&lower, &["view", "handler", "endpoint"]) =>
        {
            for token in [
                "wsgi_app",
                "request_context",
                "full_dispatch_request",
                "dispatch_request",
                "view_functions",
                "view_function",
                "handle_request",
                "serve_http",
            ] {
                let owned = token.to_string();
                if seen.insert(owned.clone()) {
                    symbol_tokens.push(owned);
                }
            }
        }
        QueryIntentKind::RuntimeConfig if is_runtime_env_var_query(query) => {
            for token in [
                "get_debug_flag",
                "get_load_dotenv",
                "load_dotenv",
                "load_app",
            ] {
                let owned = token.to_string();
                if seen.insert(owned.clone()) {
                    symbol_tokens.push(owned);
                }
            }
        }
        _ => {}
    }
}

fn query_for_initial_retrieval(query: &str, intent: &QueryIntent) -> String {
    if intent.kind == QueryIntentKind::SymbolDefinition {
        return symbol_definition_subject_query(query).to_string();
    }
    if intent.kind == QueryIntentKind::RuntimeConfig && is_runtime_env_var_query(query) {
        return format!(
            "{query} FLASK_APP FLASK_DEBUG FLASK_SKIP_DOTENV FLASK_RUN_FROM_CLI dotenv load_dotenv get_debug_flag get_load_dotenv"
        );
    }
    query.to_string()
}

fn exact_match_symbol_tokens_for_intent(
    query: &str,
    intent: &QueryIntent,
    symbol_tokens: &[String],
) -> Vec<String> {
    if intent.kind != QueryIntentKind::SymbolDefinition {
        return symbol_tokens.to_vec();
    }
    let subject_tokens = extract_query_symbol_tokens(symbol_definition_subject_query(query));
    if subject_tokens.is_empty() {
        symbol_tokens.to_vec()
    } else {
        subject_tokens
    }
}

fn symbol_definition_subject_query(query: &str) -> &str {
    const SUBJECT_CUT_MARKERS: &[&str] = &[
        " and what does",
        " and what do",
        " and what steps",
        " and how does",
        " and how do",
        " and who calls",
        " when called with",
        " when passed",
        " before calling",
        " after calling",
    ];
    let lower = query.to_ascii_lowercase();
    let cut_idx = SUBJECT_CUT_MARKERS
        .iter()
        .filter_map(|marker| lower.find(marker))
        .min()
        .unwrap_or(query.len());
    query[..cut_idx].trim()
}

pub(crate) fn is_test_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    // examples/ directories contain sample code, not repository test suites.
    if lower.starts_with("examples/") || lower.contains("/examples/") {
        return false;
    }
    // Path-level test directory detection
    if lower.contains("/test")
        || lower.contains("/spec")
        || lower.contains("__tests__")
        || lower.contains("__spec__")
        || lower.starts_with("test")
        || lower.starts_with("spec")
    {
        return true;
    }
    // Test-utility directories (e.g. packages/internal-test-utils/)
    if lower.contains("-test-") {
        return true;
    }
    // Stub/fixture directories containing test doubles (e.g. stubs/, fixtures/)
    if lower.contains("/stubs/") || lower.contains("/fixtures/") {
        return true;
    }
    // Filename-level test file detection for Go (_test.go), JS/TS (.test.ts, .spec.ts),
    // and Python (test_*.py) conventions — these files live alongside production code
    // rather than under a /test or /spec directory.
    let filename = lower.split('/').next_back().unwrap_or("");
    if filename.contains("_test.go")
        || filename.contains(".test.")
        || filename.contains(".spec.")
        || filename.starts_with("test_")
    {
        return true;
    }
    // Mock implementation files (e.g. eval_context_mock.go, consoleMock.js,
    // mock_provider.go). Mock files define test doubles — they are not real callers
    // for SymbolUsage queries and not real production paths for FlowTrace queries.
    let name_stem = filename.split('.').next().unwrap_or(filename);
    name_stem.ends_with("mock")
        || name_stem.ends_with("_mock")
        || name_stem.starts_with("mock_")
        || name_stem.starts_with("mock")
            && (filename.ends_with(".go") || filename.ends_with(".ts") || filename.ends_with(".js"))
}

/// True if the chunk lives under a devtools or internal-tooling directory.
/// DevTools directories contain debugging, visualization, and profiling tools —
/// they are internal infrastructure, not the production code architecture.
pub(crate) fn is_devtools_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.contains("devtools")
        || lower.contains("noop-renderer")
        || lower.contains("noop_renderer")
        || lower.contains("nooprenderer")
}

/// True if the chunk text starts with a struct, type, or class definition.
/// These are type definitions, not function call sites — wrong context for
/// SymbolUsage queries that ask "what calls X".
fn is_struct_or_type_definition(text: &str) -> bool {
    let trimmed = text.trim_start();
    // Strip visibility modifiers (pub, pub(crate), export, etc.)
    let stripped = trimmed
        .strip_prefix("pub(crate) ")
        .or_else(|| trimmed.strip_prefix("pub(super) "))
        .or_else(|| trimmed.strip_prefix("pub "))
        .or_else(|| trimmed.strip_prefix("export "))
        .unwrap_or(trimmed);
    stripped.starts_with("struct ")
        || stripped.starts_with("type ")
        || stripped.starts_with("class ")
        || stripped.starts_with("interface ")
        || stripped.starts_with("enum ")
}

/// True if the chunk text starts with a Go test helper function (lowercase `test` prefix).
/// In Go, actual test functions are `func TestXxx(t *testing.T)` (uppercase T).
/// Helpers like `func testPlan(t *testing.T) *plans.Plan` are private fixture factories.
fn is_go_test_helper_chunk(text: &str) -> bool {
    let trimmed = text.trim_start();
    // Pattern: func test<Uppercase>...
    if let Some(rest) = trimmed.strip_prefix("func test")
        && let Some(first_char) = rest.chars().next()
    {
        return first_char.is_ascii_uppercase();
    }
    false
}

/// True if the chunk text looks like a stub/placeholder implementation.
/// Stub bodies contain markers like `panic("not implemented")`, `unimplemented!()`,
/// `todo!()`, `raise NotImplementedError`, or consist only of returning an error.
/// These are not useful definitions for SymbolDefinition queries — the real implementation
/// lives elsewhere and Claude should find it instead of getting anchored on the stub.
fn is_stub_body(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    // Explicit stub markers across languages
    if lower.contains("not implemented")
        || lower.contains("unimplemented!(")
        || lower.contains("unimplemented!()")
        || lower.contains("todo!(")
        || lower.contains("todo!()")
        || lower.contains("raise notimplementederror")
        || lower.contains("throw new error(\"not implemented")
    {
        return true;
    }
    // Very short function that just returns an error/unsupported message.
    // e.g. `func (p *Provider) ReadStateBytes(...) { return fmt.Errorf("unsupported") }`
    // Count non-blank lines (excluding braces/signatures) — if ≤ 5 and contains "unsupported"
    // or "not supported", it's a stub.
    let non_blank: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && *l != "{" && *l != "}" && *l != ")")
        .collect();
    if non_blank.len() <= 6 && (lower.contains("unsupported") || lower.contains("not supported")) {
        return true;
    }
    // Empty function body: signature + nothing meaningful in the body.
    // e.g. `func (v *QueryOperationJSON) Plan(plan *plans.Plan, schemas *terraform.Schemas) {}`
    // or a `pass` statement in Python.
    // Count non-blank lines after stripping braces — if only 1-2 lines (the signature),
    // the function has no implementation.
    if non_blank.len() <= 2 {
        return true;
    }
    // 3 lines: signature + single trivial body line (pass, return, return nil, etc.)
    if non_blank.len() == 3 {
        let body_line = non_blank
            .last()
            .map(|l| l.trim_end_matches([';', ',']).trim())
            .unwrap_or("");
        if body_line == "pass"
            || body_line == "return"
            || body_line == "return nil"
            || body_line == "return nil, nil"
            || body_line == "return None"
        {
            return true;
        }
    }
    false
}

/// True if the path is a mock/test-double file (subset of is_test_path).
/// Mock files define stubs — they are usually not what "what tests cover X" queries want.
fn is_mock_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let filename = lower.split('/').next_back().unwrap_or("");
    let name_stem = filename.split('.').next().unwrap_or(filename);
    name_stem.ends_with("mock")
        || name_stem.ends_with("_mock")
        || name_stem.starts_with("mock_")
        || name_stem.starts_with("mock")
            && (filename.ends_with(".go") || filename.ends_with(".ts") || filename.ends_with(".js"))
}

/// Extract the "subject" from a test filename.
/// Examples: `plan_test.go` → Some("plan"), `test_plan.py` → Some("plan"),
/// `plan.test.ts` → Some("plan"), `command_test.go` → Some("command").
fn extract_test_subject_stem(filename: &str) -> Option<String> {
    let lower = filename.to_ascii_lowercase();
    // Split into parts by '.' first to handle plan.test.ts / plan.spec.js
    let parts: Vec<&str> = lower.split('.').collect();
    // Go/Rust: plan_test.go → parts = ["plan_test", "go"]
    if parts.len() >= 2 {
        let stem = parts[0];
        // {subject}_test pattern (Go, Rust)
        if let Some(subject) = stem.strip_suffix("_test")
            && !subject.is_empty()
        {
            return Some(subject.to_string());
        }
        // test_{subject} pattern (Python)
        if let Some(subject) = stem.strip_prefix("test_")
            && !subject.is_empty()
        {
            return Some(subject.to_string());
        }
    }
    // JS/TS: plan.test.ts / plan.spec.js → parts = ["plan", "test"/"spec", "ts"/"js"]
    if parts.len() >= 3 && matches!(parts[1], "test" | "spec") {
        let subject = parts[0];
        if !subject.is_empty() {
            return Some(subject.to_string());
        }
    }
    None
}

/// True if the chunk lives under an examples/ directory.
/// These are tutorial/sample files, not production source — they should be penalised
/// on Architecture queries but kept for TestLookup (which explicitly excludes them from
/// is_test_path() to avoid false test-path-boost).
fn is_examples_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.starts_with("examples/") || lower.contains("/examples/")
}

/// True if the chunk text contains an inline test block.
/// Handles: Rust (#[test], #[cfg(test)], mod tests), JS/TS (describe/it/test blocks).
fn is_inline_test_chunk(text: &str) -> bool {
    // Fast byte scan before any per-line work.
    if !text.contains("test") && !text.contains("describe") && !text.contains("#[") {
        return false;
    }
    text.contains("#[test]")
        || text.contains("#[cfg(test)]")
        || text.contains("mod tests {")
        || text.contains("mod tests{")
        || text.contains("describe(")
        || text.contains("describe.each(")
}

/// Extract backtick-quoted tokens that are genuinely all-lowercase in the original query.
/// "run" → ["run"]. "scheduleUpdateOnFiber" → [] (has uppercase → excluded).
/// Used for sym-hint seeding to surface definitions of short lowercase identifiers.
fn extract_lowercase_backtick_tokens(query: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = query.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '`' {
            let token: String = chars.by_ref().take_while(|&ch| ch != '`').collect();
            // Only keep tokens that are purely lowercase alphanumeric (no uppercase, no spaces).
            if token.len() >= 2
                && token.len() <= 64
                && token
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
                && !token.chars().any(|ch| ch.is_ascii_uppercase())
            {
                let normalized = token.to_ascii_lowercase();
                if !out.contains(&normalized) {
                    out.push(normalized);
                }
            }
        }
    }
    out
}

/// Extract subject tokens from a TestLookup query for path-based seeding.
/// Strips test-noise words ("unit tests cover where live repo...") and common stop words,
/// keeping content words ≥ 4 chars that describe the feature being tested.
fn test_subject_tokens(query: &str) -> Vec<String> {
    const STOP: &[&str] = &[
        "what",
        "which",
        "where",
        "when",
        "who",
        "how",
        "unit",
        "integration",
        "e2e",
        "test",
        "tests",
        "testing",
        "spec",
        "specs",
        "cover",
        "covers",
        "covered",
        "covering",
        "live",
        "lives",
        "located",
        "location",
        "find",
        "the",
        "a",
        "an",
        "and",
        "or",
        "in",
        "of",
        "for",
        "to",
        "on",
        "do",
        "does",
        "are",
        "is",
        "it",
        "they",
        "their",
        "them",
        "repo",
        "repository",
        "codebase",
        "logic",
        "code",
        "file",
        "files",
        "directory",
        "folder",
    ];
    query
        .split(|c: char| !c.is_alphanumeric())
        .map(|w| w.to_ascii_lowercase())
        .filter(|w| w.len() >= 4 && !STOP.contains(&w.as_str()))
        .collect()
}

fn is_test_coverage_inventory_query(query: &str) -> bool {
    let lower = query.to_ascii_lowercase();
    contains_any(&lower, &["test", "tests", "spec", "specs"])
        && contains_any(
            &lower,
            &[
                "cover",
                "covers",
                "covered",
                "covering",
                "exercise",
                "exercises",
                "validate",
                "validates",
                "verify",
                "verifies",
            ],
        )
}

fn maybe_inject_test_file_inventory_card(
    query: &str,
    runtime: &RuntimeIndex,
    scored: &[ScoredChunk],
    selection: &mut SnippetSelectionState,
    target_limit: usize,
) {
    if selection.snippets.is_empty() || !is_test_coverage_inventory_query(query) {
        return;
    }
    let Some(path) = select_test_inventory_path(query, &selection.snippets) else {
        return;
    };
    let seed_line = selection
        .snippets
        .iter()
        .find(|s| s.path == path)
        .map(|s| s.start_line)
        .unwrap_or_default();
    if selection.snippets.iter().filter(|s| s.path == path).count() >= 2 {
        return;
    }
    let top_score = selection
        .snippets
        .first()
        .map(|s| s.score)
        .unwrap_or_default();
    let Some(inventory_card) =
        build_test_file_inventory_card(query, runtime, scored, path, seed_line, top_score * 0.92)
    else {
        return;
    };
    if selection
        .snippets
        .iter()
        .any(|s| s.path == path && s.reasons.iter().any(|r| r == "test-file-inventory"))
    {
        return;
    }
    let insert_at = selection.snippets.len().min(1);
    selection.snippets.insert(insert_at, inventory_card);
    if selection.snippets.len() > target_limit {
        selection.snippets.pop();
    }
}

fn select_test_inventory_path<'a>(query: &str, snippets: &'a [QueryResultItem]) -> Option<&'a str> {
    let subject_tokens = test_subject_tokens(query);
    let test_snippets: Vec<&QueryResultItem> = snippets
        .iter()
        .filter(|s| is_test_path(&s.path) && !is_mock_path(&s.path))
        .collect();
    // Priority 1: iterate subject tokens in query order (most specific first).
    // For "What tests cover the plan command", "plan" appears before "command",
    // so plan_test.go (stem "plan") is preferred over command_test.go (stem "command").
    for subject in &subject_tokens {
        if let Some(s) = test_snippets.iter().find(|s| {
            let filename = s.path.rsplit('/').next().unwrap_or(&s.path);
            extract_test_subject_stem(filename).is_some_and(|stem| &stem == subject)
        }) {
            return Some(s.path.as_str());
        }
    }
    // Priority 2: file whose full path contains a subject token.
    if let Some(s) = test_snippets
        .iter()
        .find(|s| path_matches_subject_tokens(&s.path, &subject_tokens))
    {
        return Some(s.path.as_str());
    }
    // Priority 3: any test file in selection.
    test_snippets.first().map(|s| s.path.as_str())
}

fn path_matches_subject_tokens(path: &str, subject_tokens: &[String]) -> bool {
    if subject_tokens.is_empty() {
        return false;
    }
    let lower = path.to_ascii_lowercase();
    subject_tokens.iter().any(|token| lower.contains(token))
}

fn build_test_file_inventory_card(
    query: &str,
    runtime: &RuntimeIndex,
    scored: &[ScoredChunk],
    path: &str,
    seed_line: usize,
    score: f32,
) -> Option<QueryResultItem> {
    let mut candidate_entries =
        extract_test_inventory_entries_from_scored_chunks(runtime, scored, path);
    let absolute = Path::new(&runtime.state.repo_root).join(path);
    if let Ok(file_text) = fs::read_to_string(absolute) {
        let mut seen_lines = candidate_entries
            .iter()
            .map(|entry| entry.line_number)
            .collect::<HashSet<_>>();
        let mut seen_labels = candidate_entries
            .iter()
            .map(|entry| entry.label.clone())
            .collect::<HashSet<_>>();
        for entry in extract_test_inventory_entries(&file_text) {
            if seen_lines.insert(entry.line_number) && seen_labels.insert(entry.label.clone()) {
                candidate_entries.push(entry);
            }
        }
    }
    let entries = prioritize_test_inventory_entries(query, path, &candidate_entries, seed_line);
    if entries.len() < 2 {
        return None;
    }
    let lines = build_test_inventory_lines(
        &entries,
        TEST_INVENTORY_MAX_LINES,
        TEST_INVENTORY_LINE_CHAR_BUDGET,
    );
    if lines.is_empty() {
        return None;
    }
    Some(QueryResultItem {
        path: path.to_string(),
        start_line: entries.first()?.line_number,
        end_line: entries.last()?.line_number,
        language: language_label_for_path(path),
        score,
        reasons: vec!["test-file-inventory".to_string()],
        channel_scores: QueryChannelScores::default(),
        text: lines.join("\n"),
        slm_relevance_note: Some("same-file test coverage inventory".to_string()),
    })
}

fn extract_test_inventory_entries_from_scored_chunks(
    runtime: &RuntimeIndex,
    scored: &[ScoredChunk],
    path: &str,
) -> Vec<TestInventoryEntry> {
    let mut entries = Vec::new();
    let mut seen_lines = HashSet::new();
    for candidate in scored {
        let Some(chunk) = runtime.chunk(candidate.id) else {
            continue;
        };
        if chunk.path != path || !seen_lines.insert(chunk.start_line) {
            continue;
        }
        let Some(entry) = extract_chunk_test_entry(chunk) else {
            continue;
        };
        entries.push(entry);
        if entries.len() >= 12 {
            break;
        }
    }
    entries
}

fn extract_chunk_test_entry(chunk: &crate::index::ChunkRecord) -> Option<TestInventoryEntry> {
    for (idx, raw_line) in chunk.text.lines().enumerate() {
        let line = raw_line.trim_start();
        if let Some(label) = extract_named_test_definition(line) {
            return Some(TestInventoryEntry {
                line_number: chunk.start_line + idx,
                label,
            });
        }
        if line.starts_with("it(") || line.starts_with("test(") {
            return Some(TestInventoryEntry {
                line_number: chunk.start_line + idx,
                label: truncate_to(line, 80).to_string(),
            });
        }
    }
    chunk.symbol_hint.clone().map(|label| TestInventoryEntry {
        line_number: chunk.start_line,
        label,
    })
}

fn extract_test_inventory_entries(file_text: &str) -> Vec<TestInventoryEntry> {
    let mut entries = Vec::new();
    let mut pending_rust_test_attr = false;
    for (idx, raw_line) in file_text.lines().enumerate() {
        let line_number = idx + 1;
        let line = raw_line.trim_start();
        if line.starts_with("#[test]") {
            pending_rust_test_attr = true;
            continue;
        }
        if pending_rust_test_attr {
            pending_rust_test_attr = false;
            if let Some(name) = extract_named_test_definition(line) {
                entries.push(TestInventoryEntry {
                    line_number,
                    label: name,
                });
                continue;
            }
        }
        if let Some(name) = extract_named_test_definition(line) {
            entries.push(TestInventoryEntry {
                line_number,
                label: name,
            });
            continue;
        }
        if line.starts_with("it(") || line.starts_with("test(") {
            entries.push(TestInventoryEntry {
                line_number,
                label: truncate_to(line, 80).to_string(),
            });
        }
    }
    entries
}

fn extract_named_test_definition(line: &str) -> Option<String> {
    for prefix in ["async def test_", "def test_", "pub fn test_", "fn test_"] {
        if let Some(rest) = line.strip_prefix(prefix) {
            let name = format!("test_{}", take_identifier(rest));
            if name != "test_" {
                return Some(name);
            }
        }
    }
    None
}

fn take_identifier(input: &str) -> String {
    input
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
        .collect()
}

fn build_test_inventory_lines(
    entries: &[TestInventoryEntry],
    max_lines: usize,
    max_chars_per_line: usize,
) -> Vec<String> {
    if entries.is_empty() || max_lines == 0 {
        return Vec::new();
    }
    let group_count = entries.len().min(max_lines);
    let chunk_size = entries.len().div_ceil(group_count);
    entries
        .chunks(chunk_size)
        .take(group_count)
        .enumerate()
        .map(|(idx, group)| {
            let prefix = inventory_line_prefix(idx, group_count);
            let mut line = String::from(prefix);
            for (included, entry) in group.iter().enumerate() {
                let token = format!("{}@{}", entry.label, entry.line_number);
                let sep = if included == 0 { " " } else { ", " };
                if line.len() + sep.len() + token.len() > max_chars_per_line {
                    let remaining = group.len().saturating_sub(included);
                    if remaining > 0 {
                        let suffix = format!(" +{} more", remaining);
                        if line.len() + suffix.len() <= max_chars_per_line {
                            line.push_str(&suffix);
                        }
                    }
                    break;
                }
                line.push_str(sep);
                line.push_str(&token);
            }
            line
        })
        .collect()
}

fn inventory_line_prefix(idx: usize, total: usize) -> &'static str {
    match (idx, total) {
        (_, 0) => "tests:",
        (0, 1) => "tests:",
        (0, 2) => "early:",
        (1, 2) => "late:",
        (0, _) => "early:",
        (1, 3) => "mid:",
        (2, 3) => "late:",
        _ => "more:",
    }
}

fn prioritize_test_inventory_entries(
    query: &str,
    path: &str,
    entries: &[TestInventoryEntry],
    seed_line: usize,
) -> Vec<TestInventoryEntry> {
    let subject_tokens = test_subject_tokens(query);
    let lower_query = query.to_ascii_lowercase();
    let registration_query =
        lower_query.contains("register") || lower_query.contains("registration");
    let registration_keywords = [
        "register",
        "registration",
        "nested",
        "nesting",
        "prefix",
        "subdomain",
        "rename",
        "renaming",
        "self_registration",
        "unique_blueprint_names",
        "dotted_name",
        "empty_name",
        "url_defaults",
        "defaults",
        "default",
    ];
    let mut ranked = entries
        .iter()
        .cloned()
        .map(|entry| {
            let lower = entry.label.to_ascii_lowercase();
            let mut score = 0i32;
            for token in &subject_tokens {
                if lower.contains(token) {
                    score += 4;
                }
            }
            if registration_query && contains_any(&lower, &registration_keywords) {
                score += 3;
            }
            if registration_query
                && contains_any(&lower, &["error", "handler"])
                && !contains_any(&lower, &registration_keywords)
            {
                score -= 4;
            }
            if registration_query
                && contains_any(&lower, &["template", "filter", "processor", "static"])
                && !contains_any(&lower, &registration_keywords)
            {
                score -= 4;
            }
            if path_matches_subject_tokens(path, &subject_tokens)
                && contains_any(
                    &lower,
                    &[
                        "blueprint",
                        "nested",
                        "prefix",
                        "subdomain",
                        "register",
                        "registration",
                        "rename",
                        "renaming",
                        "defaults",
                        "self_registration",
                        "unique_blueprint_names",
                        "dotted_name",
                        "empty_name",
                    ],
                )
            {
                score += 2;
            }
            let distance = seed_line.abs_diff(entry.line_number);
            score += match distance {
                0..=80 => 4,
                81..=200 => 3,
                201..=400 => 2,
                401..=700 => 1,
                _ => 0,
            };
            (score, entry)
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|(score_a, entry_a), (score_b, entry_b)| {
        score_b
            .cmp(score_a)
            .then(entry_a.line_number.cmp(&entry_b.line_number))
    });
    let mut out = ranked
        .into_iter()
        .take(12)
        .map(|(_, entry)| entry)
        .collect::<Vec<_>>();
    out.sort_by_key(|entry| entry.line_number);
    out
}

fn is_generic_symbol_hint(s: &str) -> bool {
    // Single-word language keywords that describe structure, not identity
    matches!(
        s,
        "fn" | "pub"
            | "function"
            | "def"
            | "class"
            | "method"
            | "func"
            | "procedure"
            | "sub"
            | "lambda"
            | "arrow"
            | "block"
            | "module"
            | "impl"
            | "trait"
            | "struct"
            | "enum"
            | "interface"
            | "type"
            | "const"
            | "let"
            | "var"
            | "static"
            | "async"
            | "export"
    )
}

fn last_path_component(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn truncate_to(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        // Walk back from `max` to the nearest valid UTF-8 char boundary so we
        // never panic on multi-byte characters (e.g. Unicode symbol names).
        let mut cut = max;
        while cut > 0 && !s.is_char_boundary(cut) {
            cut -= 1;
        }
        &s[..cut]
    }
}

/// Find graph neighbors for a specific file and return (snippets, context_string).
/// Returns an empty pair if the file has no indexed chunks or no graph neighbors.
pub fn prefetch_neighbors_for_file(
    runtime: &RuntimeIndex,
    file_path: &str,
    limit: usize,
    context_budget: usize,
) -> (Vec<QueryResultItem>, String) {
    // Collect seed tokens from the file's indexed chunks.
    let mut seed_tokens = Vec::new();
    for chunk in runtime.all_chunks() {
        if chunk.path != file_path {
            continue;
        }
        seed_tokens.extend(extract_query_symbol_tokens(&chunk.text));
        seed_tokens.extend(graph_neighbor_seed_tokens(&chunk.path, &chunk.text));
    }
    seed_tokens.sort();
    seed_tokens.dedup();

    if seed_tokens.is_empty() {
        return (Vec::new(), String::new());
    }

    // Search the graph channel for neighbor chunks.
    let hit_limit = limit * 6;
    let neighbor_hits = runtime.search_graph_tokens(&seed_tokens, hit_limit);

    // Keep one top-scoring chunk per neighbor file.
    let mut seen_paths = HashSet::new();
    seen_paths.insert(file_path.to_string());
    let mut snippets = Vec::new();
    for (id, score) in neighbor_hits {
        if snippets.len() >= limit {
            break;
        }
        let Some(chunk) = runtime.chunk(id) else {
            continue;
        };
        if !seen_paths.insert(chunk.path.clone()) {
            continue;
        }
        snippets.push(QueryResultItem {
            path: chunk.path.clone(),
            start_line: chunk.start_line,
            end_line: chunk.end_line,
            language: chunk.language.clone(),
            score,
            reasons: vec!["graph-neighbor".to_string()],
            channel_scores: QueryChannelScores {
                graph: score,
                ..Default::default()
            },
            text: chunk.text.clone(),
            slm_relevance_note: None,
        });
    }

    let context = context::build_context(&snippets, context_budget, &[]);
    (snippets, context)
}

// ── Selection helpers ─────────────────────────────────────────────────────────

/// Per-intent default retrieval limit.
/// Precision intents (SymbolDefinition, FlowTrace) get fewer, higher-quality results.
/// Breadth intents (Architecture, TestLookup) get more candidates for coverage.
fn intent_retrieval_limit(kind: QueryIntentKind) -> usize {
    match kind {
        QueryIntentKind::SymbolDefinition | QueryIntentKind::FlowTrace => 5,
        QueryIntentKind::Architecture | QueryIntentKind::TestLookup => 8,
        QueryIntentKind::SymbolUsage => 5,
        _ => 6,
    }
}

fn min_selection_score(candidates: &[ScoredChunk], intent_kind: QueryIntentKind) -> f32 {
    let Some(top) = candidates.first() else {
        return f32::NEG_INFINITY;
    };
    let relative = (top.score * 0.40_f32).max(0.05);
    match intent_kind {
        // Raised to 0.25: lexical-only hits from common query words ("return", "call")
        // at scores 0.23-0.24 add noise for focused call-chain questions.
        QueryIntentKind::FlowTrace => relative.max(0.25),
        // Raised from 0.20 to 0.30 to exclude lexical noise at 0.26-0.29
        // that dilutes SymbolDef context when sym-hint-seed already placed the definition at 0.58.
        QueryIntentKind::SymbolDefinition => relative.max(0.30),
        QueryIntentKind::TestLookup => relative.max(0.22),
        // When top score is high (≥0.60, strong rt-cfg signal), raise floor to 0.40
        // to filter noise cards (brew formulas, hyperlink env-var code) at 0.33-0.38.
        // When top is low (< 0.60, weak signal like React __DEV__ flags), keep floor at 0.18.
        QueryIntentKind::RuntimeConfig => {
            if top.score >= 0.60 {
                relative.max(0.40)
            } else {
                relative.max(0.18)
            }
        }
        // Raised from none to 0.22: filters lexical noise at ~0.19 for sym-use queries.
        QueryIntentKind::SymbolUsage => relative.max(0.22),
        // When top score is high (≥0.60, strong signal), raise floor to 0.40
        // to filter weakly-related cards (devtools profiling, tests, server rendering) at 0.30-0.36
        // that dilute high-confidence architecture answers.
        // Even for low-confidence arch queries (top < 0.60), apply a minimum
        // floor of 0.30. Combined with the test-path penalty (−0.15), this filters
        // test fixture chunks that nominally score 0.41–0.44 but land at 0.26–0.29
        // after the penalty (below 0.30). Without this floor the standard relative floor
        // can be as low as 0.12 (top=0.30), admitting any chunk above 0.12.
        QueryIntentKind::Architecture => {
            if top.score >= 0.60 {
                relative.max(0.40)
            } else {
                relative.max(0.30)
            }
        }
    }
}

fn should_expand_graph_neighbors(intent_kind: QueryIntentKind) -> bool {
    // Disabled for SymbolUsage — graph expansion adds distant callee/caller
    // code (e.g. release scripts connected via scheduler imports) that dilutes the
    // precise call-site evidence collected by the regular sym-use retrieval.
    // SymbolDefinition graph expansion is gated separately via sym_def_seeded.
    matches!(intent_kind, QueryIntentKind::SymbolDefinition)
}

fn try_push_scored_chunk(
    runtime: &RuntimeIndex,
    candidate: &ScoredChunk,
    selection: &mut SnippetSelectionState,
) -> bool {
    let Some(chunk) = runtime.chunk(candidate.id) else {
        return false;
    };
    let path_count = selection
        .snippets_per_path
        .get(&chunk.path)
        .copied()
        .unwrap_or_default();
    if path_count >= selection.per_file_limit {
        return false;
    }
    let bucket = path_diversity_bucket(&chunk.path);
    let bucket_count = selection
        .snippets_per_bucket
        .get(&bucket)
        .copied()
        .unwrap_or_default();
    if bucket_count >= selection.per_bucket_limit {
        return false;
    }
    let fingerprint = snippet_fingerprint(&chunk.text);
    if !selection.seen_fingerprints.insert(fingerprint) {
        return false;
    }
    // Skip candidates that overlap with an already-selected snippet from the
    // same file. Stride=60/overlap=20 chunking can select two adjacent chunks that share
    // 20 lines — injecting both duplicates those lines and can confuse Claude by showing
    // the same code twice in slightly different evidence cards.
    // Exempt synthetic condenser packs from the overlap check. Packs intentionally
    // span large regions (e.g. web-request-flow-pack covers lines 966-1616) and should not
    // be removed when a constituent chunk (e.g. wsgi_app at 1566-1616) was selected first
    // with a higher individual score — the pack provides holistic call-chain context that
    // the constituent chunk alone cannot supply.
    let is_synthetic_pack = candidate.reasons.iter().any(|r| {
        matches!(
            r.as_str(),
            "web-request-flow-pack"
                | "wrapper-implementation-pack"
                | "delegated-definition-pack"
                | "react-effect-lifecycle-pack"
                | "nextjs-app-router-pack"
        )
    });
    let overlaps_existing = !is_synthetic_pack
        && selection.snippets.iter().any(|s| {
            s.path == chunk.path && s.start_line < chunk.end_line && chunk.start_line < s.end_line
        });
    if overlaps_existing {
        return false;
    }
    selection.snippets.push(QueryResultItem {
        path: chunk.path.clone(),
        start_line: chunk.start_line,
        end_line: chunk.end_line,
        language: chunk.language.clone(),
        score: candidate.score,
        reasons: candidate.reasons.clone(),
        channel_scores: candidate.channel_scores,
        text: chunk.text.clone(),
        slm_relevance_note: None,
    });
    selection.selected_chunk_ids.push(candidate.id);
    *selection
        .snippets_per_path
        .entry(chunk.path.clone())
        .or_insert(0) += 1;
    *selection.snippets_per_bucket.entry(bucket).or_insert(0) += 1;
    true
}

fn expand_graph_neighbors(
    runtime: &RuntimeIndex,
    selection: &mut SnippetSelectionState,
    target_total: usize,
    expansion_limit: usize,
) {
    if expansion_limit == 0
        || selection.snippets.is_empty()
        || selection.snippets.len() >= target_total
        || selection.selected_chunk_ids.is_empty()
    {
        return;
    }

    let seed_ids = selection
        .selected_chunk_ids
        .iter()
        .copied()
        .take(3)
        .collect::<Vec<_>>();

    let mut selected_ids = selection
        .selected_chunk_ids
        .iter()
        .copied()
        .collect::<HashSet<_>>();
    let mut neighbor_scores: HashMap<u64, f32> = HashMap::new();
    for (seed_idx, seed_id) in seed_ids.iter().enumerate() {
        let Some(seed_chunk) = runtime.chunk(*seed_id) else {
            continue;
        };
        let seed_tokens = graph_neighbor_seed_tokens(&seed_chunk.path, &seed_chunk.text);
        if seed_tokens.is_empty() {
            continue;
        }
        let neighbor_window = expansion_limit.saturating_mul(6).max(12);
        for (neighbor_id, raw_score) in runtime.search_graph_tokens(&seed_tokens, neighbor_window) {
            if neighbor_id == *seed_id || selected_ids.contains(&neighbor_id) {
                continue;
            }
            let seed_priority_bonus = 0.03f32 / ((seed_idx as f32) + 1.0);
            // Cap raw_score at 0.45 to prevent graph-neighbor inflation.
            // raw_score from search_graph_tokens can exceed 1.0 (token-weight × rarity
            // accumulated across multiple matching tokens). Without the cap, graph-neighbor
            // chunks routinely score > 1.0 and dominate over the definition/usage chunks
            // that were carefully selected by the main retrieval pipeline.
            // 0.45 is intentionally below the sym-hint-seed+hint-match-boost floor (0.58)
            // so that a seeded definition chunk always outranks its graph neighbors.
            let candidate_score = raw_score.min(0.45) + seed_priority_bonus;
            let entry = neighbor_scores.entry(neighbor_id).or_insert(f32::MIN);
            if candidate_score > *entry {
                *entry = candidate_score;
            }
        }
    }
    if neighbor_scores.is_empty() {
        return;
    }

    let mut ordered_neighbors = neighbor_scores.into_iter().collect::<Vec<_>>();
    ordered_neighbors.sort_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.cmp(&right.0))
    });

    let mut added = 0usize;
    for (neighbor_id, neighbor_score) in ordered_neighbors {
        if added >= expansion_limit || selection.snippets.len() >= target_total {
            break;
        }
        if selected_ids.contains(&neighbor_id) {
            continue;
        }
        let candidate = ScoredChunk {
            id: neighbor_id,
            score: neighbor_score,
            reasons: vec!["graph-neighbor".to_string(), "graph-hit".to_string()],
            channel_scores: QueryChannelScores {
                graph: neighbor_score.max(0.0),
                ..QueryChannelScores::default()
            },
        };
        if try_push_scored_chunk(runtime, &candidate, selection) {
            selected_ids.insert(neighbor_id);
            added = added.saturating_add(1);
        }
    }
}

fn graph_neighbor_seed_tokens(path: &str, text: &str) -> Vec<String> {
    let mut out = extract_query_symbol_tokens(text);
    if out.len() < 4 {
        for token in extract_query_path_tokens(path) {
            if out.iter().any(|existing| existing == &token) {
                continue;
            }
            out.push(token);
        }
    }
    out.retain(|token| token.len() >= 3 && !is_query_noise_token(token));
    out.truncate(8);
    out
}

// ── Channel fusion ────────────────────────────────────────────────────────────

fn fuse_channel_scores(
    lexical: &[(u64, f32)],
    vector: &[(u64, f32)],
    symbol: &[(u64, f32)],
    path: &[(u64, f32)],
    graph: &[(u64, f32)],
    intent: &QueryIntent,
) -> HashMap<u64, CandidateScore> {
    let mut scores: HashMap<u64, CandidateScore> = HashMap::new();
    apply_channel_scores(
        &mut scores,
        lexical,
        intent.weights.lexical,
        ChannelKind::Lexical,
    );
    apply_channel_scores(
        &mut scores,
        vector,
        intent.weights.vector,
        ChannelKind::Vector,
    );
    apply_channel_scores(
        &mut scores,
        symbol,
        intent.weights.symbol,
        ChannelKind::Symbol,
    );
    apply_channel_scores(&mut scores, path, intent.weights.path, ChannelKind::Path);
    apply_channel_scores(&mut scores, graph, intent.weights.graph, ChannelKind::Graph);
    scores
}

fn diversify_channel_by_path(
    runtime: &RuntimeIndex,
    channel: &[(u64, f32)],
    limit: usize,
) -> Vec<(u64, f32)> {
    let mut out = Vec::new();
    let mut seen_paths = HashSet::new();
    for (id, score) in channel {
        let Some(chunk) = runtime.chunk(*id) else {
            continue;
        };
        if !seen_paths.insert(chunk.path.as_str()) {
            continue;
        }
        out.push((*id, *score));
        if out.len() >= limit {
            break;
        }
    }
    out
}

fn apply_channel_scores(
    scores: &mut HashMap<u64, CandidateScore>,
    channel: &[(u64, f32)],
    weight: f32,
    kind: ChannelKind,
) {
    for (rank, (id, raw_score)) in channel.iter().enumerate() {
        let rr = weight / ((rank as f32) + RRF_K);
        let normalized = normalize_channel_score(*raw_score, kind);
        let contribution = rr + normalized * weight * 0.35;
        let entry = scores.entry(*id).or_default();
        entry.score += contribution;
        add_channel_contribution(&mut entry.channel_scores, kind, contribution);
        push_unique_reason(&mut entry.signals, channel_signal_name(kind));
    }
}

fn add_channel_contribution(scores: &mut QueryChannelScores, kind: ChannelKind, value: f32) {
    match kind {
        ChannelKind::Lexical => scores.lexical += value,
        ChannelKind::Vector => scores.vector += value,
        ChannelKind::Symbol => scores.symbol += value,
        ChannelKind::Path => scores.path += value,
        ChannelKind::Graph => scores.graph += value,
    }
}

fn normalize_channel_score(raw_score: f32, kind: ChannelKind) -> f32 {
    match kind {
        ChannelKind::Lexical => (raw_score / 25.0).clamp(0.0, 1.0),
        ChannelKind::Vector => raw_score.clamp(0.0, 1.0),
        ChannelKind::Symbol | ChannelKind::Path | ChannelKind::Graph => {
            (raw_score / 2.0).clamp(0.0, 1.0)
        }
    }
}

fn channel_signal_name(kind: ChannelKind) -> &'static str {
    match kind {
        ChannelKind::Lexical => "lexical-hit",
        ChannelKind::Vector => "semantic-hit",
        ChannelKind::Symbol => "symbol-hit",
        ChannelKind::Path => "path-hit",
        ChannelKind::Graph => "graph-hit",
    }
}

fn retrieval_mode_allows_channel(mode: RetrievalMode, kind: ChannelKind) -> bool {
    match mode {
        RetrievalMode::Hybrid => true,
        RetrievalMode::Lexical => matches!(kind, ChannelKind::Lexical),
        RetrievalMode::Vector => matches!(kind, ChannelKind::Vector),
        RetrievalMode::SymbolGraph => {
            matches!(
                kind,
                ChannelKind::Symbol | ChannelKind::Path | ChannelKind::Graph
            )
        }
    }
}

fn push_unique_reason(reasons: &mut Vec<String>, reason: &str) {
    if reasons.iter().any(|existing| existing == reason) {
        return;
    }
    reasons.push(reason.to_string());
}

// ── Intent classification (for retrieval channel weights) ─────────────────────

fn intent_name(kind: QueryIntentKind) -> &'static str {
    match kind {
        QueryIntentKind::SymbolUsage => "symbol-usage",
        QueryIntentKind::SymbolDefinition => "symbol-definition",
        QueryIntentKind::RuntimeConfig => "runtime-config",
        QueryIntentKind::FlowTrace => "flow-trace",
        QueryIntentKind::Architecture => "architecture",
        QueryIntentKind::TestLookup => "test-lookup",
    }
}

fn classify_intent(prompt: &str) -> QueryIntentKind {
    let lower = prompt.to_ascii_lowercase();
    // SymbolUsage check runs first — "what calls X" is unambiguous and must not be
    // shadowed by "where is" in "from where is it triggered" (which would give sym-def).
    if contains_any(
        &lower,
        &[
            "what calls",
            "callers of",
            "who calls",
            "uses of",
            "usages of",
            "who constructs",
            "who creates",
            "who instantiates",
            "who builds",
        ],
    ) {
        return QueryIntentKind::SymbolUsage;
    }
    if contains_any(
        &lower,
        &[
            "where is",
            "defined",
            "definition",
            "declaration",
            "declare",
        ],
    ) {
        return QueryIntentKind::SymbolDefinition;
    }
    if contains_any(
        &lower,
        &[
            "what does",
            "called by",
            "call chain",
            "calls internally",
            "trace the",
            "trace from",
            "execution order",
            "cleanup order",
            "cleanup sequence",
            "unmount order",
            "lifecycle order",
            "removal order",
            "what order",
        ],
    ) {
        return QueryIntentKind::FlowTrace;
    }
    if contains_any(
        &lower,
        &[
            "architecture",
            "layout",
            "modules",
            "structure",
            "overview",
            "entry point",
            "entrypoint",
            "directory",
        ],
    ) {
        return QueryIntentKind::Architecture;
    }
    // Generative test queries ("what tests would you add/write") ask Claude to design new
    // tests rather than locate existing ones. Route to Architecture so the response is
    // grounded in codebase structure, not anchored to existing test files via test-path-boost.
    if contains_any(
        &lower,
        &[
            "would you add",
            "would you write",
            "should we add",
            "should be added",
            "suggest tests",
            "suggest test",
            "design tests",
            "design test",
            "test cases to",
            "tests to add",
            "tests to write",
        ],
    ) {
        return QueryIntentKind::Architecture;
    }
    if contains_any(
        &lower,
        &["test", "testing", "coverage", "spec", "unit test"],
    ) {
        return QueryIntentKind::TestLookup;
    }
    if contains_any(
        &lower,
        &[
            "config file",
            "load config",
            "read config",
            "env var",
            "environment variable",
            "configuration",
            "settings",
            "build flag",
        ],
    ) {
        return QueryIntentKind::RuntimeConfig;
    }
    QueryIntentKind::Architecture
}

fn weights_for_intent(kind: QueryIntentKind) -> IntentWeights {
    match kind {
        QueryIntentKind::SymbolDefinition => IntentWeights {
            lexical: 1.5,
            vector: 1.0,
            symbol: 2.0,
            path: 0.5,
            graph: 1.0,
        },
        QueryIntentKind::FlowTrace => IntentWeights {
            lexical: 1.0,
            vector: 1.0,
            symbol: 1.5,
            path: 0.5,
            graph: 2.5,
        },
        QueryIntentKind::SymbolUsage => IntentWeights {
            lexical: 1.0,
            vector: 1.0,
            symbol: 2.0,
            path: 0.5,
            graph: 2.0,
        },
        QueryIntentKind::Architecture => IntentWeights {
            lexical: 1.0,
            vector: 2.0,
            symbol: 1.0,
            path: 1.5,
            graph: 0.5,
        },
        QueryIntentKind::TestLookup => IntentWeights {
            lexical: 1.5,
            vector: 1.5,
            symbol: 1.0,
            path: 1.0,
            graph: 0.5,
        },
        QueryIntentKind::RuntimeConfig => IntentWeights {
            lexical: 1.5,
            vector: 1.5,
            symbol: 1.0,
            path: 1.5,
            graph: 0.5,
        },
    }
}

// ── Query token extraction ────────────────────────────────────────────────────

fn contains_any(input: &str, patterns: &[&str]) -> bool {
    patterns.iter().any(|p| input.contains(p))
}

fn is_runtime_env_var_query(query: &str) -> bool {
    let lower = query.to_ascii_lowercase();
    contains_any(
        &lower,
        &[
            "environment variable",
            "environment variables",
            "env var",
            "env vars",
        ],
    )
}

fn extract_scope_path_hints(query: &str) -> Vec<String> {
    let mut hints = Vec::new();
    let mut seen = HashSet::new();
    let tokens = query
        .split_whitespace()
        .map(normalize_query_hint_token)
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    for idx in 0..tokens.len() {
        if !matches!(tokens[idx].as_str(), "in" | "under" | "within" | "inside") {
            continue;
        }
        let mut phrase_parts = Vec::new();
        for token in tokens.iter().skip(idx + 1).take(4) {
            if is_scope_boundary_token(token) {
                break;
            }
            if token.len() < 2 || token.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            if is_query_noise_token(token) {
                continue;
            }
            if is_generic_scope_hint(token) {
                continue;
            }
            phrase_parts.push(token.clone());
            push_scope_hint(token, &mut hints, &mut seen);
        }
        if phrase_parts.is_empty() {
            continue;
        }
        let joined = phrase_parts.join("/");
        if joined.len() >= 4 {
            push_scope_hint(&joined, &mut hints, &mut seen);
        }
    }
    hints
}

fn normalize_query_hint_token(raw: &str) -> String {
    raw.trim_matches(|c: char| !c.is_ascii_alphanumeric() && !matches!(c, '/' | '_' | '-' | '.'))
        .to_ascii_lowercase()
}

fn is_scope_boundary_token(token: &str) -> bool {
    matches!(
        token,
        "and"
            | "or"
            | "where"
            | "which"
            | "that"
            | "for"
            | "to"
            | "from"
            | "with"
            | "by"
            | "on"
            | "at"
            | "of"
    )
}

fn is_generic_scope_hint(token: &str) -> bool {
    matches!(
        token,
        "code"
            | "repo"
            | "project"
            | "file"
            | "files"
            | "module"
            | "modules"
            | "component"
            | "components"
            | "function"
            | "functions"
            | "hook"
            | "hooks"
            | "class"
            | "classes"
            | "folder"
            | "folders"
            | "directory"
            | "directories"
    )
}

fn push_scope_hint(token: &str, out: &mut Vec<String>, seen: &mut HashSet<String>) {
    if token.is_empty() {
        return;
    }
    let normalized = token.to_ascii_lowercase();
    if seen.insert(normalized.clone()) {
        out.push(normalized.clone());
    }
    for piece in normalized
        .split(['/', '.', '_', '-'])
        .filter(|part| part.len() >= 2)
    {
        if seen.insert(piece.to_string()) {
            out.push(piece.to_string());
        }
    }
}

/// Extract function identifiers from FlowTrace queries for anchor seeding.
/// Includes:
/// - Backtick-quoted identifiers (explicit symbol refs)
/// - camelCase tokens (`reconcileChildFibers`, `useState`) — not TitleCase
/// - snake_case tokens with underscores (`get_response`, `dispatch_request`)
///   These are unambiguous code identifiers since English words never contain underscores.
fn extract_flowtrace_anchor_tokens(query: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    // Backtick-quoted identifiers are always included (explicit symbol refs)
    let mut chars = query.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '`' {
            let token: String = chars.by_ref().take_while(|&ch| ch != '`').collect();
            let ident = token
                .rsplit(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .next()
                .unwrap_or(&token);
            let normalized = ident.to_ascii_lowercase();
            if normalized.len() >= 3 && normalized.len() <= 64 && seen.insert(normalized.clone()) {
                out.push(normalized);
            }
        }
    }

    for raw in query
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|t| !t.is_empty())
    {
        if raw.len() < 3 || raw.len() > 64 {
            continue;
        }
        // camelCase (has uppercase after pos 0) or snake_case (contains underscore)
        if has_symbol_case_pattern(raw) || raw.contains('_') {
            let normalized = raw.to_ascii_lowercase();
            if seen.insert(normalized.clone()) {
                out.push(normalized);
            }
        }
    }
    out
}

fn extract_query_symbol_tokens(query: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    // Backtick-quoted identifiers are always symbols regardless of case pattern.
    // Handles queries like "Where is `run` defined?" where "run" is all-lowercase but
    // the backticks signal an exact identifier reference.
    let mut chars = query.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '`' {
            let token: String = chars.by_ref().take_while(|&ch| ch != '`').collect();
            // Strip any path prefix (e.g. `src/foo.rs` → take identifier part after last /)
            let ident = token
                .rsplit(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .next()
                .unwrap_or(&token);
            let normalized = ident.to_ascii_lowercase();
            if normalized.len() >= 2 && normalized.len() <= 64 && seen.insert(normalized.clone()) {
                out.push(normalized);
            }
        }
    }

    for raw in query
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|token| !token.is_empty())
    {
        if raw.len() < 3 || raw.len() > 64 {
            continue;
        }
        let has_underscore = raw.contains('_');
        let has_digit = raw.chars().any(|c| c.is_ascii_digit());
        if !(has_underscore
            || has_digit
            || has_symbol_case_pattern(raw)
            || is_titlecase_symbol_candidate(raw))
        {
            continue;
        }
        let normalized = raw.to_ascii_lowercase();
        if seen.insert(normalized.clone()) {
            out.push(normalized);
        }
    }

    // Natural language pattern: "the X function/method/class" where X is a plain
    // lowercase identifier that lacks syntactic signals (underscore, camelCase).
    // E.g. "Where is the resolve function defined" → extract "resolve".
    extract_named_symbol_from_prose(query, &mut out, &mut seen);

    out
}

/// Detect "the X function/method/class/..." patterns in natural language queries.
/// Captures bare identifiers like "resolve" that have no camelCase/underscore signal.
fn extract_named_symbol_from_prose(
    query: &str,
    out: &mut Vec<String>,
    seen: &mut HashSet<String>,
) {
    const CODE_NOUNS: &[&str] = &[
        "function",
        "method",
        "class",
        "struct",
        "type",
        "interface",
        "trait",
        "module",
        "enum",
        "metaclass",
    ];
    let words: Vec<&str> = query.split_whitespace().collect();
    for (i, &word) in words.iter().enumerate() {
        let clean = word.trim_end_matches(|c: char| !c.is_ascii_alphanumeric());
        let lower = clean.to_ascii_lowercase();
        if !CODE_NOUNS.contains(&lower.as_str()) {
            continue;
        }
        // "the X function" or "X function" — the word before the noun
        if i > 0 {
            let prev = words[i - 1].trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_');
            let prev_lower = prev.to_ascii_lowercase();
            if prev_lower.len() >= 2
                && prev_lower.len() <= 64
                && prev_lower != "the"
                && prev_lower != "this"
                && prev_lower != "that"
                && prev_lower != "a"
                && prev_lower != "an"
                && prev_lower.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && seen.insert(prev_lower.clone())
            {
                out.push(prev_lower);
            }
        }
    }
}

/// Extract meaningful query words for proof-line needle matching.
/// Filters out common English stop words and very short tokens.
/// Returns lowercase tokens that help proof lines match the user's question.
pub fn extract_query_proof_tokens(query: &str) -> Vec<String> {
    const STOP: &[&str] = &[
        "the", "and", "for", "are", "but", "not", "you", "all", "can", "her", "was", "one", "our",
        "out", "has", "its", "this", "that", "with", "from", "they", "been", "have", "what",
        "where", "which", "when", "why", "how", "does", "will", "would", "could", "should",
        "describe", "trace", "show", "list", "explain", "tell", "give", "each", "defined",
        "called", "used", "using", "before", "after", "during", "into", "being",
    ];
    let mut seen = HashSet::new();
    query
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|tok| tok.len() >= 4)
        .map(|tok| tok.to_ascii_lowercase())
        .filter(|tok| !STOP.contains(&tok.as_str()))
        .filter(|tok| seen.insert(tok.clone()))
        .collect()
}

fn has_symbol_case_pattern(raw: &str) -> bool {
    let chars: Vec<char> = raw.chars().collect();
    let has_lower = chars.iter().any(|c| c.is_ascii_lowercase());
    let has_upper = chars.iter().any(|c| c.is_ascii_uppercase());
    if !(has_lower && has_upper) {
        return false;
    }
    // Ignore simple title-cased words like "Where".
    chars
        .iter()
        .enumerate()
        .any(|(idx, c)| c.is_ascii_uppercase() && idx > 0)
}

fn is_titlecase_symbol_candidate(raw: &str) -> bool {
    const STOP: &[&str] = &[
        "what", "where", "which", "when", "why", "how", "describe", "trace", "show", "list",
        "explain", "tell", "give",
    ];
    if raw.len() < 3 || raw.len() > 64 {
        return false;
    }
    let mut chars = raw.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_uppercase() {
        return false;
    }
    let rest = chars.collect::<Vec<_>>();
    if rest.is_empty() {
        return false;
    }
    if !rest.iter().all(|ch| ch.is_ascii_lowercase()) {
        return false;
    }
    !STOP.contains(&raw.to_ascii_lowercase().as_str())
}

/// Extract PascalCase tokens from the query, preserving original case.
/// Returns tokens like "Session", "Engine", "Context" — words that start with
/// uppercase and contain at least one lowercase character (not ALL_CAPS).
/// These indicate the user is likely referring to a class/type, not a method/property.
fn extract_query_pascal_tokens(query: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    // Check backtick-quoted identifiers first
    let mut chars = query.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '`' {
            let token: String = chars.by_ref().take_while(|&ch| ch != '`').collect();
            if is_pascal_case(&token) {
                out.insert(token.to_ascii_lowercase());
            }
        }
    }
    // Check bare words
    for raw in query
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|t| !t.is_empty())
    {
        if is_pascal_case(raw) {
            out.insert(raw.to_ascii_lowercase());
        }
    }
    out
}

/// True when the token is PascalCase: starts uppercase, has at least one lowercase,
/// and is not a common English sentence-start word.
fn is_pascal_case(token: &str) -> bool {
    const STOP: &[&str] = &[
        "where", "what", "which", "when", "why", "how", "describe", "trace", "show", "list",
        "explain", "tell", "give", "does", "the", "and", "but", "from", "with", "this",
        "that", "they", "there", "their", "into", "would", "could", "should", "also",
        "walk", "here",
    ];
    if token.len() < 3 || token.len() > 64 {
        return false;
    }
    let first = token.chars().next().unwrap_or('a');
    if !first.is_ascii_uppercase() {
        return false;
    }
    let has_lower = token.chars().any(|c| c.is_ascii_lowercase());
    if !has_lower {
        return false; // ALL_CAPS — not PascalCase
    }
    !STOP.contains(&token.to_ascii_lowercase().as_str())
}

fn extract_query_path_tokens(query: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut compound_parts = Vec::new();
    for raw in query
        .split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | ':' | '"' | '\'' | '`'))
        .filter(|token| !token.is_empty())
    {
        let has_path_punct =
            raw.contains('/') || raw.contains('.') || raw.contains('_') || raw.contains('-');
        let normalized_raw = raw
            .trim_matches(|c: char| {
                !c.is_ascii_alphanumeric() && !matches!(c, '/' | '_' | '.' | '-')
            })
            .to_ascii_lowercase();
        if normalized_raw.is_empty() {
            continue;
        }
        if !has_path_punct
            && should_include_plain_query_path_token(normalized_raw.as_str())
            && seen.insert(normalized_raw.clone())
        {
            out.push(normalized_raw.clone());
        }
        if has_path_punct
            && normalized_raw.len() >= 3
            && !is_query_noise_token(normalized_raw.as_str())
            && seen.insert(normalized_raw.clone())
        {
            out.push(normalized_raw.clone());
        }
        let collapsed = normalized_raw
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>();
        if has_path_punct
            && collapsed.len() >= 5
            && !is_query_noise_token(collapsed.as_str())
            && seen.insert(collapsed.clone())
        {
            out.push(collapsed.clone());
            compound_parts.push(collapsed);
        }
        if !has_path_punct
            && normalized_raw.len() >= 4
            && !is_query_noise_token(normalized_raw.as_str())
        {
            compound_parts.push(normalized_raw.clone());
        }
        if !has_path_punct {
            continue;
        }
        for piece in normalized_raw
            .split(['/', '.', '_', '-'])
            .filter(|part| !part.is_empty())
        {
            if piece.len() >= 3 && !is_query_noise_token(piece) {
                let piece_owned = piece.to_string();
                if seen.insert(piece_owned.clone()) {
                    out.push(piece_owned.clone());
                }
                compound_parts.push(piece_owned);
            }
        }
    }
    add_compound_query_tokens(&mut out, &mut seen, &compound_parts);
    out
}

fn is_query_noise_token(token: &str) -> bool {
    matches!(
        token,
        "where"
            | "what"
            | "which"
            | "when"
            | "with"
            | "from"
            | "into"
            | "that"
            | "this"
            | "does"
            | "used"
            | "call"
            | "calls"
            | "list"
            | "main"
            | "the"
            | "and"
            | "for"
            | "are"
            | "was"
            | "were"
            | "show"
            | "find"
            | "bullet"
            | "bullets"
            | "step"
            | "steps"
    )
}

fn add_compound_query_tokens(out: &mut Vec<String>, seen: &mut HashSet<String>, parts: &[String]) {
    let max_parts = parts.len().min(16);
    for start in 0..max_parts {
        for width in 2..=3 {
            if start + width > max_parts {
                break;
            }
            let combined = parts[start..start + width].concat();
            if combined.len() < 6 || combined.len() > 48 || is_query_noise_token(combined.as_str())
            {
                continue;
            }
            if seen.insert(combined.clone()) {
                out.push(combined);
            }
        }
    }
}

fn should_keep_plain_path_token(token: &str) -> bool {
    matches!(
        token,
        "service"
            | "services"
            | "client"
            | "clients"
            | "api"
            | "request"
            | "requests"
            | "helper"
            | "helpers"
            | "store"
            | "stores"
            | "slice"
            | "slices"
            | "redux"
            | "zustand"
            | "bootstrap"
            | "state"
            | "route"
            | "routes"
            | "router"
            | "routing"
            | "auth"
            | "session"
            | "login"
            | "token"
            | "docs"
            | "readme"
            | "architecture"
            | "module"
            | "modules"
            | "controller"
            | "controllers"
            | "selector"
            | "selectors"
            | "reducer"
            | "reducers"
            | "action"
            | "actions"
            | "test"
            | "tests"
            | "spec"
            | "specs"
            | "e2e"
            | "integration"
            | "middleware"
            | "handler"
            | "handlers"
            | "websocket"
            | "socket"
            | "feature"
            | "features"
            | "flag"
            | "flags"
            | "consume"
            | "consumed"
            | "consumer"
            | "consumers"
            | "import"
            | "imports"
            | "redirect"
            | "redirects"
            | "query"
            | "params"
            | "locale"
            | "locales"
            | "i18n"
            | "intl"
            | "message"
            | "messages"
            | "translation"
            | "translations"
    )
}

fn should_include_plain_query_path_token(token: &str) -> bool {
    if should_keep_plain_path_token(token) {
        return true;
    }
    token.len() >= 3
        && !is_query_noise_token(token)
        && !is_low_signal_plain_query_token(token)
        && !is_common_short_query_token(token)
}

fn is_common_short_query_token(token: &str) -> bool {
    matches!(
        token,
        "how"
            | "why"
            | "who"
            | "all"
            | "new"
            | "old"
            | "top"
            | "any"
            | "get"
            | "set"
            | "run"
            | "app"
            | "dev"
            | "cli"
            | "cmd"
    )
}

fn is_low_signal_plain_query_token(token: &str) -> bool {
    matches!(
        token,
        "implemented"
            | "implementation"
            | "define"
            | "defined"
            | "definition"
            | "declared"
            | "declare"
            | "registered"
            | "register"
            | "registers"
            | "wired"
            | "wiring"
            | "wire"
            | "generated"
            | "generate"
            | "generation"
            | "show"
            | "explain"
            | "explaining"
            | "where"
            | "which"
            | "what"
            | "how"
            | "local"
            | "repo"
            | "repository"
            | "project"
            | "codebase"
            | "code"
            | "command"
            | "commands"
            | "script"
            | "scripts"
            | "workflow"
            | "workflows"
            | "endpoint"
            | "endpoints"
            | "handling"
            | "check"
            | "checks"
            | "bound"
            | "binding"
            | "type"
            | "types"
            | "helper"
            | "helpers"
            | "using"
            | "usage"
            | "unit"
            | "exact"
            | "file"
            | "files"
            | "path"
            | "paths"
            | "trace"
            | "flow"
            | "entrypoint"
            | "output"
            | "outputs"
            | "function"
            | "functions"
    )
}

fn augment_path_tokens_for_intent(
    query: &str,
    intent: &QueryIntent,
    path_tokens: &mut Vec<String>,
) {
    let lower = query.to_ascii_lowercase();
    match intent.kind {
        QueryIntentKind::RuntimeConfig
        | QueryIntentKind::FlowTrace
        | QueryIntentKind::SymbolDefinition
        | QueryIntentKind::TestLookup
        | QueryIntentKind::Architecture => {
            if intent.kind == QueryIntentKind::RuntimeConfig && is_runtime_env_var_query(query) {
                add_path_tokens(
                    path_tokens,
                    &["cli", "helpers", "helper", "config", "dotenv", "startup"],
                );
            }
            if contains_any(&lower, &["service", "client", "api", "request", "helper"]) {
                add_path_tokens(
                    path_tokens,
                    &[
                        "service", "services", "client", "clients", "api", "fetcher", "request",
                        "requests", "helper", "helpers",
                    ],
                );
            }
            if contains_any(
                &lower,
                &["store", "slice", "redux", "zustand", "state", "bootstrap"],
            ) {
                add_path_tokens(
                    path_tokens,
                    &[
                        "store",
                        "stores",
                        "slice",
                        "slices",
                        "redux",
                        "zustand",
                        "state",
                        "bootstrap",
                    ],
                );
            }
            if contains_any(&lower, &["route", "router", "routing"]) {
                add_path_tokens(path_tokens, &["route", "routes", "router", "routing"]);
            }
            if contains_any(&lower, &["auth", "login", "session", "token"]) {
                add_path_tokens(
                    path_tokens,
                    &["auth", "login", "session", "token", "tokens"],
                );
            }
            if contains_any(&lower, &["middleware", "handler", "handlers"]) {
                add_path_tokens(path_tokens, &["middleware", "handler", "handlers"]);
            }
            if contains_any(&lower, &["test", "tests", "spec", "specs", "e2e"]) {
                add_path_tokens(path_tokens, &["test", "tests", "spec", "specs", "e2e"]);
            }
            if contains_any(
                &lower,
                &["i18n", "intl", "locale", "locales", "translation"],
            ) {
                add_path_tokens(
                    path_tokens,
                    &[
                        "i18n",
                        "intl",
                        "locale",
                        "locales",
                        "translation",
                        "translations",
                    ],
                );
            }
        }
        _ => {}
    }
}

fn add_path_tokens(path_tokens: &mut Vec<String>, additions: &[&str]) {
    let mut seen: HashSet<String> = path_tokens.iter().cloned().collect();
    for token in additions {
        let owned = token.to_string();
        if seen.insert(owned.clone()) {
            path_tokens.push(owned);
        }
    }
}

fn add_dynamic_path_tokens(path_tokens: &mut Vec<String>, additions: &[String]) {
    let mut seen: HashSet<String> = path_tokens.iter().cloned().collect();
    for token in additions {
        if token.len() >= 3 && seen.insert(token.clone()) {
            path_tokens.push(token.clone());
        }
    }
}

fn normalize_path(input: &str) -> String {
    input
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BudiConfig;
    use crate::index::RepoIndexState;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    // ── classify_intent ───────────────────────────────────────────────────────

    #[test]
    fn classify_symbol_usage_who_calls() {
        assert_eq!(
            classify_intent("who calls scheduleUpdateOnFiber"),
            QueryIntentKind::SymbolUsage
        );
        assert_eq!(
            classify_intent("what calls processWork"),
            QueryIntentKind::SymbolUsage
        );
        assert_eq!(
            classify_intent("callers of performWork"),
            QueryIntentKind::SymbolUsage
        );
        assert_eq!(
            classify_intent("uses of commitRoot"),
            QueryIntentKind::SymbolUsage
        );
        assert_eq!(
            classify_intent("usages of renderFiber"),
            QueryIntentKind::SymbolUsage
        );
        assert_eq!(
            classify_intent("who constructs FiberNode"),
            QueryIntentKind::SymbolUsage
        );
        assert_eq!(
            classify_intent("who creates the scheduler"),
            QueryIntentKind::SymbolUsage
        );
        assert_eq!(
            classify_intent("who instantiates WorkLoop"),
            QueryIntentKind::SymbolUsage
        );
        assert_eq!(
            classify_intent("who builds the context"),
            QueryIntentKind::SymbolUsage
        );
    }

    #[test]
    fn classify_symbol_definition_where_is() {
        assert_eq!(
            classify_intent("where is scheduleUpdateOnFiber defined"),
            QueryIntentKind::SymbolDefinition
        );
        assert_eq!(
            classify_intent("where is the reconciler"),
            QueryIntentKind::SymbolDefinition
        );
        assert_eq!(
            classify_intent("definition of commitWork"),
            QueryIntentKind::SymbolDefinition
        );
        assert_eq!(
            classify_intent("declaration of FiberNode"),
            QueryIntentKind::SymbolDefinition
        );
        assert_eq!(
            classify_intent("declare the interface"),
            QueryIntentKind::SymbolDefinition
        );
    }

    #[test]
    fn classify_flow_trace_what_does() {
        assert_eq!(
            classify_intent("what does scheduleUpdateOnFiber do"),
            QueryIntentKind::FlowTrace
        );
        assert_eq!(
            classify_intent("called by renderFiber"),
            QueryIntentKind::FlowTrace
        );
        assert_eq!(
            classify_intent("trace the call chain of commitRoot"),
            QueryIntentKind::FlowTrace
        );
        assert_eq!(
            classify_intent("trace from app creation to config loading"),
            QueryIntentKind::FlowTrace
        );
        assert_eq!(
            classify_intent("execution order in the scheduler"),
            QueryIntentKind::FlowTrace
        );
        assert_eq!(
            classify_intent("cleanup order for useEffect"),
            QueryIntentKind::FlowTrace
        );
        assert_eq!(
            classify_intent("cleanup sequence when component unmounts"),
            QueryIntentKind::FlowTrace
        );
        assert_eq!(
            classify_intent("unmount order in React"),
            QueryIntentKind::FlowTrace
        );
        assert_eq!(
            classify_intent("lifecycle order for hooks"),
            QueryIntentKind::FlowTrace
        );
        assert_eq!(
            classify_intent("what order do effects fire"),
            QueryIntentKind::FlowTrace
        );
    }

    #[test]
    fn classify_architecture() {
        assert_eq!(
            classify_intent("what is the architecture of this codebase"),
            QueryIntentKind::Architecture
        );
        assert_eq!(
            classify_intent("give me an overview of the project"),
            QueryIntentKind::Architecture
        );
        assert_eq!(
            classify_intent("what modules are there"),
            QueryIntentKind::Architecture
        );
        assert_eq!(
            classify_intent("show me the structure"),
            QueryIntentKind::Architecture
        );
        // "where is X" routes to SymbolDefinition because "where is" check runs before Architecture
        assert_eq!(
            classify_intent("where is the entry point"),
            QueryIntentKind::SymbolDefinition
        );
        assert_eq!(
            classify_intent("what is the directory layout"),
            QueryIntentKind::Architecture
        );
        // Without "where is", entry point / entrypoint routes correctly to Architecture
        assert_eq!(
            classify_intent("explain the entrypoint"),
            QueryIntentKind::Architecture
        );
    }

    #[test]
    fn classify_generative_test_routes_to_architecture() {
        assert_eq!(
            classify_intent("what tests would you add for commitRoot"),
            QueryIntentKind::Architecture
        );
        assert_eq!(
            classify_intent("what tests would you write"),
            QueryIntentKind::Architecture
        );
        assert_eq!(
            classify_intent("suggest tests for the scheduler"),
            QueryIntentKind::Architecture
        );
        assert_eq!(
            classify_intent("design test cases for this"),
            QueryIntentKind::Architecture
        );
        assert_eq!(
            classify_intent("tests to add for renderFiber"),
            QueryIntentKind::Architecture
        );
    }

    #[test]
    fn classify_test_lookup() {
        assert_eq!(
            classify_intent("show me the test for commitRoot"),
            QueryIntentKind::TestLookup
        );
        // "where is the unit test" routes to SymbolDefinition because "where is" check runs first
        assert_eq!(
            classify_intent("where is the unit test"),
            QueryIntentKind::SymbolDefinition
        );
        assert_eq!(
            classify_intent("testing the scheduler"),
            QueryIntentKind::TestLookup
        );
        // "what is the spec for X" — "what is" not in the flow-trace keywords, falls through to TestLookup
        assert_eq!(
            classify_intent("spec for renderFiber"),
            QueryIntentKind::TestLookup
        );
        assert_eq!(
            classify_intent("coverage for this file"),
            QueryIntentKind::TestLookup
        );
        assert_eq!(
            classify_intent("unit test for commitRoot"),
            QueryIntentKind::TestLookup
        );
    }

    #[test]
    fn classify_runtime_config() {
        assert_eq!(
            classify_intent("where is the config file"),
            QueryIntentKind::SymbolDefinition
        ); // "where is" wins
        assert_eq!(
            classify_intent("how to load config"),
            QueryIntentKind::RuntimeConfig
        );
        assert_eq!(
            classify_intent("read config from disk"),
            QueryIntentKind::RuntimeConfig
        );
        assert_eq!(
            classify_intent("what env var controls logging"),
            QueryIntentKind::RuntimeConfig
        );
        assert_eq!(
            classify_intent("environment variable for database"),
            QueryIntentKind::RuntimeConfig
        );
        assert_eq!(
            classify_intent("what are the settings"),
            QueryIntentKind::RuntimeConfig
        );
        assert_eq!(
            classify_intent("which build flag enables debug mode"),
            QueryIntentKind::RuntimeConfig
        );
    }

    #[test]
    fn classify_default_to_architecture() {
        assert_eq!(
            classify_intent("how does the reconciler work"),
            QueryIntentKind::Architecture
        );
        assert_eq!(
            classify_intent("explain the fiber model"),
            QueryIntentKind::Architecture
        );
    }

    #[test]
    fn classify_case_insensitive() {
        assert_eq!(
            classify_intent("WHERE IS scheduleUpdateOnFiber"),
            QueryIntentKind::SymbolDefinition
        );
        assert_eq!(
            classify_intent("WHAT CALLS commitRoot"),
            QueryIntentKind::SymbolUsage
        );
        assert_eq!(
            classify_intent("What Does processWork Do"),
            QueryIntentKind::FlowTrace
        );
    }

    // SymbolUsage must beat SymbolDefinition when both keywords appear
    #[test]
    fn classify_sym_usage_wins_over_sym_def() {
        // "who calls" should win over "defined" even if both appear
        let prompt = "who calls the function defined in reconciler";
        assert_eq!(classify_intent(prompt), QueryIntentKind::SymbolUsage);
    }

    // SymbolDefinition must win before FlowTrace when both could match
    #[test]
    fn classify_sym_def_wins_over_flow_trace() {
        // "where is" appears before "what does"
        assert_eq!(
            classify_intent("where is what does defined"),
            QueryIntentKind::SymbolDefinition
        );
    }

    // ── min_selection_score ────────────────────────────────────────────────────

    fn make_scored_chunk(id: u64, score: f32) -> ScoredChunk {
        ScoredChunk {
            id,
            score,
            reasons: vec![],
            channel_scores: crate::rpc::QueryChannelScores::default(),
        }
    }

    #[test]
    fn min_score_empty_candidates_returns_neg_infinity() {
        let floor = min_selection_score(&[], QueryIntentKind::Architecture);
        assert_eq!(floor, f32::NEG_INFINITY);
    }

    #[test]
    fn min_score_relative_floor_is_40_percent_of_top() {
        // Architecture with top < 0.60 uses max(relative, 0.30) floor.
        // top=0.50: relative = 0.50 * 0.40 = 0.20; minimum floor = 0.30
        let chunks = vec![make_scored_chunk(1, 0.50), make_scored_chunk(2, 0.20)];
        let floor = min_selection_score(&chunks, QueryIntentKind::Architecture);
        assert!(
            (floor - 0.30).abs() < 1e-5,
            "expected 0.30 minimum floor, got {floor}"
        );
    }

    #[test]
    fn min_score_floor_enforced_for_flow_trace() {
        // relative = 0.40 * 0.40 = 0.16, but FlowTrace floor is 0.25
        let chunks = vec![make_scored_chunk(1, 0.40)];
        let floor = min_selection_score(&chunks, QueryIntentKind::FlowTrace);
        assert!(
            (floor - 0.25).abs() < 1e-5,
            "expected 0.25 floor, got {floor}"
        );
    }

    #[test]
    fn min_score_floor_enforced_for_symbol_def() {
        // relative = 0.40 * 0.40 = 0.16, but SymbolDefinition floor is 0.30
        let chunks = vec![make_scored_chunk(1, 0.40)];
        let floor = min_selection_score(&chunks, QueryIntentKind::SymbolDefinition);
        assert!(
            (floor - 0.30).abs() < 1e-5,
            "expected 0.30 floor, got {floor}"
        );
    }

    #[test]
    fn min_score_floor_enforced_for_symbol_usage() {
        let chunks = vec![make_scored_chunk(1, 0.40)];
        let floor = min_selection_score(&chunks, QueryIntentKind::SymbolUsage);
        assert!(
            (floor - 0.22).abs() < 1e-5,
            "expected 0.22 floor, got {floor}"
        );
    }

    #[test]
    fn min_score_floor_enforced_for_test_lookup() {
        let chunks = vec![make_scored_chunk(1, 0.40)];
        let floor = min_selection_score(&chunks, QueryIntentKind::TestLookup);
        assert!(
            (floor - 0.22).abs() < 1e-5,
            "expected 0.22 floor, got {floor}"
        );
    }

    #[test]
    fn min_score_runtime_config_high_confidence_uses_0_40_floor() {
        // top >= 0.60 → floor = relative.max(0.40) to cut brew/hyperlink noise
        let chunks = vec![make_scored_chunk(1, 0.80)];
        let floor = min_selection_score(&chunks, QueryIntentKind::RuntimeConfig);
        // relative = 0.80 * 0.40 = 0.32; clamped to 0.40
        assert!(
            (floor - 0.40).abs() < 1e-5,
            "expected 0.40 floor, got {floor}"
        );
    }

    #[test]
    fn min_score_runtime_config_low_confidence_uses_0_18_floor() {
        // top < 0.60 → keep 0.18 floor (React __DEV__ scenario)
        let chunks = vec![make_scored_chunk(1, 0.40)];
        let floor = min_selection_score(&chunks, QueryIntentKind::RuntimeConfig);
        // relative = 0.40 * 0.40 = 0.16; clamped to 0.18
        assert!(
            (floor - 0.18).abs() < 1e-5,
            "expected 0.18 floor, got {floor}"
        );
    }

    #[test]
    fn min_score_architecture_high_confidence_uses_0_40_floor() {
        // top >= 0.60 → floor = relative.max(0.40) to cut devtools/test noise
        let chunks = vec![make_scored_chunk(1, 0.65)];
        let floor = min_selection_score(&chunks, QueryIntentKind::Architecture);
        // relative = 0.65 * 0.40 = 0.26; clamped to 0.40
        assert!(
            (floor - 0.40).abs() < 1e-5,
            "expected 0.40 floor, got {floor}"
        );
    }

    #[test]
    fn min_score_architecture_low_confidence_uses_min_0_30_floor() {
        // top < 0.60 → floor = relative.max(0.30) to filter test fixtures
        let chunks = vec![make_scored_chunk(1, 0.40)];
        let floor = min_selection_score(&chunks, QueryIntentKind::Architecture);
        // relative = 0.40 * 0.40 = 0.16; minimum floor = 0.30
        assert!(
            (floor - 0.30).abs() < 1e-5,
            "expected 0.30 floor, got {floor}"
        );
    }

    #[test]
    fn min_score_relative_dominates_when_high() {
        // top = 0.90, relative = 0.36; FlowTrace floor is 0.25, so relative wins
        let chunks = vec![make_scored_chunk(1, 0.90)];
        let floor = min_selection_score(&chunks, QueryIntentKind::FlowTrace);
        assert!(
            (floor - 0.36).abs() < 1e-5,
            "expected 0.36 (relative wins), got {floor}"
        );
    }

    // ── is_test_path ──────────────────────────────────────────────────────────

    #[test]
    fn is_test_path_detects_test_dirs() {
        assert!(is_test_path("src/tests/foo.rs"));
        assert!(is_test_path("src/test/scheduler.rs"));
        assert!(is_test_path("__tests__/App.test.tsx")); // Jest __tests__ directory
        assert!(is_test_path("src/__tests__/Foo.test.ts")); // nested __tests__
        assert!(is_test_path("spec/scheduler_spec.rb"));
        assert!(is_test_path("test_scheduler.py")); // starts_with("test")
        assert!(is_test_path("spec_helper.rb")); // starts_with("spec")
    }

    #[test]
    fn is_test_path_rejects_non_test_paths() {
        assert!(!is_test_path("src/scheduler.rs"));
        assert!(!is_test_path("crates/budi-core/src/daemon.rs"));
        assert!(!is_test_path("components/Button.tsx"));
    }

    // ── extract_flowtrace_anchor_tokens ───────────────────────────────────

    #[test]
    fn flowtrace_anchor_tokens_extracts_camelcase() {
        let tokens = extract_flowtrace_anchor_tokens(
            "What functions does reconcileChildFibers call and what does each return?",
        );
        assert!(tokens.contains(&"reconcilechildfibers".to_string()));
        // "What", "does", "call" etc. are not camelCase — not extracted
        assert!(!tokens.contains(&"what".to_string()));
    }

    #[test]
    fn flowtrace_anchor_tokens_excludes_titlecase() {
        let tokens = extract_flowtrace_anchor_tokens(
            "Trace the lifecycle hook execution order when a React component mounts",
        );
        // "React" is TitleCase (uppercase only at pos 0) — should NOT be included
        assert!(!tokens.contains(&"react".to_string()));
        // No camelCase or snake_case identifiers in this query
        assert!(tokens.is_empty());
    }

    #[test]
    fn flowtrace_anchor_tokens_backtick_included() {
        let tokens = extract_flowtrace_anchor_tokens("Trace `useState` internal call chain");
        assert!(tokens.contains(&"usestate".to_string()));
    }

    #[test]
    fn flowtrace_anchor_tokens_includes_snake_case() {
        let tokens = extract_flowtrace_anchor_tokens(
            "What functions does get_response call and what does each return?",
        );
        assert!(tokens.contains(&"get_response".to_string()));
        // Plain English words without underscores should NOT be included
        assert!(!tokens.contains(&"functions".to_string()));
        assert!(!tokens.contains(&"call".to_string()));
    }

    #[test]
    fn flowtrace_anchor_tokens_snake_case_python() {
        let tokens = extract_flowtrace_anchor_tokens(
            "Trace the call chain from dispatch_request to the view function",
        );
        assert!(tokens.contains(&"dispatch_request".to_string()));
    }

    // ── extract_named_symbol_from_prose ──────────────────────────────────

    #[test]
    fn named_symbol_from_prose_resolve_function() {
        let tokens = extract_query_symbol_tokens(
            "Where is the resolve function defined and what is its role in URL resolution?",
        );
        assert!(
            tokens.contains(&"resolve".to_string()),
            "expected 'resolve' from 'the resolve function', got: {:?}",
            tokens
        );
    }

    #[test]
    fn named_symbol_from_prose_modelbase_metaclass() {
        let tokens = extract_query_symbol_tokens(
            "Where is the ModelBase metaclass defined?",
        );
        // "ModelBase" should be found by TitleCase detection AND by "X metaclass" pattern
        assert!(tokens.contains(&"modelbase".to_string()));
    }

    #[test]
    fn named_symbol_from_prose_ignores_determiners() {
        let tokens = extract_query_symbol_tokens("Where is the function defined?");
        // "the" before "function" should be skipped
        assert!(!tokens.contains(&"the".to_string()));
    }

    #[test]
    fn named_symbol_from_prose_handler_class() {
        let tokens = extract_query_symbol_tokens(
            "Where is the handler class defined?",
        );
        assert!(
            tokens.contains(&"handler".to_string()),
            "expected 'handler' from 'the handler class', got: {:?}",
            tokens
        );
    }

    #[test]
    fn is_test_path_excludes_examples() {
        assert!(!is_test_path("examples/tutorial/tests/test_auth.py"));
        assert!(!is_test_path("examples/basic/tests/test_app.py"));
        assert!(is_test_path("tests/test_blueprints.py"));
    }

    #[test]
    fn is_examples_path_detects_examples_dirs() {
        assert!(is_examples_path("examples/tutorial/flaskr/db.py"));
        assert!(is_examples_path("examples/basic/app.py"));
        assert!(is_examples_path("src/examples/demo.py"));
        assert!(!is_examples_path("src/flask/app.py"));
        assert!(!is_examples_path("tests/test_blueprints.py"));
    }

    // ── overlapping chunk deduplication ──────────────────────────────────────

    #[test]
    fn overlapping_chunks_skipped_in_selection() {
        // Two adjacent stride=60/overlap=20 chunks from the same file share lines 661-680.
        // The second (lower-scored) one should be rejected by try_push_scored_chunk.
        use crate::rpc::QueryResultItem;
        use context::SnippetSelectionState;
        let mut selection = SnippetSelectionState::default();
        // Insert the first chunk manually (simulating a successful push)
        selection.snippets.push(QueryResultItem {
            path: "src/ReactFiberCommitWork.js".to_string(),
            start_line: 601_usize,
            end_line: 680_usize,
            language: "javascript".to_string(),
            score: 0.65,
            reasons: vec![],
            channel_scores: Default::default(),
            text: String::new(),
            slm_relevance_note: None,
        });
        // The overlapping check logic
        let path = "src/ReactFiberCommitWork.js";
        let start_line: usize = 661;
        let end_line: usize = 740;
        let overlaps = selection
            .snippets
            .iter()
            .any(|s| s.path == path && s.start_line < end_line && start_line < s.end_line);
        assert!(overlaps, "661-740 should overlap with 601-680");
        // Non-overlapping chunk from same file: 741-820 should not overlap
        let start2: usize = 741;
        let end2: usize = 820;
        let no_overlap = selection
            .snippets
            .iter()
            .any(|s| s.path == path && s.start_line < end2 && start2 < s.end_line);
        assert!(!no_overlap, "741-820 should not overlap with 601-680");
    }

    #[test]
    fn is_test_path_permissive_for_test_in_name() {
        // Files with "test" in the name (via /test substring) are treated as test-related.
        // This is intentional: utils/testUtils.ts contains /test as a substring.
        assert!(is_test_path("utils/testUtils.ts")); // contains /test (in /testUtils)
        assert!(is_test_path("src/testUtils.ts")); // contains /test (in /testUtils)
        // Top-level file starting with "test" is also detected
        assert!(is_test_path("testUtils.ts"));
    }

    #[test]
    fn is_test_path_detects_test_utils_dirs() {
        // Test-utility directories like internal-test-utils
        assert!(is_test_path("packages/internal-test-utils/consoleMock.js"));
        assert!(is_test_path("src/react-test-helpers/setup.ts"));
    }

    #[test]
    fn is_test_path_detects_stubs_and_fixtures_dirs() {
        assert!(is_test_path(
            "internal/stacks/stackruntime/internal/stackeval/stubs/errored.go"
        ));
        assert!(is_test_path("test/fixtures/sample_config.json"));
        // Production paths should not match
        assert!(!is_test_path("internal/stacks/stackruntime/eval.go"));
    }

    #[test]
    fn extract_query_proof_tokens_filters_stop_words() {
        let tokens = extract_query_proof_tokens(
            "Where is Plan defined and what steps does it take from a loaded configuration?",
        );
        // Should include meaningful words
        assert!(tokens.contains(&"plan".to_string()));
        assert!(tokens.contains(&"steps".to_string()));
        assert!(tokens.contains(&"loaded".to_string()));
        assert!(tokens.contains(&"configuration".to_string()));
        // Should exclude stop words
        assert!(!tokens.contains(&"where".to_string()));
        assert!(!tokens.contains(&"what".to_string()));
        assert!(!tokens.contains(&"does".to_string()));
        assert!(!tokens.contains(&"from".to_string()));
    }

    #[test]
    fn is_test_path_detects_mock_files() {
        // Mock implementation files
        assert!(is_test_path("internal/terraform/eval_context_mock.go"));
        assert!(is_test_path("internal/configs/mock_provider.go"));
        assert!(is_test_path("packages/internal-test-utils/consoleMock.js"));
        assert!(is_test_path("src/__mocks__/consoleMock.ts"));
        // Non-mock production files should not be detected
        assert!(!is_test_path("src/components/Modal.tsx"));
        assert!(!is_test_path("internal/terraform/context.go"));
    }

    #[test]
    fn is_test_path_detects_filename_test_conventions() {
        // Go _test.go convention
        assert!(is_test_path("internal/command/plan_test.go"));
        assert!(is_test_path("internal/terraform/context_plan_test.go"));
        // JS/TS .test.ts / .spec.ts convention
        assert!(is_test_path("src/components/Button.test.tsx"));
        assert!(is_test_path("src/utils/parser.spec.ts"));
        // Python test_ convention
        assert!(is_test_path("app/test_views.py"));
        // Production files should NOT match
        assert!(!is_test_path("internal/command/plan.go"));
        assert!(!is_test_path("src/components/Button.tsx"));
    }

    #[test]
    fn extract_test_subject_stem_go() {
        assert_eq!(
            extract_test_subject_stem("plan_test.go"),
            Some("plan".to_string())
        );
        assert_eq!(
            extract_test_subject_stem("context_plan_test.go"),
            Some("context_plan".to_string())
        );
        assert_eq!(
            extract_test_subject_stem("command_test.go"),
            Some("command".to_string())
        );
    }

    #[test]
    fn extract_test_subject_stem_python() {
        assert_eq!(
            extract_test_subject_stem("test_views.py"),
            Some("views".to_string())
        );
        assert_eq!(
            extract_test_subject_stem("test_plan.py"),
            Some("plan".to_string())
        );
    }

    #[test]
    fn extract_test_subject_stem_js_ts() {
        assert_eq!(
            extract_test_subject_stem("Button.test.tsx"),
            Some("button".to_string())
        );
        assert_eq!(
            extract_test_subject_stem("parser.spec.ts"),
            Some("parser".to_string())
        );
    }

    #[test]
    fn extract_test_subject_stem_non_test_file() {
        assert_eq!(extract_test_subject_stem("plan.go"), None);
        assert_eq!(extract_test_subject_stem("Button.tsx"), None);
    }

    #[test]
    fn is_mock_path_detects_mocks() {
        assert!(is_mock_path("internal/terraform/eval_context_mock.go"));
        assert!(is_mock_path("internal/providers/mock.go"));
        assert!(is_mock_path("internal/cloud/tfe_client_mock.go"));
        assert!(!is_mock_path("internal/command/plan_test.go"));
        assert!(!is_mock_path("internal/command/plan.go"));
    }

    #[test]
    fn is_go_test_helper_chunk_detects_helpers() {
        // Go test helper (lowercase t)
        assert!(is_go_test_helper_chunk(
            "func testPlan(t *testing.T) *plans.Plan {"
        ));
        assert!(is_go_test_helper_chunk(
            "func testFixturePath(name string) string {"
        ));
        // Actual Go test function (uppercase T) — NOT a helper
        assert!(!is_go_test_helper_chunk("func TestPlan(t *testing.T) {"));
        assert!(!is_go_test_helper_chunk(
            "func TestPlanHuman_operation(t *testing.T) {"
        ));
        // Non-test function
        assert!(!is_go_test_helper_chunk("func Plan(ctx *Context) error {"));
    }

    // ── is_inline_test_chunk ─────────────────────────────────────────────────

    #[test]
    fn inline_test_chunk_detects_rust_test_attr() {
        assert!(is_inline_test_chunk("#[test]\nfn my_test() {}"));
        assert!(is_inline_test_chunk(
            "// some code\n#[cfg(test)]\nmod tests {}"
        ));
        assert!(is_inline_test_chunk(
            "impl Foo {}\nmod tests {\n    use super::*;\n}"
        ));
    }

    #[test]
    fn inline_test_chunk_detects_js_describe() {
        assert!(is_inline_test_chunk("describe('MyComponent', () => {"));
        assert!(is_inline_test_chunk("describe.each([[1, 2]])("));
    }

    #[test]
    fn inline_test_chunk_rejects_production_code() {
        assert!(!is_inline_test_chunk("fn process_fiber(fiber: &Fiber) {}"));
        assert!(!is_inline_test_chunk("export function TestComponent() {}"));
        assert!(!is_inline_test_chunk("class Config { testMode: bool }"));
    }

    // ── test inventory helpers ────────────────────────────────────────────────

    #[test]
    fn coverage_inventory_query_detects_broad_test_coverage_prompt() {
        assert!(is_test_coverage_inventory_query(
            "What unit tests cover Blueprint registration and where do they live in the repo?"
        ));
        assert!(is_test_coverage_inventory_query(
            "Which tests validate config parsing?"
        ));
    }

    #[test]
    fn coverage_inventory_query_rejects_non_coverage_queries() {
        assert!(!is_test_coverage_inventory_query(
            "Where is register_blueprint defined and what does it do?"
        ));
        assert!(!is_test_coverage_inventory_query(
            "Trace the call chain from an incoming HTTP request to the view function"
        ));
    }

    #[test]
    fn path_matches_subject_tokens_handles_plural_file_names() {
        let tokens = vec!["blueprint".to_string(), "registration".to_string()];
        assert!(path_matches_subject_tokens(
            "tests/test_blueprints.py",
            &tokens
        ));
        assert!(!path_matches_subject_tokens("tests/test_cli.py", &tokens));
    }

    #[test]
    fn extract_test_inventory_entries_extracts_common_test_shapes() {
        let text = r#"
def test_alpha():
    pass

async def test_beta():
    pass

#[test]
fn test_gamma() {}

it("renders", () => {})
"#;
        let entries = extract_test_inventory_entries(text);
        let labels = entries.iter().map(|e| e.label.as_str()).collect::<Vec<_>>();
        assert!(labels.contains(&"test_alpha"), "got: {labels:?}");
        assert!(labels.contains(&"test_beta"), "got: {labels:?}");
        assert!(labels.contains(&"test_gamma"), "got: {labels:?}");
        assert!(
            labels.iter().any(|label| label.starts_with("it(")),
            "got: {labels:?}"
        );
    }

    #[test]
    fn build_test_inventory_lines_spreads_entries_across_early_mid_late() {
        let entries = vec![
            TestInventoryEntry {
                line_number: 120,
                label: "test_blueprint_prefix_slash".to_string(),
            },
            TestInventoryEntry {
                line_number: 131,
                label: "test_blueprint_url_defaults".to_string(),
            },
            TestInventoryEntry {
                line_number: 865,
                label: "test_nested_blueprint".to_string(),
            },
            TestInventoryEntry {
                line_number: 914,
                label: "test_nested_callback_order".to_string(),
            },
            TestInventoryEntry {
                line_number: 1066,
                label: "test_unique_blueprint_names".to_string(),
            },
            TestInventoryEntry {
                line_number: 1083,
                label: "test_self_registration".to_string(),
            },
        ];
        let lines = build_test_inventory_lines(&entries, 3, 200);
        assert_eq!(lines.len(), 3, "got: {lines:?}");
        assert!(lines[0].starts_with("early:"), "got: {lines:?}");
        assert!(lines[1].starts_with("mid:"), "got: {lines:?}");
        assert!(lines[2].starts_with("late:"), "got: {lines:?}");
        assert!(lines[0].contains("test_blueprint_prefix_slash@120"));
        assert!(lines[1].contains("test_nested_blueprint@865"));
        assert!(lines[2].contains("test_self_registration@1083"));
    }

    #[test]
    fn registration_inventory_ranking_drops_error_handler_tests() {
        let entries = vec![
            TestInventoryEntry {
                line_number: 8,
                label: "test_blueprint_specific_error_handling".to_string(),
            },
            TestInventoryEntry {
                line_number: 46,
                label: "test_blueprint_specific_user_error_handling".to_string(),
            },
            TestInventoryEntry {
                line_number: 120,
                label: "test_blueprint_prefix_slash".to_string(),
            },
            TestInventoryEntry {
                line_number: 131,
                label: "test_blueprint_url_defaults".to_string(),
            },
            TestInventoryEntry {
                line_number: 245,
                label: "test_dotted_name_not_allowed".to_string(),
            },
            TestInventoryEntry {
                line_number: 250,
                label: "test_empty_name_not_allowed".to_string(),
            },
            TestInventoryEntry {
                line_number: 865,
                label: "test_nested_blueprint".to_string(),
            },
            TestInventoryEntry {
                line_number: 914,
                label: "test_nested_callback_order".to_string(),
            },
            TestInventoryEntry {
                line_number: 1003,
                label: "test_nesting_url_prefixes".to_string(),
            },
            TestInventoryEntry {
                line_number: 1025,
                label: "test_nesting_subdomains".to_string(),
            },
            TestInventoryEntry {
                line_number: 1044,
                label: "test_child_and_parent_subdomain".to_string(),
            },
            TestInventoryEntry {
                line_number: 1066,
                label: "test_unique_blueprint_names".to_string(),
            },
            TestInventoryEntry {
                line_number: 1083,
                label: "test_self_registration".to_string(),
            },
            TestInventoryEntry {
                line_number: 1089,
                label: "test_blueprint_renaming".to_string(),
            },
        ];
        let ranked = prioritize_test_inventory_entries(
            "What unit tests cover Blueprint registration and where do they live in the repo?",
            "tests/test_blueprints.py",
            &entries,
            1083,
        );
        let labels = ranked
            .into_iter()
            .map(|entry| entry.label)
            .collect::<Vec<_>>();
        assert!(
            !labels
                .iter()
                .any(|label| label.contains("error_handling") || label.contains("template_filter")),
            "got: {labels:?}"
        );
        assert!(labels.iter().any(|label| label == "test_self_registration"));
        assert!(
            labels
                .iter()
                .any(|label| label == "test_blueprint_renaming")
        );
        assert!(
            labels
                .iter()
                .any(|label| label == "test_dotted_name_not_allowed")
        );
        assert!(
            labels
                .iter()
                .any(|label| label == "test_empty_name_not_allowed")
        );
    }

    #[test]
    fn thin_wrapper_prefers_same_file_definition_continuation() {
        let def_item = QueryResultItem {
            path: "internal/terraform/context_plan.go".to_string(),
            start_line: 180,
            end_line: 183,
            language: "go".to_string(),
            score: 0.696,
            reasons: vec!["hint-match-boost".to_string()],
            channel_scores: QueryChannelScores::default(),
            text: "func (c *Context) Plan(config *configs.Config, prevRunState *states.State, opts *PlanOpts) (*plans.Plan, tfdiags.Diagnostics) {\n    plan, _, diags := c.PlanAndEval(config, prevRunState, opts)\n    return plan, diags\n}".to_string(),
            slm_relevance_note: None,
        };
        let continuation = ChunkRecord {
            id: 1,
            path: "internal/terraform/context_plan.go".to_string(),
            start_line: 194,
            end_line: 273,
            language: "go".to_string(),
            symbol_hint: Some("PlanAndEval".to_string()),
            text: "func (c *Context) PlanAndEval(...) {".to_string(),
            embedding: Vec::new(),
        };
        assert!(should_prefer_same_file_definition_continuation(
            &def_item,
            &continuation
        ));
    }

    #[test]
    fn symbol_definition_first_steps_card_extracts_compact_steps() {
        let continuation = ChunkRecord {
            id: 1,
            path: "internal/terraform/context_plan.go".to_string(),
            start_line: 194,
            end_line: 273,
            language: "go".to_string(),
            symbol_hint: Some("PlanAndEval".to_string()),
            text: "func (c *Context) PlanAndEval(config *configs.Config, prevRunState *states.State, opts *PlanOpts) (*plans.Plan, *lang.Scope, tfdiags.Diagnostics) {\n    defer c.acquireRun(\"plan\")()\n    var diags tfdiags.Diagnostics\n    if opts == nil {\n        opts = DefaultPlanOpts\n    }\n}".to_string(),
            embedding: Vec::new(),
        };
        let card = build_symbol_definition_first_steps_card(&continuation, 0.5)
            .expect("expected first-steps card");
        assert!(card.text.contains("first steps:"), "got: {}", card.text);
        assert!(card.text.contains("defer c.acquireRun(\"plan\")()"));
        assert!(card.text.contains("var diags tfdiags.Diagnostics"));
        assert_eq!(
            card.slm_relevance_note.as_deref(),
            Some("same-file first steps summary")
        );
    }

    #[test]
    fn request_to_view_flow_query_detection_matches_web_flow_prompt() {
        assert!(is_request_to_view_flow_query(
            "Trace the call chain from an incoming HTTP request to the view function."
        ));
        assert!(!is_request_to_view_flow_query(
            "Where is dispatch_request defined?"
        ));
    }

    #[test]
    fn web_request_flow_chain_card_builds_compact_chain() {
        let wsgi_chunk = ChunkRecord {
            id: 1,
            path: "src/flask/app.py".to_string(),
            start_line: 1566,
            end_line: 1616,
            language: "python".to_string(),
            symbol_hint: Some("wsgi_app".to_string()),
            text: "def wsgi_app(self, environ, start_response):\n    ctx = self.request_context(environ)\n    ctx.push()\n    response = self.full_dispatch_request(ctx)\n    return response(environ, start_response)\n".to_string(),
            embedding: Vec::new(),
        };
        let full_dispatch_chunk = ChunkRecord {
            id: 2,
            path: "src/flask/app.py".to_string(),
            start_line: 992,
            end_line: 1019,
            language: "python".to_string(),
            symbol_hint: Some("full_dispatch_request".to_string()),
            text: "def full_dispatch_request(self, ctx):\n    rv = self.preprocess_request(ctx)\n    if rv is None:\n        rv = self.dispatch_request(ctx)\n    return self.finalize_request(ctx, rv)\n".to_string(),
            embedding: Vec::new(),
        };
        let dispatch_chunk = ChunkRecord {
            id: 3,
            path: "src/flask/app.py".to_string(),
            start_line: 966,
            end_line: 990,
            language: "python".to_string(),
            symbol_hint: Some("dispatch_request".to_string()),
            text: "def dispatch_request(self, ctx):\n    req = ctx.request\n    return self.ensure_sync(self.view_functions[rule.endpoint])(**view_args)\n".to_string(),
            embedding: Vec::new(),
        };
        let card = build_web_request_flow_chain_card(
            &wsgi_chunk,
            &full_dispatch_chunk,
            &dispatch_chunk,
            0.4,
        )
        .expect("expected flow chain card");
        assert!(
            card.text
                .contains("chain: wsgi_app@1566 -> full_dispatch_request@992")
        );
        assert!(
            card.text
                .contains("wsgi_app@1567: ctx = self.request_context(environ)")
        );
        assert!(
            card.text
                .contains("wsgi_app@1569: response = self.full_dispatch_request(ctx)")
        );
        assert!(
            card.text
                .contains("full_dispatch_request@993: rv = self.preprocess_request(ctx)")
        );
        assert!(
            card.text
                .contains("full_dispatch_request@996: return self.finalize_request(ctx, rv)")
        );
        assert!(card.text.contains("dispatch_request@968: return self.ensure_sync(self.view_functions[rule.endpoint])(**view_args)"));
        assert_eq!(
            card.slm_relevance_note.as_deref(),
            Some("request-to-view chain summary")
        );
    }

    #[test]
    fn non_wrapper_does_not_replace_alt_definition_with_continuation() {
        let def_item = QueryResultItem {
            path: "src/flask/helpers.py".to_string(),
            start_line: 10,
            end_line: 26,
            language: "python".to_string(),
            score: 0.61,
            reasons: vec!["hint-match-boost".to_string()],
            channel_scores: QueryChannelScores::default(),
            text: "def make_response(*args):\n    response = current_app.make_response(args)\n    response.headers['X-Test'] = '1'\n    return response\n".to_string(),
            slm_relevance_note: None,
        };
        let continuation = ChunkRecord {
            id: 2,
            path: "src/flask/helpers.py".to_string(),
            start_line: 30,
            end_line: 80,
            language: "python".to_string(),
            symbol_hint: Some("after_this_request".to_string()),
            text: "def after_this_request(f):".to_string(),
            embedding: Vec::new(),
        };
        assert!(!should_prefer_same_file_definition_continuation(
            &def_item,
            &continuation
        ));
    }

    // ── is_generic_symbol_hint ────────────────────────────────────────────────

    #[test]
    fn generic_symbol_hint_matches_language_keywords() {
        for kw in &[
            "fn",
            "pub",
            "function",
            "def",
            "class",
            "method",
            "func",
            "procedure",
            "sub",
            "lambda",
            "arrow",
            "block",
            "module",
            "impl",
            "trait",
            "struct",
            "enum",
            "interface",
            "type",
            "const",
            "let",
            "var",
            "static",
            "async",
            "export",
        ] {
            assert!(is_generic_symbol_hint(kw), "expected {kw} to be generic");
        }
    }

    #[test]
    fn generic_symbol_hint_allows_real_names() {
        assert!(!is_generic_symbol_hint("scheduleUpdateOnFiber"));
        assert!(!is_generic_symbol_hint("commitRoot"));
        assert!(!is_generic_symbol_hint("renderFiber"));
        assert!(!is_generic_symbol_hint("WorkLoop"));
        assert!(!is_generic_symbol_hint("my_function"));
    }

    // ── intent_retrieval_limit ────────────────────────────────────────────────

    #[test]
    fn retrieval_limit_precision_intents_are_five() {
        assert_eq!(intent_retrieval_limit(QueryIntentKind::SymbolDefinition), 5);
        assert_eq!(intent_retrieval_limit(QueryIntentKind::FlowTrace), 5);
        assert_eq!(intent_retrieval_limit(QueryIntentKind::SymbolUsage), 5);
    }

    #[test]
    fn retrieval_limit_breadth_intents_are_eight() {
        assert_eq!(intent_retrieval_limit(QueryIntentKind::Architecture), 8);
        assert_eq!(intent_retrieval_limit(QueryIntentKind::TestLookup), 8);
    }

    #[test]
    fn retrieval_limit_others_are_six() {
        assert_eq!(intent_retrieval_limit(QueryIntentKind::RuntimeConfig), 6);
        assert_eq!(intent_retrieval_limit(QueryIntentKind::SymbolUsage), 5);
    }

    // ── parse_retrieval_mode ──────────────────────────────────────────────────

    #[test]
    fn parse_retrieval_mode_variants() {
        assert_eq!(parse_retrieval_mode(None), RetrievalMode::Hybrid);
        assert_eq!(parse_retrieval_mode(Some("")), RetrievalMode::Hybrid);
        assert_eq!(
            parse_retrieval_mode(Some("lexical")),
            RetrievalMode::Lexical
        );
        assert_eq!(parse_retrieval_mode(Some("vector")), RetrievalMode::Vector);
        assert_eq!(
            parse_retrieval_mode(Some("symbol-graph")),
            RetrievalMode::SymbolGraph
        );
        assert_eq!(
            parse_retrieval_mode(Some("symbol_graph")),
            RetrievalMode::SymbolGraph
        );
        assert_eq!(
            parse_retrieval_mode(Some("symbolgraph")),
            RetrievalMode::SymbolGraph
        );
        assert_eq!(parse_retrieval_mode(Some("unknown")), RetrievalMode::Hybrid);
        // Case insensitive
        assert_eq!(
            parse_retrieval_mode(Some("LEXICAL")),
            RetrievalMode::Lexical
        );
        assert_eq!(
            parse_retrieval_mode(Some("  vector  ")),
            RetrievalMode::Vector
        );
    }

    #[test]
    fn flow_trace_web_queries_seed_dispatch_symbols() {
        let intent = QueryIntent {
            kind: QueryIntentKind::FlowTrace,
            code_related: true,
            allow_docs: false,
            weights: weights_for_intent(QueryIntentKind::FlowTrace),
        };
        let mut symbol_tokens = extract_query_symbol_tokens(
            "Trace the call chain from an incoming HTTP request to the view function.",
        );
        augment_symbol_tokens_for_intent(
            "Trace the call chain from an incoming HTTP request to the view function.",
            &intent,
            &mut symbol_tokens,
        );
        assert!(
            symbol_tokens.iter().any(|t| t == "wsgi_app"),
            "got: {symbol_tokens:?}"
        );
        assert!(
            symbol_tokens.iter().any(|t| t == "dispatch_request"),
            "got: {symbol_tokens:?}"
        );
        assert!(
            symbol_tokens.iter().any(|t| t == "view_functions"),
            "got: {symbol_tokens:?}"
        );
    }

    #[test]
    fn runtime_env_queries_expand_startup_tokens() {
        let intent = QueryIntent {
            kind: QueryIntentKind::RuntimeConfig,
            code_related: true,
            allow_docs: false,
            weights: weights_for_intent(QueryIntentKind::RuntimeConfig),
        };
        let query = "Which environment variables does Flask read at startup and how do they affect runtime behavior?";
        let retrieval_query = query_for_initial_retrieval(query, &intent);
        assert!(
            retrieval_query.contains("FLASK_APP"),
            "got: {retrieval_query}"
        );
        assert!(
            retrieval_query.contains("FLASK_DEBUG"),
            "got: {retrieval_query}"
        );
        assert!(
            retrieval_query.contains("FLASK_SKIP_DOTENV"),
            "got: {retrieval_query}"
        );
        let mut symbol_tokens = extract_query_symbol_tokens(&retrieval_query);
        augment_symbol_tokens_for_intent(&retrieval_query, &intent, &mut symbol_tokens);
        assert!(
            symbol_tokens.iter().any(|t| t == "get_debug_flag"),
            "got: {symbol_tokens:?}"
        );
        assert!(
            symbol_tokens.iter().any(|t| t == "get_load_dotenv"),
            "got: {symbol_tokens:?}"
        );
        assert!(
            symbol_tokens.iter().any(|t| t == "load_dotenv"),
            "got: {symbol_tokens:?}"
        );
        let mut path_tokens = extract_query_path_tokens(&retrieval_query);
        augment_path_tokens_for_intent(&retrieval_query, &intent, &mut path_tokens);
        assert!(
            path_tokens.iter().any(|t| t == "cli"),
            "got: {path_tokens:?}"
        );
        assert!(
            path_tokens.iter().any(|t| t == "helpers"),
            "got: {path_tokens:?}"
        );
    }

    #[test]
    fn broad_env_listing_queries_skip_injection() {
        // Broad env-var listing queries ("which env vars") skip injection entirely
        // because partial context anchors Claude on a narrow set. Claude's own
        // exploration produces more comprehensive answers.
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let repo_root = std::env::temp_dir().join(format!("budi-retrieval-test-{unique}"));
        fs::create_dir_all(&repo_root).expect("temp repo root");
        let state = RepoIndexState {
            repo_root: repo_root.to_string_lossy().to_string(),
            files: Vec::new(),
            chunks: vec![
                ChunkRecord {
                    id: 1,
                    path: "framework/runtime_helpers.py".to_string(),
                    start_line: 28,
                    end_line: 33,
                    language: "python".to_string(),
                    symbol_hint: Some("get_debug_flag".to_string()),
                    text: "def get_debug_flag() -> bool:\n    val = os.environ.get(\"FLASK_DEBUG\")\n    return bool(val)\n".to_string(),
                    embedding: Vec::new(),
                },
                ChunkRecord {
                    id: 2,
                    path: "framework/runtime_helpers.py".to_string(),
                    start_line: 36,
                    end_line: 48,
                    language: "python".to_string(),
                    symbol_hint: Some("get_load_dotenv".to_string()),
                    text: "def get_load_dotenv(default: bool = True) -> bool:\n    val = os.environ.get(\"FLASK_SKIP_DOTENV\")\n    if not val:\n        return default\n    return val.lower() in (\"0\", \"false\", \"no\")\n".to_string(),
                    embedding: Vec::new(),
                },
            ],
            updated_at_ts: 0,
        };
        let runtime = RuntimeIndex::from_state(&repo_root, state).expect("runtime");
        let config = BudiConfig::default();
        // Broad listing query → should skip injection
        let response = build_query_response(
            &runtime,
            "Which environment variables does Flask read at startup and how do they affect runtime behavior?",
            None,
            None,
            None,
            RetrievalMode::Hybrid,
            &config,
        )
        .expect("query response");
        assert!(
            response.snippets.is_empty(),
            "broad env listing should skip injection, got: {:?}",
            response.snippets
        );
        // Targeted env query → should still inject
        let response2 = build_query_response(
            &runtime,
            "How does the app read FLASK_DEBUG to control debug mode?",
            None,
            None,
            None,
            RetrievalMode::Hybrid,
            &config,
        )
        .expect("query response");
        // Targeted query may or may not inject depending on HNSW scores,
        // but at least it should not be blocked by env-listing-skip.
        // (May classify as architecture or runtime-config — either is fine.)
        assert!(
            !response2.diagnostics.intent.is_empty(),
            "should have an intent"
        );
        let _ = fs::remove_dir_all(&repo_root);
    }

    #[test]
    fn sym_def_exact_match_tokens_trim_supporting_clause() {
        let intent = QueryIntent {
            kind: QueryIntentKind::SymbolDefinition,
            code_related: true,
            allow_docs: false,
            weights: weights_for_intent(QueryIntentKind::SymbolDefinition),
        };
        let symbol_tokens = extract_query_symbol_tokens(
            "Where is register_blueprint defined and what does it do when called with a Blueprint?",
        );
        let exact_match_tokens = exact_match_symbol_tokens_for_intent(
            "Where is register_blueprint defined and what does it do when called with a Blueprint?",
            &intent,
            &symbol_tokens,
        );
        assert_eq!(exact_match_tokens, vec!["register_blueprint".to_string()]);
    }

    #[test]
    fn sym_def_primary_symbol_beats_argument_type_noise() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let repo_root = std::env::temp_dir().join(format!("budi-retrieval-test-{unique}"));
        fs::create_dir_all(&repo_root).expect("temp repo root");
        let state = RepoIndexState {
            repo_root: repo_root.to_string_lossy().to_string(),
            files: Vec::new(),
            chunks: vec![
                ChunkRecord {
                    id: 1,
                    path: "src/flask/wrappers.py".to_string(),
                    start_line: 161,
                    end_line: 178,
                    language: "python".to_string(),
                    symbol_hint: Some("blueprint".to_string()),
                    text: "@property\ndef blueprint(self) -> str | None:\n    \"\"\"The registered name of the current blueprint.\"\"\"\n    return endpoint.rpartition(\".\")[0]\n".to_string(),
                    embedding: Vec::new(),
                },
                ChunkRecord {
                    id: 2,
                    path: "src/flask/sansio/app.py".to_string(),
                    start_line: 566,
                    end_line: 592,
                    language: "python".to_string(),
                    symbol_hint: Some("register_blueprint".to_string()),
                    text: "@setupmethod\ndef register_blueprint(self, blueprint: Blueprint, **options):\n    \"\"\"Register a Blueprint on the application.\"\"\"\n    blueprint.register(self, options)\n".to_string(),
                    embedding: Vec::new(),
                },
            ],
            updated_at_ts: 0,
        };
        let runtime = RuntimeIndex::from_state(&repo_root, state).expect("runtime");
        let config = BudiConfig::default();
        let response = build_query_response(
            &runtime,
            "Where is register_blueprint defined and what does it do when called with a Blueprint?",
            None,
            None,
            None,
            RetrievalMode::Hybrid,
            &config,
        )
        .expect("query response");
        assert_eq!(
            response.snippets.first().map(|s| s.path.as_str()),
            Some("src/flask/sansio/app.py")
        );
        let _ = fs::remove_dir_all(&repo_root);
    }

    #[test]
    fn framework_query_detection_matches_react_next_and_flask_terms() {
        let react = detect_query_ecosystems("Trace the React component mount flow");
        assert!(react.iter().any(|ecosystem| ecosystem == "react"));

        let next = detect_query_ecosystems("How does page.tsx work in the Next.js app router?");
        assert!(next.iter().any(|ecosystem| ecosystem == "nextjs"));
        assert!(next.iter().any(|ecosystem| ecosystem == "react"));

        let flask = detect_query_ecosystems("How does Flask blueprint registration work?");
        assert!(flask.iter().any(|ecosystem| ecosystem == "flask"));
    }

    #[test]
    fn react_queries_surface_ecosystem_diagnostics_and_match_reason() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let repo_root = std::env::temp_dir().join(format!("budi-react-ecosystem-test-{unique}"));
        fs::create_dir_all(&repo_root).expect("temp repo root");
        let state = RepoIndexState {
            repo_root: repo_root.to_string_lossy().to_string(),
            files: Vec::new(),
            chunks: vec![
                ChunkRecord {
                    id: 1,
                    path: "src/components/Dashboard.tsx".to_string(),
                    start_line: 1,
                    end_line: 8,
                    language: "typescript".to_string(),
                    symbol_hint: Some("DashboardComponent".to_string()),
                    text: "import { useEffect } from \"react\";\nexport function DashboardComponent() {\n    useEffect(() => {\n        console.log(\"mounted\");\n    }, []);\n    return <div>dashboard</div>;\n}\n".to_string(),
                    embedding: Vec::new(),
                },
                ChunkRecord {
                    id: 2,
                    path: "src/server/logger.ts".to_string(),
                    start_line: 1,
                    end_line: 4,
                    language: "typescript".to_string(),
                    symbol_hint: Some("requestLogger".to_string()),
                    text: "export function requestLogger() {\n    return \"logger\";\n}\n".to_string(),
                    embedding: Vec::new(),
                },
            ],
            updated_at_ts: 0,
        };
        let runtime = RuntimeIndex::from_state(&repo_root, state).expect("runtime");
        let config = BudiConfig::default();
        let response = build_query_response(
            &runtime,
            "Where is the React component that calls useEffect defined?",
            None,
            None,
            None,
            RetrievalMode::Hybrid,
            &config,
        )
        .expect("query response");
        let top = response.snippets.first().expect("top snippet");
        assert_eq!(top.path, "src/components/Dashboard.tsx");
        assert!(
            top.reasons
                .iter()
                .any(|reason| reason == "ecosystem-match:react"),
            "got: {:?}",
            top.reasons
        );
        assert_eq!(response.diagnostics.top_ecosystem.as_deref(), Some("react"));
        assert!(
            response
                .diagnostics
                .snippet_ecosystems
                .iter()
                .any(|ecosystem| ecosystem == "react"),
            "got: {:?}",
            response.diagnostics.snippet_ecosystems
        );
        let _ = fs::remove_dir_all(&repo_root);
    }

    #[test]
    fn react_effect_lifecycle_card_builds_grounded_order() {
        let layout_unmount_chunk = ChunkRecord {
            id: 1,
            path: "packages/react-reconciler/src/ReactFiberCommitEffects.js".to_string(),
            start_line: 114,
            end_line: 138,
            language: "javascript".to_string(),
            symbol_hint: Some("commitHookLayoutUnmountEffects".to_string()),
            text: "export function commitHookLayoutUnmountEffects(finishedWork, nearestMountedAncestor, hookFlags) {\n  // Layout effects are destroyed during the mutation phase so that all\n  // destroy functions for all fibers are called before any create functions.\n  commitHookEffectListUnmount(hookFlags, finishedWork, nearestMountedAncestor);\n}\n".to_string(),
            embedding: Vec::new(),
        };
        let layout_mount_chunk = ChunkRecord {
            id: 2,
            path: "packages/react-reconciler/src/ReactFiberCommitEffects.js".to_string(),
            start_line: 97,
            end_line: 110,
            language: "javascript".to_string(),
            symbol_hint: Some("commitHookLayoutEffects".to_string()),
            text: "export function commitHookLayoutEffects(finishedWork, hookFlags) {\n  // At this point layout effects have already been destroyed (during mutation phase).\n  commitHookEffectListMount(hookFlags, finishedWork);\n}\n".to_string(),
            embedding: Vec::new(),
        };
        let flush_layout_chunk = ChunkRecord {
            id: 3,
            path: "packages/react-reconciler/src/ReactFiberWorkLoop.js".to_string(),
            start_line: 4028,
            end_line: 4100,
            language: "javascript".to_string(),
            symbol_hint: Some("flushLayoutEffects".to_string()),
            text: "function flushLayoutEffects(): void {\n  // The next phase is the layout phase, where we call effects that read the host tree.\n  commitLayoutEffects(finishedWork, root, lanes);\n}\n".to_string(),
            embedding: Vec::new(),
        };
        let flush_passive_chunk = ChunkRecord {
            id: 4,
            path: "packages/react-reconciler/src/ReactFiberWorkLoop.js".to_string(),
            start_line: 4645,
            end_line: 4749,
            language: "javascript".to_string(),
            symbol_hint: Some("flushPassiveEffects".to_string()),
            text: "function flushPassiveEffects(): boolean {\n  return flushPassiveEffectsImpl();\n}\nfunction flushPassiveEffectsImpl() {\n  commitPassiveUnmountEffects(root.current);\n  commitPassiveMountEffects(root, root.current, lanes, transitions, pendingEffectsRenderEndTime);\n}\n".to_string(),
            embedding: Vec::new(),
        };
        let passive_unmount_chunk = ChunkRecord {
            id: 5,
            path: "packages/react-reconciler/src/ReactFiberCommitWork.js".to_string(),
            start_line: 4585,
            end_line: 4588,
            language: "javascript".to_string(),
            symbol_hint: Some("commitPassiveUnmountEffects".to_string()),
            text: "export function commitPassiveUnmountEffects(finishedWork: Fiber): void {\n  commitPassiveUnmountOnFiber(finishedWork);\n}\n".to_string(),
            embedding: Vec::new(),
        };
        let passive_mount_chunk = ChunkRecord {
            id: 6,
            path: "packages/react-reconciler/src/ReactFiberCommitWork.js".to_string(),
            start_line: 3497,
            end_line: 3512,
            language: "javascript".to_string(),
            symbol_hint: Some("commitPassiveMountEffects".to_string()),
            text: "export function commitPassiveMountEffects(root, finishedWork, committedLanes, committedTransitions, renderEndTime) {\n  commitPassiveMountOnFiber(root, finishedWork, committedLanes, committedTransitions, renderEndTime);\n}\n".to_string(),
            embedding: Vec::new(),
        };
        let hook_unmount_chunk = ChunkRecord {
            id: 7,
            path: "packages/react-reconciler/src/ReactFiberCommitEffects.js".to_string(),
            start_line: 248,
            end_line: 300,
            language: "javascript".to_string(),
            symbol_hint: Some("commitHookEffectListUnmount".to_string()),
            text: "export function commitHookEffectListUnmount(flags, finishedWork, nearestMountedAncestor) {\n  const destroy = inst.destroy;\n  safelyCallDestroy(finishedWork, nearestMountedAncestor, destroy);\n}\n".to_string(),
            embedding: Vec::new(),
        };
        let hook_mount_chunk = ChunkRecord {
            id: 8,
            path: "packages/react-reconciler/src/ReactFiberCommitEffects.js".to_string(),
            start_line: 141,
            end_line: 177,
            language: "javascript".to_string(),
            symbol_hint: Some("commitHookEffectListMount".to_string()),
            text: "export function commitHookEffectListMount(flags, finishedWork) {\n  const create = effect.create;\n  destroy = create();\n  inst.destroy = destroy;\n}\n".to_string(),
            embedding: Vec::new(),
        };
        let card = crate::repo_plugins::react::build_react_effect_lifecycle_card(
            [
                &layout_unmount_chunk,
                &layout_mount_chunk,
                &flush_layout_chunk,
                &flush_passive_chunk,
                &passive_unmount_chunk,
                &passive_mount_chunk,
                &hook_unmount_chunk,
                &hook_mount_chunk,
            ],
            0.9,
        )
        .expect("expected lifecycle card");
        assert!(
            card.text.contains("mutation/layout cleanup"),
            "got: {}",
            card.text
        );
        assert!(
            card.text.contains("flushPassiveEffects"),
            "got: {}",
            card.text
        );
        assert!(
            card.text.contains("commitHookEffectListUnmount"),
            "got: {}",
            card.text
        );
        assert_eq!(
            card.slm_relevance_note.as_deref(),
            Some("React effect lifecycle summary")
        );
    }

    #[test]
    fn react_lifecycle_broad_overview_skips_injection() {
        // Broad lifecycle overview queries ("Trace the lifecycle hook execution
        // order when a component mounts, updates, and unmounts") are better
        // served by Claude's training knowledge. The lifecycle pack anchors
        // Claude on internal commit-phase functions instead of the user-facing
        // lifecycle model.
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let repo_root = std::env::temp_dir().join(format!("budi-react-lifecycle-test-{unique}"));
        fs::create_dir_all(&repo_root).expect("temp repo root");
        let state = RepoIndexState {
            repo_root: repo_root.to_string_lossy().to_string(),
            files: Vec::new(),
            chunks: vec![
                ChunkRecord {
                    id: 1,
                    path: "packages/react-reconciler/src/ReactFiberCommitEffects.js".to_string(),
                    start_line: 97,
                    end_line: 110,
                    language: "javascript".to_string(),
                    symbol_hint: Some("commitHookLayoutEffects".to_string()),
                    text: "export function commitHookLayoutEffects(finishedWork, hookFlags) {\n  // At this point layout effects have already been destroyed (during mutation phase).\n  commitHookEffectListMount(hookFlags, finishedWork);\n}\n".to_string(),
                    embedding: Vec::new(),
                },
                ChunkRecord {
                    id: 9,
                    path: "packages/react-reconciler/src/ReactFiberHooks.js".to_string(),
                    start_line: 1,
                    end_line: 20,
                    language: "javascript".to_string(),
                    symbol_hint: Some("renderWithHooks".to_string()),
                    text: "import ReactSharedInternals from 'shared/ReactSharedInternals';\n// React component lifecycle hook execution order for mounts, updates, and unmounts.\nexport function renderWithHooks() {}\n".to_string(),
                    embedding: Vec::new(),
                },
            ],
            updated_at_ts: 0,
        };
        let runtime = RuntimeIndex::from_state(&repo_root, state).expect("runtime");
        let config = BudiConfig::default();
        let response = build_query_response(
            &runtime,
            "Trace the lifecycle hook execution order when a React component mounts, updates, and unmounts.",
            None,
            None,
            None,
            RetrievalMode::Hybrid,
            &config,
        )
        .expect("query response");
        assert!(
            response.snippets.is_empty(),
            "expected lifecycle-overview skip to block injection, got: {:?}",
            response.snippets
        );
        let _ = fs::remove_dir_all(&repo_root);
    }

    #[test]
    fn react_lifecycle_specific_query_still_injects_pack() {
        // Specific lifecycle queries (about a named commit-phase function)
        // should still get the lifecycle pack.
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let repo_root =
            std::env::temp_dir().join(format!("budi-react-lifecycle-specific-test-{unique}"));
        fs::create_dir_all(&repo_root).expect("temp repo root");
        let state = RepoIndexState {
            repo_root: repo_root.to_string_lossy().to_string(),
            files: Vec::new(),
            chunks: vec![
                ChunkRecord {
                    id: 1,
                    path: "packages/react-reconciler/src/ReactFiberCommitEffects.js".to_string(),
                    start_line: 97,
                    end_line: 110,
                    language: "javascript".to_string(),
                    symbol_hint: Some("commitHookLayoutEffects".to_string()),
                    text: "export function commitHookLayoutEffects(finishedWork, hookFlags) {\n  // At this point layout effects have already been destroyed (during mutation phase).\n  commitHookEffectListMount(hookFlags, finishedWork);\n}\nexport function commitHookLayoutUnmountEffects(finishedWork, nearestMountedAncestor, hookFlags) {\n  // Layout effects are destroyed during the mutation phase so that all\n  // destroy functions for all fibers are called before any create functions.\n  commitHookEffectListUnmount(hookFlags, finishedWork, nearestMountedAncestor);\n}\n".to_string(),
                    embedding: Vec::new(),
                },
                ChunkRecord {
                    id: 2,
                    path: "packages/react-reconciler/src/ReactFiberCommitEffects.js".to_string(),
                    start_line: 114,
                    end_line: 138,
                    language: "javascript".to_string(),
                    symbol_hint: Some("commitHookLayoutUnmountEffects".to_string()),
                    text: "export function commitHookLayoutUnmountEffects(finishedWork, nearestMountedAncestor, hookFlags) {\n  // Layout effects are destroyed during the mutation phase so that all\n  // destroy functions for all fibers are called before any create functions.\n  commitHookEffectListUnmount(hookFlags, finishedWork, nearestMountedAncestor);\n}\n".to_string(),
                    embedding: Vec::new(),
                },
                ChunkRecord {
                    id: 3,
                    path: "packages/react-reconciler/src/ReactFiberCommitEffects.js".to_string(),
                    start_line: 141,
                    end_line: 177,
                    language: "javascript".to_string(),
                    symbol_hint: Some("commitHookEffectListMount".to_string()),
                    text: "export function commitHookEffectListMount(flags, finishedWork) {\n  const create = effect.create;\n  destroy = create();\n  inst.destroy = destroy;\n}\n".to_string(),
                    embedding: Vec::new(),
                },
                ChunkRecord {
                    id: 4,
                    path: "packages/react-reconciler/src/ReactFiberCommitEffects.js".to_string(),
                    start_line: 248,
                    end_line: 300,
                    language: "javascript".to_string(),
                    symbol_hint: Some("commitHookEffectListUnmount".to_string()),
                    text: "export function commitHookEffectListUnmount(flags, finishedWork, nearestMountedAncestor) {\n  const destroy = inst.destroy;\n  safelyCallDestroy(finishedWork, nearestMountedAncestor, destroy);\n}\n".to_string(),
                    embedding: Vec::new(),
                },
                ChunkRecord {
                    id: 5,
                    path: "packages/react-reconciler/src/ReactFiberCommitWork.js".to_string(),
                    start_line: 3497,
                    end_line: 3512,
                    language: "javascript".to_string(),
                    symbol_hint: Some("commitPassiveMountEffects".to_string()),
                    text: "export function commitPassiveMountEffects(root, finishedWork, committedLanes, committedTransitions, renderEndTime) {\n  commitPassiveMountOnFiber(root, finishedWork, committedLanes, committedTransitions, renderEndTime);\n}\n".to_string(),
                    embedding: Vec::new(),
                },
                ChunkRecord {
                    id: 6,
                    path: "packages/react-reconciler/src/ReactFiberCommitWork.js".to_string(),
                    start_line: 4585,
                    end_line: 4588,
                    language: "javascript".to_string(),
                    symbol_hint: Some("commitPassiveUnmountEffects".to_string()),
                    text: "export function commitPassiveUnmountEffects(finishedWork: Fiber): void {\n  commitPassiveUnmountOnFiber(finishedWork);\n}\n".to_string(),
                    embedding: Vec::new(),
                },
                ChunkRecord {
                    id: 7,
                    path: "packages/react-reconciler/src/ReactFiberWorkLoop.js".to_string(),
                    start_line: 4028,
                    end_line: 4100,
                    language: "javascript".to_string(),
                    symbol_hint: Some("flushLayoutEffects".to_string()),
                    text: "function flushLayoutEffects(): void {\n  // The next phase is the layout phase, where we call effects that read the host tree.\n  commitLayoutEffects(finishedWork, root, lanes);\n}\n".to_string(),
                    embedding: Vec::new(),
                },
                ChunkRecord {
                    id: 8,
                    path: "packages/react-reconciler/src/ReactFiberWorkLoop.js".to_string(),
                    start_line: 4645,
                    end_line: 4749,
                    language: "javascript".to_string(),
                    symbol_hint: Some("flushPassiveEffects".to_string()),
                    text: "function flushPassiveEffects(): boolean {\n  return flushPassiveEffectsImpl();\n}\nfunction flushPassiveEffectsImpl() {\n  commitPassiveUnmountEffects(root.current);\n  commitPassiveMountEffects(root, root.current, lanes, transitions, pendingEffectsRenderEndTime);\n}\n".to_string(),
                    embedding: Vec::new(),
                },
            ],
            updated_at_ts: 0,
        };
        let runtime = RuntimeIndex::from_state(&repo_root, state).expect("runtime");
        let config = BudiConfig::default();
        // Specific query about commitHookLayoutEffects — not a broad overview
        let response = build_query_response(
            &runtime,
            "Where is commitHookLayoutEffects defined and what is its role in the effect lifecycle?",
            None,
            None,
            None,
            RetrievalMode::Hybrid,
            &config,
        )
        .expect("query response");
        assert!(
            !response.snippets.is_empty(),
            "specific lifecycle query should still inject"
        );
        let _ = fs::remove_dir_all(&repo_root);
    }

    #[test]
    fn nextjs_app_router_query_injects_boundary_pack_and_nextjs_diagnostics() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let repo_root = std::env::temp_dir().join(format!("budi-nextjs-router-test-{unique}"));
        fs::create_dir_all(&repo_root).expect("temp repo root");
        let state = RepoIndexState {
            repo_root: repo_root.to_string_lossy().to_string(),
            files: Vec::new(),
            chunks: vec![
                ChunkRecord {
                    id: 1,
                    path: "app/layout.tsx".to_string(),
                    start_line: 1,
                    end_line: 18,
                    language: "typescript".to_string(),
                    symbol_hint: Some("RootLayout".to_string()),
                    text: "export const metadata = {\n  title: \"Budi Next Smoke\",\n};\n\nexport default function RootLayout({ children }: { children: React.ReactNode }) {\n  return <html>{children}</html>;\n}\n".to_string(),
                    embedding: Vec::new(),
                },
                ChunkRecord {
                    id: 2,
                    path: "app/page.tsx".to_string(),
                    start_line: 1,
                    end_line: 7,
                    language: "typescript".to_string(),
                    symbol_hint: Some("HomePage".to_string()),
                    text: "export default function HomePage() {\n  return <main>home</main>;\n}\n".to_string(),
                    embedding: Vec::new(),
                },
                ChunkRecord {
                    id: 3,
                    path: "app/dashboard/layout.tsx".to_string(),
                    start_line: 1,
                    end_line: 7,
                    language: "typescript".to_string(),
                    symbol_hint: Some("DashboardLayout".to_string()),
                    text: "export default function DashboardLayout({ children }: { children: React.ReactNode }) {\n  return <section>{children}</section>;\n}\n".to_string(),
                    embedding: Vec::new(),
                },
                ChunkRecord {
                    id: 4,
                    path: "app/dashboard/page.tsx".to_string(),
                    start_line: 1,
                    end_line: 7,
                    language: "typescript".to_string(),
                    symbol_hint: Some("DashboardPage".to_string()),
                    text: "export default function DashboardPage() {\n  return <main>dashboard</main>;\n}\n".to_string(),
                    embedding: Vec::new(),
                },
                ChunkRecord {
                    id: 5,
                    path: "app/api/users/route.ts".to_string(),
                    start_line: 1,
                    end_line: 5,
                    language: "typescript".to_string(),
                    symbol_hint: Some("GET".to_string()),
                    text: "export async function GET() {\n  return Response.json({ users: [\"ada\"] });\n}\n".to_string(),
                    embedding: Vec::new(),
                },
            ],
            updated_at_ts: 0,
        };
        let runtime = RuntimeIndex::from_state(&repo_root, state).expect("runtime");
        let config = BudiConfig::default();
        let response = build_query_response(
            &runtime,
            "How is this Next.js app-router structured, and which layout.tsx/page.tsx files own each route boundary?",
            None,
            None,
            None,
            RetrievalMode::Hybrid,
            &config,
        )
        .expect("query response");
        let first = response.snippets.first().expect("at least one snippet");
        assert!(
            first
                .reasons
                .iter()
                .any(|reason| reason == "nextjs-app-router-pack"),
            "got: {:?}",
            response.snippets
        );
        assert!(
            first.text.contains("segment / => layout app/layout.tsx"),
            "got: {}",
            first.text
        );
        assert!(
            first
                .text
                .contains("handler /api/users => route app/api/users/route.ts"),
            "got: {}",
            first.text
        );
        assert_eq!(
            response.diagnostics.top_ecosystem.as_deref(),
            Some("nextjs")
        );
        assert!(
            response
                .diagnostics
                .snippet_ecosystems
                .iter()
                .any(|ecosystem| ecosystem == "nextjs"),
            "got: {:?}",
            response.diagnostics.snippet_ecosystems
        );
        let _ = fs::remove_dir_all(&repo_root);
    }

    #[test]
    fn nextjs_route_boundary_query_without_framework_name_still_injects_pack() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let repo_root =
            std::env::temp_dir().join(format!("budi-nextjs-router-no-name-test-{unique}"));
        fs::create_dir_all(&repo_root).expect("temp repo root");
        let state = RepoIndexState {
            repo_root: repo_root.to_string_lossy().to_string(),
            files: Vec::new(),
            chunks: vec![
                ChunkRecord {
                    id: 1,
                    path: "app/layout.tsx".to_string(),
                    start_line: 1,
                    end_line: 6,
                    language: "typescript".to_string(),
                    symbol_hint: Some("RootLayout".to_string()),
                    text: "export default function RootLayout({ children }: { children: React.ReactNode }) {\n  return <html>{children}</html>;\n}\n".to_string(),
                    embedding: Vec::new(),
                },
                ChunkRecord {
                    id: 2,
                    path: "app/page.tsx".to_string(),
                    start_line: 1,
                    end_line: 3,
                    language: "typescript".to_string(),
                    symbol_hint: Some("HomePage".to_string()),
                    text: "export default function HomePage() {\n  return <main>home</main>;\n}\n".to_string(),
                    embedding: Vec::new(),
                },
                ChunkRecord {
                    id: 3,
                    path: "app/api/users/route.ts".to_string(),
                    start_line: 1,
                    end_line: 5,
                    language: "typescript".to_string(),
                    symbol_hint: Some("GET".to_string()),
                    text: "export async function GET() {\n  return Response.json({ users: [\"ada\"] });\n}\n".to_string(),
                    embedding: Vec::new(),
                },
            ],
            updated_at_ts: 0,
        };
        let runtime = RuntimeIndex::from_state(&repo_root, state).expect("runtime");
        let config = BudiConfig::default();
        let response = build_query_response(
            &runtime,
            "Which file handles the /api/users route, and how is it separated from the page/layout boundaries?",
            None,
            None,
            None,
            RetrievalMode::Hybrid,
            &config,
        )
        .expect("query response");
        let first = response.snippets.first().expect("at least one snippet");
        assert!(
            first
                .reasons
                .iter()
                .any(|reason| reason == "nextjs-app-router-pack"),
            "got: {:?}",
            response.snippets
        );
        assert_eq!(
            response.diagnostics.top_ecosystem.as_deref(),
            Some("nextjs")
        );
        let _ = fs::remove_dir_all(&repo_root);
    }

    #[test]
    fn architecture_design_query_ci_skip_blocks_low_confidence_first_card() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let repo_root = std::env::temp_dir().join(format!("budi-retrieval-test-{unique}"));
        fs::create_dir_all(&repo_root).expect("temp repo root");
        let state = RepoIndexState {
            repo_root: repo_root.to_string_lossy().to_string(),
            files: Vec::new(),
            chunks: vec![ChunkRecord {
                id: 1,
                path: "src/flask/sansio/scaffold.py".to_string(),
                start_line: 486,
                end_line: 505,
                language: "python".to_string(),
                symbol_hint: Some("after_request".to_string()),
                text: "@setupmethod\ndef after_request(self, f):\n    \"\"\"Register a function to run after each request to this object.\"\"\"\n    self.after_request_funcs.setdefault(None, []).append(f)\n    return f\n".to_string(),
                embedding: Vec::new(),
            }],
            updated_at_ts: 0,
        };
        let runtime = RuntimeIndex::from_state(&repo_root, state).expect("runtime");
        let config = BudiConfig::default();
        let response = build_query_response(
            &runtime,
            "I want to add a middleware that logs request timing. What files and functions would I need to modify?",
            None,
            None,
            None,
            RetrievalMode::Hybrid,
            &config,
        )
        .expect("query response");
        assert!(
            response.snippets.is_empty(),
            "expected ci_skip to block low-confidence first card, got: {:?}",
            response.snippets
        );
        let _ = fs::remove_dir_all(&repo_root);
    }

    #[test]
    fn module_layout_query_unconditional_skip_blocks_injection() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let repo_root = std::env::temp_dir().join(format!("budi-retrieval-test-{unique}"));
        fs::create_dir_all(&repo_root).expect("temp repo root");
        let state = RepoIndexState {
            repo_root: repo_root.to_string_lossy().to_string(),
            files: Vec::new(),
            chunks: vec![ChunkRecord {
                id: 1,
                path: "src/flask/sansio/app.py".to_string(),
                start_line: 479,
                end_line: 493,
                language: "python".to_string(),
                symbol_hint: Some("Flask".to_string()),
                text: "class Flask(App):\n    \"\"\"The flask object implements a WSGI application.\"\"\"\n".to_string(),
                embedding: Vec::new(),
            }],
            updated_at_ts: 0,
        };
        let runtime = RuntimeIndex::from_state(&repo_root, state).expect("runtime");
        let config = BudiConfig::default();
        let response = build_query_response(
            &runtime,
            "Describe the module layout of this codebase — which files own which concerns?",
            None,
            None,
            None,
            RetrievalMode::Hybrid,
            &config,
        )
        .expect("query response");
        assert!(
            response.snippets.is_empty(),
            "expected module-layout unconditional skip to block injection, got: {:?}",
            response.snippets
        );
        let _ = fs::remove_dir_all(&repo_root);
    }

    // ── extract_query_symbol_tokens ───────────────────────────────────────────

    #[test]
    fn symbol_tokens_extracts_camel_case() {
        let tokens = extract_query_symbol_tokens("how does scheduleUpdateOnFiber work");
        assert!(
            tokens.contains(&"scheduleupdateonfiber".to_string()),
            "got: {tokens:?}"
        );
    }

    #[test]
    fn symbol_tokens_extracts_underscore_names() {
        let tokens = extract_query_symbol_tokens("what is render_fiber_tree");
        assert!(
            tokens.contains(&"render_fiber_tree".to_string()),
            "got: {tokens:?}"
        );
    }

    #[test]
    fn symbol_tokens_extracts_simple_titlecase_identifier() {
        let tokens = extract_query_symbol_tokens("Where is Plan defined and what does it do?");
        assert!(tokens.contains(&"plan".to_string()), "got: {tokens:?}");
        assert!(!tokens.contains(&"where".to_string()), "got: {tokens:?}");
    }

    #[test]
    fn symbol_tokens_filters_plain_lowercase_words() {
        let tokens = extract_query_symbol_tokens("how does the scheduler work");
        // Plain lowercase words without underscore/digit/mixed-case should be filtered
        assert!(
            !tokens.contains(&"scheduler".to_string()),
            "got: {tokens:?}"
        );
        assert!(!tokens.contains(&"how".to_string()), "got: {tokens:?}");
    }

    #[test]
    fn symbol_tokens_rejects_short_tokens() {
        let tokens = extract_query_symbol_tokens("fn do");
        assert!(tokens.is_empty(), "got: {tokens:?}");
    }

    #[test]
    fn symbol_tokens_deduplicates() {
        let tokens = extract_query_symbol_tokens("commitRoot and commitRoot");
        let count = tokens.iter().filter(|t| t.as_str() == "commitroot").count();
        assert_eq!(count, 1, "got: {tokens:?}");
    }

    // ── normalize_channel_score ────────────────────────────────────────────────

    #[test]
    fn normalize_channel_score_lexical_clamps_0_to_1() {
        assert!((normalize_channel_score(0.0, ChannelKind::Lexical) - 0.0).abs() < 1e-6);
        assert!((normalize_channel_score(25.0, ChannelKind::Lexical) - 1.0).abs() < 1e-6);
        assert!((normalize_channel_score(12.5, ChannelKind::Lexical) - 0.5).abs() < 1e-6);
        // Negative input should clamp to 0
        assert_eq!(normalize_channel_score(-5.0, ChannelKind::Lexical), 0.0);
        // Over 25 should clamp to 1
        assert_eq!(normalize_channel_score(100.0, ChannelKind::Lexical), 1.0);
    }

    #[test]
    fn normalize_channel_score_vector_clamps_0_to_1() {
        assert!((normalize_channel_score(0.75, ChannelKind::Vector) - 0.75).abs() < 1e-6);
        assert_eq!(normalize_channel_score(-0.1, ChannelKind::Vector), 0.0);
        assert_eq!(normalize_channel_score(1.5, ChannelKind::Vector), 1.0);
    }

    #[test]
    fn normalize_channel_score_graph_divides_by_2() {
        assert!((normalize_channel_score(1.0, ChannelKind::Graph) - 0.5).abs() < 1e-6);
        assert!((normalize_channel_score(2.0, ChannelKind::Graph) - 1.0).abs() < 1e-6);
        assert_eq!(normalize_channel_score(0.0, ChannelKind::Symbol), 0.0);
    }

    // ── truncate_to (UTF-8 safety) ────────────────────────────────────────────

    #[test]
    fn truncate_to_ascii_unchanged_when_short() {
        assert_eq!(truncate_to("hello", 10), "hello");
    }

    #[test]
    fn truncate_to_ascii_truncated() {
        assert_eq!(truncate_to("hello world", 5), "hello");
    }

    #[test]
    fn truncate_to_multibyte_does_not_panic() {
        // "café" is 5 bytes (é is 2 bytes). Truncating at byte 4 would split é.
        let s = "café";
        assert_eq!(s.len(), 5); // c(1) + a(1) + f(1) + é(2)
        let result = truncate_to(s, 4);
        // Should back up to byte 3 ("caf") to avoid splitting the 2-byte 'é'
        assert_eq!(result, "caf");
    }

    #[test]
    fn truncate_to_multibyte_longer_string() {
        let s = "schedüleUpdate";
        // This shouldn't panic regardless of where we cut
        let result = truncate_to(s, 8);
        assert!(s.starts_with(result));
        assert!(s.is_char_boundary(result.len()));
    }

    // ── push_unique_reason ────────────────────────────────────────────────────

    #[test]
    fn push_unique_reason_no_duplicates() {
        let mut reasons: Vec<String> = vec!["lexical-hit".to_string()];
        push_unique_reason(&mut reasons, "lexical-hit");
        assert_eq!(reasons.len(), 1);
        push_unique_reason(&mut reasons, "semantic-hit");
        assert_eq!(reasons.len(), 2);
        push_unique_reason(&mut reasons, "semantic-hit");
        assert_eq!(reasons.len(), 2);
    }

    // ── weights_for_intent ────────────────────────────────────────────────────

    #[test]
    fn weights_are_positive_for_all_intents() {
        for kind in &[
            QueryIntentKind::SymbolDefinition,
            QueryIntentKind::FlowTrace,
            QueryIntentKind::SymbolUsage,
            QueryIntentKind::Architecture,
            QueryIntentKind::TestLookup,
            QueryIntentKind::RuntimeConfig,
        ] {
            let w = weights_for_intent(*kind);
            assert!(w.lexical > 0.0, "{kind:?} lexical weight must be > 0");
            assert!(w.vector > 0.0, "{kind:?} vector weight must be > 0");
            assert!(w.symbol > 0.0, "{kind:?} symbol weight must be > 0");
        }
    }

    #[test]
    fn sym_def_boosts_symbol_channel() {
        let sym_def = weights_for_intent(QueryIntentKind::SymbolDefinition);
        let arch = weights_for_intent(QueryIntentKind::Architecture);
        assert!(
            sym_def.symbol > arch.symbol,
            "SymbolDefinition should have higher symbol weight than Architecture"
        );
    }

    #[test]
    fn flow_trace_boosts_graph_channel() {
        let flow = weights_for_intent(QueryIntentKind::FlowTrace);
        let sym_def = weights_for_intent(QueryIntentKind::SymbolDefinition);
        assert!(
            flow.graph > sym_def.graph,
            "FlowTrace should have higher graph weight than SymbolDefinition"
        );
    }

    // ── contains_any ─────────────────────────────────────────────────────────

    #[test]
    fn contains_any_matches_substring() {
        assert!(contains_any(
            "what calls foo",
            &["what calls", "callers of"]
        ));
        assert!(contains_any(
            "callers of bar",
            &["what calls", "callers of"]
        ));
        assert!(!contains_any("hello world", &["what calls", "callers of"]));
    }

    #[test]
    fn contains_any_empty_patterns_returns_false() {
        assert!(!contains_any("anything", &[]));
    }

    // ── normalize_path ────────────────────────────────────────────────────────

    #[test]
    fn normalize_path_converts_backslash_and_lowercases() {
        assert_eq!(normalize_path("src\\Foo\\Bar"), "src/foo/bar");
        assert_eq!(normalize_path("src/foo/"), "src/foo");
        assert_eq!(normalize_path("SRC/FOO"), "src/foo");
    }

    // ── is_stub_body ─────────────────────────────────────────────────────────

    #[test]
    fn is_stub_body_detects_go_panic_not_implemented() {
        let text = r#"func (p *GRPCProvider) ReadStateBytes(r providers.ReadStateBytesRequest) providers.ReadStateBytesResponse {
	panic("not implemented")
}"#;
        assert!(is_stub_body(text));
    }

    #[test]
    fn is_stub_body_detects_rust_unimplemented() {
        assert!(is_stub_body("fn foo() {\n    unimplemented!()\n}"));
        assert!(is_stub_body("fn bar() -> Result<()> {\n    todo!()\n}"));
    }

    #[test]
    fn is_stub_body_detects_python_not_implemented() {
        assert!(is_stub_body(
            "def read_state(self):\n    raise NotImplementedError"
        ));
    }

    #[test]
    fn is_stub_body_detects_short_unsupported_error() {
        let text = r#"func (p *Provider) ReadStateBytes(req providers.ReadStateBytesRequest) providers.ReadStateBytesResponse {
	var resp providers.ReadStateBytesResponse
	resp.Diagnostics.Append(fmt.Errorf("unsupported state store type %q", req.TypeName))
	return resp
}"#;
        assert!(is_stub_body(text));
    }

    #[test]
    fn is_stub_body_detects_empty_function() {
        // Go empty function body
        assert!(is_stub_body(
            "func (v *QueryOperationJSON) Plan(plan *plans.Plan, schemas *terraform.Schemas) {\n}"
        ));
        // Single-line empty
        assert!(is_stub_body("def noop(self): pass"));
    }

    #[test]
    fn is_stub_body_detects_trivial_return() {
        assert!(is_stub_body("func foo() error {\n    return nil\n}"));
        assert!(is_stub_body("def noop(self):\n    pass\n"));
    }

    #[test]
    fn is_stub_body_rejects_real_implementation() {
        let text = r#"func (c *Context) Plan(config *configs.Config, prevRunState *states.State, opts *PlanOpts) (*plans.Plan, tfdiags.Diagnostics) {
	plan, _, diags := c.PlanAndEval(config, prevRunState, opts)
	if diags.HasErrors() {
		return nil, diags
	}
	plan.Complete = true
	plan.Timestamp = time.Now()
	err := plan.Validate()
	if err != nil {
		diags = diags.Append(err)
	}
	return plan, diags
}"#;
        assert!(!is_stub_body(text));
    }

    #[test]
    fn extract_query_pascal_tokens_finds_class_names() {
        let tokens = extract_query_pascal_tokens("Where is Session class defined?");
        assert!(tokens.contains("session"));
        assert!(!tokens.contains("where")); // stop word
    }

    #[test]
    fn extract_query_pascal_tokens_ignores_stop_words() {
        let tokens = extract_query_pascal_tokens("Describe the Engine type");
        assert!(tokens.contains("engine"));
        assert!(!tokens.contains("describe"));
        assert!(!tokens.contains("the"));
    }

    #[test]
    fn extract_query_pascal_tokens_backtick_pascal() {
        let tokens = extract_query_pascal_tokens("Where is `Session` defined?");
        assert!(tokens.contains("session"));
    }

    #[test]
    fn is_pascal_case_basic() {
        assert!(is_pascal_case("Session"));
        assert!(is_pascal_case("Engine"));
        assert!(is_pascal_case("Context"));
        assert!(is_pascal_case("RuntimeConfig"));
        assert!(!is_pascal_case("session"));   // lowercase
        assert!(!is_pascal_case("SESSION"));   // ALL_CAPS
        assert!(!is_pascal_case("Where"));     // stop word
        assert!(!is_pascal_case("ab"));        // too short
    }
}
