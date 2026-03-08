use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;

use crate::config::BudiConfig;
use crate::index::RuntimeIndex;
use crate::reason_codes::SKIP_REASON_NON_CODE_INTENT;
use crate::rpc::{QueryChannelScores, QueryDiagnostics, QueryResponse, QueryResultItem};
use context::{SnippetSelectionState, build_context, path_diversity_bucket, snippet_fingerprint};

mod context;

const RRF_K: f32 = 60.0;
const GRAPH_NEIGHBOR_EXPANSION_LIMIT: usize = 2;

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

// ── build_query_response ──────────────────────────────────────────────────────

pub fn build_query_response(
    runtime: &RuntimeIndex,
    query: &str,
    query_embedding: Option<&[f32]>,
    cwd: Option<&Path>,
    active_file: Option<&str>, // Phase AB: repo-relative path of last edited file → +0.20 boost
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
    let symbol_tokens = extract_query_symbol_tokens(query);
    let mut path_tokens = extract_query_path_tokens(query);
    let scope_hints = extract_scope_path_hints(query);
    add_dynamic_path_tokens(&mut path_tokens, &scope_hints);
    augment_path_tokens_for_intent(query, &intent, &mut path_tokens);

    // TestLookup: widen the candidate pool so inline test blocks (which score lower
    // than production code on the query text) have a chance to enter the fused stage.
    // This also helps Z2 find production co-location seeds from files like config.rs
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
        runtime.search_lexical(query, topk_lex)?
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

    // Phase Z2: For TestLookup, inject inline-test chunks co-located with top results.
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
        let mut top_pairs: Vec<(u64, f32)> =
            fused.iter().map(|(id, cs)| (*id, cs.score)).collect();
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

    // Phase AC/AD: Direct symbol-hint injection for SymbolDefinition/SymbolUsage.
    // Problem: definition chunk is invisible to the symbol channel in two cases:
    //   AC: pure-lowercase symbol names like "run" — `is_symbol_like_token` rejects them,
    //       so they never enter symbol_to_chunk_ids at all.
    //   AD: camelCase names like `scheduleUpdateOnFiber` — they ARE in symbol_to_chunk_ids
    //       but the codebase has so many call-sites that the definition chunk falls outside
    //       the topk limit, never entering the fused map where hint-match-boost can fire.
    //
    // Different seeds per intent:
    //   SymbolDefinition: use all symbol_tokens (includes camelCase from plain text + all
    //     backtick tokens via the pre-pass). Seeding the definition chunk is always correct.
    //   SymbolUsage: use only lowercase_backtick_tokens (Phase AC behaviour). For sym-use
    //     queries the symbol channel already finds callers well for camelCase identifiers.
    //     Seeding the definition chunk for sym-use just adds noise (e.g. P16 beginWork ctx
    //     jumped 787→1331 when sym-use also used all symbol_tokens).
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
            if !symbol_tokens.iter().any(|t| hint_lower == t.as_str()) {
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
    // Phase AC (preserved for SymbolUsage): only seed for pure-lowercase backtick tokens.
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

    // Phase Z3: Path-based inline-test seeding for TestLookup.
    // Problem: when the query is wordy ("What unit tests cover the config file parsing..."),
    // the production file (e.g. flags/config.rs) may not appear in topk at all — so Z2
    // has no seed and never injects its inline test block.
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
                if !subject_tokens.iter().any(|t| path_lower.contains(t.as_str())) {
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

        // Phase AB: boost chunks from the file Claude most recently edited/read.
        // Iterative edit sessions: the next query is very likely about the same file.
        // +0.20 is intentionally larger than cwd-proximity (+0.08) to ensure the
        // active file surfaces above directory-level neighbors.
        if let Some(af) = active_file {
            if chunk.path == af {
                adjusted += 0.20;
                push_unique_reason(&mut reasons, "active-file-boost");
            }
        }

        // R1: TestLookup — boost chunks from test files so they surface above source files.
        // Z1: Also boost inline test blocks (#[test], #[cfg(test)], mod tests, describe/it)
        // to handle Rust crates and JS/TS files that colocate tests in production files.
        if intent.kind == QueryIntentKind::TestLookup
            && (is_test_path(&chunk.path) || is_inline_test_chunk(&chunk.text))
        {
            adjusted += 0.15;
            push_unique_reason(&mut reasons, "test-path-boost");
        }

        // S1: SymbolDefinition — boost chunks whose symbol_hint is an exact match for a
        // query token. This surfaces definition chunks over reference/usage chunks when
        // the dominant function in a window is precisely what the user asked about.
        if intent.kind == QueryIntentKind::SymbolDefinition {
            if let Some(hint) = chunk.symbol_hint.as_deref() {
                let hint_lower = hint.to_ascii_lowercase();
                if !hint_lower.is_empty()
                    && !is_generic_symbol_hint(hint)
                    && symbol_tokens.iter().any(|t| t == &hint_lower)
                {
                    adjusted += 0.30;
                    push_unique_reason(&mut reasons, "hint-match-boost");
                }
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

    // Phase K2: Per-intent retrieval limit. Honour explicit user config override.
    let default_limit = intent_retrieval_limit(intent.kind);
    // Phase AJ: SymbolDefinition with hint-match-boost → definition confirmed found.
    // Restrict to 2 candidates and skip graph expansion to avoid noise cards from
    // common query words ("path", "matcher", "writer") crowding out the definition.
    // The [structural context] block already provides callers/callees compactly.
    // Use hint-match-boost (stable across seeded and HNSW paths) not sym-hint-seed.
    let sym_def_seeded = intent.kind == QueryIntentKind::SymbolDefinition
        && scored
            .first()
            .map_or(false, |c| c.reasons.iter().any(|r| r == "hint-match-boost"));
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
    for candidate in &scored {
        if selection.snippets.len() >= target_limit {
            break;
        }
        if candidate.score < min_score && !selection.snippets.is_empty() {
            continue;
        }
        let _ = try_push_scored_chunk(runtime, candidate, &mut selection);
    }
    if selection.snippets.is_empty() {
        if let Some(best) = scored.first() {
            let _ = try_push_scored_chunk(runtime, best, &mut selection);
        }
    }
    if should_expand_graph_neighbors(intent.kind) && !sym_def_seeded {
        expand_graph_neighbors(
            runtime,
            &mut selection,
            target_limit.saturating_add(GRAPH_NEIGHBOR_EXPANSION_LIMIT),
            GRAPH_NEIGHBOR_EXPANSION_LIMIT,
        );
    }

    // Phase AR: SymbolDefinition continuation chunk.
    // When sym_def_seeded (hint-match-boost confirmed the definition was found), the
    // 80-line definition chunk often doesn't cover the full function body. Look for the
    // next chunk from the same file (smallest start_line > def.start_line, accounting for
    // the 20-line overlap where stride=60) and inject it in place of any call-site card
    // from a different file. This gives Claude the complete function body — including the
    // key operations that follow the DEV guards — for "what are its first steps" queries
    // like P3 (scheduleUpdateOnFiber, lines 967-1095, chunk at 961-1040 + 1021-1100).
    if sym_def_seeded {
        let continuation = selection.snippets.first().and_then(|def_item| {
            let def_path = def_item.path.clone();
            let def_start = def_item.start_line;
            let def_score = def_item.score;
            let has_foreign_card2 = selection
                .snippets
                .get(1)
                .map_or(false, |s| s.path != def_path);
            if !has_foreign_card2 {
                return None;
            }
            let cont_id = runtime.adjacent_chunk(&def_path, def_start)?;
            let cont = runtime.chunk(cont_id)?;
            Some(crate::rpc::QueryResultItem {
                path: cont.path.clone(),
                start_line: cont.start_line,
                end_line: cont.end_line,
                score: def_score * 0.60,
                reasons: vec!["definition-continuation".to_string()],
                channel_scores: QueryChannelScores::default(),
                text: cont.text.clone(),
                slm_relevance_note: None,
            })
        });
        if let Some(cont_item) = continuation {
            // Replace card 2 (foreign call site) with the continuation of the function body.
            selection.snippets.truncate(1);
            selection.snippets.push(cont_item);
        }
    }

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
        recommended_injection: !selection.snippets.is_empty() && intent.code_related,
        skip_reason: if !intent.code_related {
            Some(SKIP_REASON_NON_CODE_INTENT.to_string())
        } else {
            None
        },
    };

    let context = build_context(&selection.snippets, config.context_char_budget);
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
pub fn format_context(snippets: &[QueryResultItem], budget: usize) -> String {
    context::build_context(snippets, budget)
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
        if !callee_names.is_empty() {
            entry.push_str(&format!("  → calls: {}\n", callee_names.join(", ")));
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

fn is_test_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.contains("/test")
        || lower.contains("/spec")
        || lower.contains("__tests__")
        || lower.contains("__spec__")
        || lower.starts_with("test")
        || lower.starts_with("spec")
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

/// Extract backtick-quoted tokens that are genuinely all-lowercase in the original query (Phase AC).
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
                && token.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
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

/// Extract subject tokens from a TestLookup query for path-based seeding (Phase Z3).
/// Strips test-noise words ("unit tests cover where live repo...") and common stop words,
/// keeping content words ≥ 4 chars that describe the feature being tested.
fn test_subject_tokens(query: &str) -> Vec<String> {
    const STOP: &[&str] = &[
        "what", "which", "where", "when", "who", "how",
        "unit", "integration", "e2e", "test", "tests", "testing", "spec", "specs",
        "cover", "covers", "covered", "covering",
        "live", "lives", "located", "location", "find",
        "the", "a", "an", "and", "or", "in", "of", "for", "to", "on",
        "do", "does", "are", "is", "it", "they", "their", "them",
        "repo", "repository", "codebase", "logic", "code",
        "file", "files", "directory", "folder",
    ];
    query
        .split(|c: char| !c.is_alphanumeric())
        .map(|w| w.to_ascii_lowercase())
        .filter(|w| w.len() >= 4 && !STOP.contains(&w.as_str()))
        .collect()
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

    let context = context::build_context(&snippets, context_budget);
    (snippets, context)
}

// ── Selection helpers ─────────────────────────────────────────────────────────

/// Phase K2: Per-intent default retrieval limit.
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
        // Phase AH: raised from 0.20 to 0.30 to exclude lexical noise at 0.26-0.29
        // that dilutes SymbolDef context when sym-hint-seed already placed the definition at 0.58.
        QueryIntentKind::SymbolDefinition => relative.max(0.30),
        QueryIntentKind::TestLookup => relative.max(0.22),
        // Phase AM: when top score is high (≥0.60, strong rt-cfg signal), raise floor to 0.40
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
        // Phase AQ: when top score is high (≥0.60, strong signal), raise floor to 0.40
        // to filter weakly-related cards (devtools profiling, tests, server rendering) at 0.30-0.36
        // that dilute high-confidence architecture answers. Low-confidence arch queries
        // (top < 0.60, e.g. entry-point surveys) keep the standard relative floor.
        QueryIntentKind::Architecture => {
            if top.score >= 0.60 {
                relative.max(0.40)
            } else {
                relative
            }
        }
        _ => relative,
    }
}

fn should_expand_graph_neighbors(intent_kind: QueryIntentKind) -> bool {
    // Phase AO: disabled for SymbolUsage — graph expansion adds distant callee/caller
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
    selection.snippets.push(QueryResultItem {
        path: chunk.path.clone(),
        start_line: chunk.start_line,
        end_line: chunk.end_line,
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
            // Phase AE: cap raw_score at 0.45 to prevent graph-neighbor inflation.
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
    // V1: SymbolUsage check runs first — "what calls X" is unambiguous and must not be
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

fn extract_query_symbol_tokens(query: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    // Phase AC: backtick-quoted identifiers are always symbols regardless of case pattern.
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
        if !(has_underscore || has_digit || has_symbol_case_pattern(raw)) {
            continue;
        }
        let normalized = raw.to_ascii_lowercase();
        if seen.insert(normalized.clone()) {
            out.push(normalized);
        }
    }
    out
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
            | "url"
            | "uri"
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
        // Architecture with top < 0.60 uses standard relative floor (no AQ conditional)
        let chunks = vec![make_scored_chunk(1, 0.50), make_scored_chunk(2, 0.20)];
        let floor = min_selection_score(&chunks, QueryIntentKind::Architecture);
        // relative = 0.50 * 0.40 = 0.20; no intent-specific minimum
        assert!((floor - 0.20).abs() < 1e-5, "expected ~0.20, got {floor}");
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
        // relative = 0.40 * 0.40 = 0.16, but SymbolDefinition floor is 0.30 (Phase AH)
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
        // Phase AM: top >= 0.60 → floor = relative.max(0.40) to cut brew/hyperlink noise
        let chunks = vec![make_scored_chunk(1, 0.80)];
        let floor = min_selection_score(&chunks, QueryIntentKind::RuntimeConfig);
        // relative = 0.80 * 0.40 = 0.32; clamped to 0.40
        assert!((floor - 0.40).abs() < 1e-5, "expected 0.40 floor, got {floor}");
    }

    #[test]
    fn min_score_runtime_config_low_confidence_uses_0_18_floor() {
        // Phase AM: top < 0.60 → keep 0.18 floor (React __DEV__ scenario)
        let chunks = vec![make_scored_chunk(1, 0.40)];
        let floor = min_selection_score(&chunks, QueryIntentKind::RuntimeConfig);
        // relative = 0.40 * 0.40 = 0.16; clamped to 0.18
        assert!((floor - 0.18).abs() < 1e-5, "expected 0.18 floor, got {floor}");
    }

    #[test]
    fn min_score_architecture_high_confidence_uses_0_40_floor() {
        // Phase AQ: top >= 0.60 → floor = relative.max(0.40) to cut devtools/test noise
        let chunks = vec![make_scored_chunk(1, 0.65)];
        let floor = min_selection_score(&chunks, QueryIntentKind::Architecture);
        // relative = 0.65 * 0.40 = 0.26; clamped to 0.40
        assert!((floor - 0.40).abs() < 1e-5, "expected 0.40 floor, got {floor}");
    }

    #[test]
    fn min_score_architecture_low_confidence_uses_relative_floor() {
        // Phase AQ: top < 0.60 → standard relative floor (entry-point survey)
        let chunks = vec![make_scored_chunk(1, 0.40)];
        let floor = min_selection_score(&chunks, QueryIntentKind::Architecture);
        // relative = 0.40 * 0.40 = 0.16
        assert!((floor - 0.16).abs() < 1e-5, "expected 0.16 floor, got {floor}");
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

    #[test]
    fn is_test_path_permissive_for_test_in_name() {
        // Files with "test" in the name (via /test substring) are treated as test-related.
        // This is intentional: utils/testUtils.ts contains /test as a substring.
        assert!(is_test_path("utils/testUtils.ts")); // contains /test (in /testUtils)
        assert!(is_test_path("src/testUtils.ts")); // contains /test (in /testUtils)
        // Top-level file starting with "test" is also detected
        assert!(is_test_path("testUtils.ts"));
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
}
