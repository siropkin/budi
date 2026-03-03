use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;

use crate::config::BudiConfig;
use crate::git::GitSnapshot;
use crate::index::RuntimeIndex;
use crate::rpc::{QueryDiagnostics, QueryResponse, QueryResultItem};

const RRF_K: f32 = 60.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueryIntentKind {
    SymbolUsage,
    SymbolDefinition,
    PathLookup,
    Architecture,
    Docs,
    CodeNavigation,
    TestLookup,
    NonCode,
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
}

#[derive(Debug, Clone)]
struct ScoredChunk {
    id: u64,
    score: f32,
    reasons: Vec<String>,
    clause_hits: Vec<usize>,
}

#[derive(Debug, Default)]
struct SnippetSelectionState {
    snippets: Vec<QueryResultItem>,
    seen_fingerprints: HashSet<String>,
    snippets_per_path: HashMap<String, usize>,
    snippets_per_bucket: HashMap<String, usize>,
    per_file_limit: usize,
    per_bucket_limit: usize,
}

#[derive(Debug, Clone, Default)]
enum QueryClauseKind {
    Definition,
    Usage,
    #[default]
    Generic,
}

#[derive(Debug, Clone, Default)]
struct QueryClause {
    kind: QueryClauseKind,
    tokens: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
struct RetrievalPolicy {
    apply_route_ownership_rules: bool,
}

fn retrieval_policy(intent_kind: QueryIntentKind) -> RetrievalPolicy {
    match intent_kind {
        // Keep route ownership rules for explicit "where is this symbol rendered/owned" prompts,
        // but prune broad focus/config/routing rule packs that were too ad-hoc.
        QueryIntentKind::SymbolDefinition => RetrievalPolicy {
            apply_route_ownership_rules: true,
        },
        QueryIntentKind::SymbolUsage
        | QueryIntentKind::PathLookup
        | QueryIntentKind::Architecture
        | QueryIntentKind::Docs
        | QueryIntentKind::CodeNavigation
        | QueryIntentKind::TestLookup
        | QueryIntentKind::NonCode => RetrievalPolicy {
            apply_route_ownership_rules: false,
        },
    }
}

pub fn build_query_response(
    runtime: &RuntimeIndex,
    query: &str,
    query_embedding: Option<&[f32]>,
    git_snapshot: &GitSnapshot,
    cwd: Option<&Path>,
    config: &BudiConfig,
) -> Result<QueryResponse> {
    let intent = analyze_query_intent(query);
    let policy = retrieval_policy(intent.kind);
    let query_lower = query.to_ascii_lowercase();
    let i18n_keys = extract_query_i18n_keys(query);
    let scope_hints = extract_scope_path_hints(query);
    let clauses = extract_query_clauses(query);
    let workflow_docs_focus =
        matches!(intent.kind, QueryIntentKind::Docs) && is_workflow_docs_query(&query_lower);
    let wants_test_artifacts = contains_any(
        &query_lower,
        &["test", "tests", "unit", "spec", "mock", "fixture"],
    );
    let symbol_tokens = extract_query_symbol_tokens(query);
    let route_ownership_focus = policy.apply_route_ownership_rules
        && is_route_ownership_query(&query_lower, &symbol_tokens, intent.kind);
    let mut path_tokens = extract_query_path_tokens(query);
    add_dynamic_path_tokens(&mut path_tokens, &scope_hints);
    augment_path_tokens_for_intent(query, &intent, &mut path_tokens);
    let specific_path_tokens = path_tokens
        .iter()
        .filter(|token| !is_generic_path_token(token.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    let has_specific_path_tokens = !specific_path_tokens.is_empty();
    let doc_path_hints = extract_explicit_doc_path_hints(query);
    let lexical = runtime.search_lexical(query, config.topk_lexical)?;
    let vector = query_embedding
        .map(|embedding| runtime.search_vector(embedding, config.topk_vector))
        .unwrap_or_default();
    let symbol_limit = config.topk_lexical.max(config.retrieval_limit * 2);
    let path_limit = config.topk_lexical.max(config.retrieval_limit * 2);
    let graph_limit = config.topk_lexical.max(config.retrieval_limit * 2);
    let symbol = diversify_channel_by_path(
        runtime,
        &runtime.search_symbol_tokens(&symbol_tokens, symbol_limit),
        symbol_limit,
    );
    let path = diversify_channel_by_path(
        runtime,
        &runtime.search_path_tokens(&path_tokens, path_limit),
        path_limit,
    );
    let graph = diversify_channel_by_path(
        runtime,
        &runtime.search_graph_tokens(&symbol_tokens, graph_limit),
        graph_limit,
    );
    let fused = fuse_channel_scores(&lexical, &vector, &symbol, &path, &graph, &intent);

    let dirty_set: std::collections::HashSet<&str> = git_snapshot
        .dirty_files
        .iter()
        .map(String::as_str)
        .collect();
    let cwd_rel = cwd
        .and_then(|path| path.to_str())
        .map(normalize_path)
        .unwrap_or_default();

    let mut scored = Vec::new();
    for (id, candidate) in fused {
        let Some(chunk) = runtime.chunk(id) else {
            continue;
        };
        let mut adjusted = candidate.score;
        let mut reasons = candidate.signals;
        let lower_path = chunk.path.to_ascii_lowercase();
        let lower_text = chunk.text.to_ascii_lowercase();
        if runtime.is_doc_like_chunk(id) && intent.code_related && !intent.allow_docs {
            adjusted -= 0.25;
            push_unique_reason(&mut reasons, "doc-penalty");
        }
        let path_token_matches = count_path_token_matches(&chunk.path, &path_tokens);
        let specific_path_matches = count_path_token_matches(&chunk.path, &specific_path_tokens);
        let scope_matches = if scope_hints.is_empty() {
            0
        } else {
            count_path_token_matches(&chunk.path, &scope_hints)
        };
        if path_token_matches > 0 {
            adjusted += (path_token_matches as f32).min(3.0) * 0.03;
            push_unique_reason(&mut reasons, "path-token-match");
        }
        if has_specific_path_tokens {
            if specific_path_matches > 0 {
                adjusted += 0.14 + (specific_path_matches as f32).min(2.0) * 0.02;
                push_unique_reason(&mut reasons, "specific-path-hit");
            } else if path_token_matches > 0 {
                let generic_only_penalty = if route_ownership_focus
                    && contains_path_fragment(
                        &lower_path,
                        &["route", "routes", "routing", "router"],
                    ) {
                    0.04
                } else {
                    0.18
                };
                adjusted -= generic_only_penalty;
                push_unique_reason(&mut reasons, "generic-path-only");
            }
        }
        if intent.code_related && is_low_signal_code_path(&lower_path) {
            adjusted -= 0.28;
            push_unique_reason(&mut reasons, "analysis-config-penalty");
        }
        if matches!(
            intent.kind,
            QueryIntentKind::PathLookup
                | QueryIntentKind::CodeNavigation
                | QueryIntentKind::TestLookup
                | QueryIntentKind::SymbolDefinition
                | QueryIntentKind::Architecture
        ) {
            let has_path_channel_signal = reasons.iter().any(|r| r == "path-hit");
            let has_symbol_signal = reasons.iter().any(|r| r == "symbol-hit");
            let strong_path_signal = has_symbol_signal
                || specific_path_matches > 0
                || (!has_specific_path_tokens
                    && (path_token_matches > 0 || has_path_channel_signal));
            if strong_path_signal {
                adjusted += 0.08;
                push_unique_reason(&mut reasons, "path-intent-boost");
            } else {
                let weak_path_penalty = if has_specific_path_tokens && !route_ownership_focus {
                    0.14
                } else {
                    0.08
                };
                adjusted -= weak_path_penalty;
                push_unique_reason(&mut reasons, "weak-path-signal");
            }
        }
        if !scope_hints.is_empty() {
            if scope_matches > 0 {
                adjusted += 0.16 + ((scope_matches as f32).min(2.0) * 0.02);
                push_unique_reason(&mut reasons, "scope-hit");
            } else {
                adjusted -= 0.06;
                push_unique_reason(&mut reasons, "scope-miss");
            }
        }
        if matches!(intent.kind, QueryIntentKind::Docs) {
            if runtime.is_doc_like_chunk(id) {
                adjusted += 0.22;
                push_unique_reason(&mut reasons, "doc-hit");
            } else {
                adjusted -= 0.25;
                push_unique_reason(&mut reasons, "non-doc-penalty");
            }
            if lower_path == "readme.md" || lower_path.starts_with("docs/") {
                adjusted += 0.18;
                push_unique_reason(&mut reasons, "top-docs-hit");
            } else if lower_path.ends_with("/readme.md") || lower_path.contains("/docs/") {
                adjusted += 0.06;
                push_unique_reason(&mut reasons, "nested-docs-hit");
            }
            if is_experimental_path(&lower_path) {
                adjusted -= 0.24;
                push_unique_reason(&mut reasons, "experimental-doc-penalty");
            }
            if workflow_docs_focus {
                if lower_path.starts_with("docs/") {
                    adjusted += 0.16;
                    push_unique_reason(&mut reasons, "workflow-docs-hit");
                } else if lower_path.ends_with("/readme.md") {
                    adjusted -= 0.12;
                    push_unique_reason(&mut reasons, "workflow-docs-miss");
                }
            }
            if matches_doc_path_hint(&chunk.path, &doc_path_hints) {
                adjusted += 0.35;
                push_unique_reason(&mut reasons, "doc-path-hit");
            }
        }
        if matches!(intent.kind, QueryIntentKind::Architecture) {
            if contains_path_fragment(
                &lower_path,
                &[
                    "/docs/",
                    "readme",
                    "architecture",
                    "routing",
                    "router",
                    "bootstrap",
                ],
            ) {
                adjusted += 0.14;
                push_unique_reason(&mut reasons, "architecture-anchor-hit");
            } else {
                adjusted -= 0.04;
                push_unique_reason(&mut reasons, "architecture-anchor-miss");
            }
            if contains_any(&query_lower, &["repository", "repo"]) {
                if lower_path == "readme.md" || lower_path.starts_with("docs/") {
                    adjusted += 0.22;
                    push_unique_reason(&mut reasons, "repo-architecture-hit");
                } else if lower_path.ends_with("/readme.md") {
                    adjusted -= 0.18;
                    push_unique_reason(&mut reasons, "repo-architecture-miss");
                } else if lower_path.contains("/docs/") && !lower_path.starts_with("docs/") {
                    adjusted -= 0.06;
                    push_unique_reason(&mut reasons, "repo-architecture-miss");
                }
            }
        }
        if !i18n_keys.is_empty() {
            let i18n_key_hits = i18n_keys
                .iter()
                .filter(|key| lower_text.contains(key.as_str()))
                .count();
            if i18n_key_hits > 0 {
                adjusted += 0.30 + (i18n_key_hits as f32).min(2.0) * 0.06;
                push_unique_reason(&mut reasons, "i18n-key-hit");
            }
        }
        if is_test_like_path(&lower_path) {
            if wants_test_artifacts || matches!(intent.kind, QueryIntentKind::TestLookup) {
                let test_bonus = if matches!(intent.kind, QueryIntentKind::Docs) {
                    0.06
                } else {
                    0.14
                };
                adjusted += test_bonus;
                push_unique_reason(&mut reasons, "test-intent-hit");
            } else {
                adjusted -= 0.18;
                push_unique_reason(&mut reasons, "test-fixture-penalty");
            }
        }
        if matches!(intent.kind, QueryIntentKind::SymbolUsage) {
            let has_symbol_signal = reasons.iter().any(|r| r == "symbol-hit");
            let has_graph_signal = reasons.iter().any(|r| r == "graph-hit");
            if has_symbol_signal || has_graph_signal {
                adjusted += if has_graph_signal { 0.30 } else { 0.25 };
                if has_graph_signal {
                    push_unique_reason(&mut reasons, "graph-usage-hit");
                }
            } else {
                adjusted -= 0.12;
            }
        }
        let definition_hits = count_symbol_definition_hits(&lower_text, &symbol_tokens);
        let import_hits = count_symbol_import_hits(&lower_text, &symbol_tokens);
        let symbol_reference_hits = count_symbol_reference_hits(&lower_text, &symbol_tokens);
        if policy.apply_route_ownership_rules
            && route_ownership_focus
            && contains_path_fragment(&lower_path, &["route", "routes", "routing", "router"])
        {
            if import_hits > 0 {
                adjusted += 0.26;
                push_unique_reason(&mut reasons, "route-ownership-hit");
            } else if symbol_reference_hits > 0 {
                adjusted += 0.12;
                push_unique_reason(&mut reasons, "route-ownership-hit");
            } else {
                adjusted -= 0.06;
                push_unique_reason(&mut reasons, "route-ownership-miss");
            }
        }
        if matches!(intent.kind, QueryIntentKind::SymbolDefinition) {
            if definition_hits > 0 {
                adjusted += 0.20 + (definition_hits as f32).min(2.0) * 0.05;
                push_unique_reason(&mut reasons, "symbol-definition-hit");
                if !scope_hints.is_empty() && scope_matches == 0 {
                    adjusted += 0.06;
                    push_unique_reason(&mut reasons, "definition-outside-scope");
                }
            }
            if import_hits > 0 {
                adjusted += 0.14;
                push_unique_reason(&mut reasons, "symbol-import-hit");
            }
            if definition_hits == 0 {
                adjusted -= 0.28;
                push_unique_reason(&mut reasons, "symbol-definition-miss");
            }
            if !scope_hints.is_empty()
                && scope_matches > 0
                && symbol_reference_hits == 0
                && definition_hits == 0
            {
                adjusted -= 0.14;
                push_unique_reason(&mut reasons, "scope-only-no-symbol");
            }
        }
        let clause_hits = if clauses.is_empty() {
            Vec::new()
        } else {
            clauses
                .iter()
                .map(|clause| {
                    count_clause_token_matches(
                        &lower_path,
                        &lower_text,
                        clause,
                        definition_hits,
                        symbol_reference_hits,
                    )
                })
                .collect::<Vec<_>>()
        };
        let matched_clauses = clause_hits.iter().filter(|hits| **hits > 0).count();
        if matched_clauses > 0 {
            adjusted += (matched_clauses as f32).min(3.0) * 0.035;
            push_unique_reason(&mut reasons, "clause-match");
        }
        if dirty_set.contains(chunk.path.as_str()) {
            adjusted += 0.12;
            push_unique_reason(&mut reasons, "dirty-file");
        }
        if !cwd_rel.is_empty() && chunk.path.starts_with(&cwd_rel) {
            adjusted += 0.08;
            push_unique_reason(&mut reasons, "cwd-proximity");
        }
        if reasons.is_empty() {
            reasons.push("semantic+lexical".to_string());
        }
        scored.push(ScoredChunk {
            id,
            score: adjusted,
            reasons,
            clause_hits,
        });
    }

    scored.sort_by(|a, b| b.score.total_cmp(&a.score));
    let per_file_limit = if matches!(intent.kind, QueryIntentKind::SymbolUsage) {
        1
    } else if matches!(intent.kind, QueryIntentKind::TestLookup) {
        3
    } else {
        2
    };
    let per_bucket_limit = if matches!(intent.kind, QueryIntentKind::TestLookup) {
        4
    } else {
        2
    };
    let mut selection = SnippetSelectionState {
        per_file_limit,
        per_bucket_limit,
        ..SnippetSelectionState::default()
    };
    let selection_window = config
        .retrieval_limit
        .saturating_mul(4)
        .max(config.retrieval_limit);
    let ranked_candidates =
        if clauses.len() > 1 || matches!(intent.kind, QueryIntentKind::SymbolDefinition) {
            scored
        } else {
            scored
                .into_iter()
                .take(selection_window)
                .collect::<Vec<_>>()
        };

    if clauses.len() > 1 {
        for clause_idx in 0..clauses.len() {
            for candidate in &ranked_candidates {
                if candidate
                    .clause_hits
                    .get(clause_idx)
                    .copied()
                    .unwrap_or_default()
                    == 0
                {
                    continue;
                }
                if try_push_scored_chunk(runtime, candidate, &mut selection) {
                    break;
                }
            }
            if selection.snippets.len() >= config.retrieval_limit {
                break;
            }
        }
    }

    for candidate in &ranked_candidates {
        if selection.snippets.len() >= config.retrieval_limit {
            break;
        }
        let _ = try_push_scored_chunk(runtime, candidate, &mut selection);
    }

    let diagnostics = build_diagnostics(
        &intent,
        &selection.snippets,
        config.smart_skip_enabled,
        config.skip_non_code_prompts,
        config.min_confidence_to_inject,
    );
    let context = build_context(&selection.snippets, config.context_char_budget);
    Ok(QueryResponse {
        branch: git_snapshot.branch.clone(),
        head: git_snapshot.head.clone(),
        total_candidates: lexical.len() + vector.len() + symbol.len() + path.len() + graph.len(),
        context,
        snippets: selection.snippets,
        diagnostics,
    })
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
        reason: candidate.reasons.join(","),
        text: chunk.text.clone(),
    });
    *selection
        .snippets_per_path
        .entry(chunk.path.clone())
        .or_insert(0) += 1;
    *selection.snippets_per_bucket.entry(bucket).or_insert(0) += 1;
    true
}

fn path_diversity_bucket(path: &str) -> String {
    let mut parts = path
        .split('/')
        .filter(|part| !part.is_empty())
        .map(|part| part.to_ascii_lowercase());
    let first = parts.next().unwrap_or_else(|| "root".to_string());
    if let Some(second) = parts.next() {
        format!("{first}/{second}")
    } else {
        first
    }
}

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
        let entry = scores.entry(*id).or_default();
        entry.score += rr + normalized * weight * 0.35;
        push_unique_reason(&mut entry.signals, channel_signal_name(kind));
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

fn push_unique_reason(reasons: &mut Vec<String>, reason: &str) {
    if reasons.iter().any(|existing| existing == reason) {
        return;
    }
    reasons.push(reason.to_string());
}

fn count_path_token_matches(path: &str, path_tokens: &[String]) -> usize {
    if path_tokens.is_empty() {
        return 0;
    }
    let normalized = path.to_ascii_lowercase();
    let mut seen = HashSet::new();
    for token in path_tokens {
        if token.len() < 3 {
            continue;
        }
        if normalized.contains(token) {
            seen.insert(token);
        }
    }
    seen.len()
}

fn build_diagnostics(
    intent: &QueryIntent,
    snippets: &[QueryResultItem],
    smart_skip_enabled: bool,
    skip_non_code_prompts: bool,
    min_confidence_to_inject: f32,
) -> QueryDiagnostics {
    let top_score = snippets.first().map(|s| s.score).unwrap_or_default();
    let second_score = snippets.get(1).map(|s| s.score).unwrap_or_default();
    let margin = (top_score - second_score).max(0.0);
    let top_signals = snippets
        .first()
        .map(|s| {
            s.reason
                .split(',')
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let confidence = estimate_confidence(top_score, margin, snippets.len(), &top_signals, intent);

    let mut recommended_injection = !snippets.is_empty();
    let mut skip_reason = None;
    if smart_skip_enabled {
        if skip_non_code_prompts && !intent.code_related {
            recommended_injection = false;
            skip_reason = Some("non-code-intent".to_string());
        } else if confidence < min_confidence_to_inject {
            recommended_injection = false;
            skip_reason = Some(format!("low-confidence:{confidence:.3}"));
        }
    }

    QueryDiagnostics {
        intent: intent_name(intent.kind).to_string(),
        confidence,
        top_score,
        margin,
        signals: top_signals,
        recommended_injection,
        skip_reason,
    }
}

fn estimate_confidence(
    top_score: f32,
    margin: f32,
    snippet_count: usize,
    top_signals: &[String],
    intent: &QueryIntent,
) -> f32 {
    let mut confidence: f32 = 0.0;
    if top_score >= 0.45 {
        confidence += 0.35;
    } else if top_score >= 0.30 {
        confidence += 0.20;
    }
    if margin >= 0.08 {
        confidence += 0.20;
    } else if margin >= 0.03 {
        confidence += 0.10;
    }
    if snippet_count >= 3 {
        confidence += 0.10;
    }
    if top_signals.len() >= 2 {
        confidence += 0.15;
    }
    if intent.code_related {
        confidence += 0.10;
    }
    if matches!(
        intent.kind,
        QueryIntentKind::SymbolUsage
            | QueryIntentKind::SymbolDefinition
            | QueryIntentKind::PathLookup
            | QueryIntentKind::TestLookup
    ) && top_signals.iter().any(|s| {
        s == "symbol-hit" || s == "path-hit" || s == "path-token-match" || s == "graph-hit"
    }) {
        confidence += 0.10;
    }
    if matches!(intent.kind, QueryIntentKind::Docs) {
        if top_signals.iter().any(|s| s == "doc-path-hit") {
            confidence += 0.25;
        } else if top_signals.iter().any(|s| s == "doc-hit") {
            confidence += 0.15;
        }
    }
    if top_signals.iter().any(|s| s == "i18n-key-hit") {
        confidence += 0.18;
    }
    if top_signals.iter().any(|s| s == "weak-path-signal") {
        confidence -= 0.10;
    }
    confidence.clamp(0.0, 1.0)
}

fn intent_name(kind: QueryIntentKind) -> &'static str {
    match kind {
        QueryIntentKind::SymbolUsage => "symbol-usage",
        QueryIntentKind::SymbolDefinition => "symbol-definition",
        QueryIntentKind::PathLookup => "path-lookup",
        QueryIntentKind::Architecture => "architecture",
        QueryIntentKind::Docs => "docs",
        QueryIntentKind::CodeNavigation => "code-navigation",
        QueryIntentKind::TestLookup => "test-lookup",
        QueryIntentKind::NonCode => "non-code",
    }
}

fn analyze_query_intent(query: &str) -> QueryIntent {
    let lower = query.to_ascii_lowercase();
    let has_path_syntax = has_query_path_syntax(query);
    let symbol_tokens = extract_query_symbol_tokens(query);
    let has_symbol_tokens = !symbol_tokens.is_empty();

    let docs_intent = contains_any(
        &lower,
        &[
            "readme",
            "docs",
            "documentation",
            "guide",
            "design doc",
            "adr",
            "spec",
        ],
    );
    let architecture_intent = contains_any(
        &lower,
        &[
            "architecture",
            "high-level",
            "module",
            "directory",
            "overview",
        ],
    );
    let test_intent = contains_any(
        &lower,
        &[
            "find test",
            "find tests",
            "where are tests",
            "where is test",
            "unit test",
            "unit tests",
            "spec",
            "e2e",
            "integration test",
            "test coverage",
            "test case",
            "test cases",
        ],
    );
    let usage_intent = contains_any(
        &lower,
        &[
            " used",
            "usage",
            "references",
            "called",
            "callers",
            "who calls",
            "where referenced",
            "used by",
            "consumed",
            "consume",
            "consumers",
        ],
    ) && (has_symbol_tokens
        || contains_any(
            &lower,
            &[
                "component",
                "function",
                "hook",
                "feature flag",
                "feature flags",
                "middleware",
                "route",
                "routes",
            ],
        ));
    let definition_intent = has_symbol_tokens
        && contains_any(
            &lower,
            &[
                "define",
                "defined",
                "definition",
                "declared",
                "declaration",
                "imported",
                "imports",
                "exported",
            ],
        );
    let path_intent = has_path_syntax
        || contains_any(
            &lower,
            &[
                "where is",
                "where are",
                "where do",
                "defined",
                "definition",
                "implemented",
                "initialize",
                "initialized",
                "query param",
                "query params",
                "redirect",
                "parse",
                "import",
                "imports",
            ],
        )
        || contains_any_word(
            &lower,
            &[
                "file",
                "files",
                "path",
                "paths",
                "directory",
                "route",
                "routes",
                "service",
                "services",
                "client",
                "clients",
                "helper",
                "helpers",
                "middleware",
                "handler",
                "handlers",
                "websocket",
                "socket",
                "test",
                "tests",
                "spec",
                "e2e",
                "redirect",
                "redirects",
                "import",
                "imports",
                "query",
                "params",
                "localization",
                "translation",
                "translations",
                "i18n",
                "intl",
                "asset",
                "assets",
                "script",
                "scripts",
                "build",
                "tooling",
                "mutation",
                "mutations",
            ],
        );
    let tooling_intent = contains_any_word(
        &lower,
        &[
            "localization",
            "translation",
            "translations",
            "i18n",
            "intl",
            "asset",
            "assets",
            "script",
            "scripts",
            "build",
            "tooling",
            "pipeline",
            "webpack",
            "babel",
        ],
    ) && contains_any(
        &lower,
        &[
            "wired",
            "wiring",
            "generation",
            "generated",
            "generate",
            "script",
            "scripts",
            "build",
            "pipeline",
        ],
    );

    let code_markers = contains_any_word(
        &lower,
        &[
            "repo",
            "code",
            "function",
            "component",
            "module",
            "class",
            "api",
            "routing",
            "auth",
            "login",
            "session",
            "store",
            "state",
            "websocket",
            "socket",
            "middleware",
            "handler",
            "handlers",
            "reducer",
            "reducers",
            "selector",
            "selectors",
            "hook",
            "file",
            "directory",
            "path",
            "test",
            "tests",
            "spec",
            "e2e",
            "define",
            "defined",
            "implemented",
            "initialized",
            "redirect",
            "redirects",
            "import",
            "imports",
            "query",
            "params",
            "parse",
            "localization",
            "translation",
            "translations",
            "i18n",
            "intl",
            "asset",
            "assets",
            "script",
            "scripts",
            "build",
            "tooling",
            "pipeline",
            "mutation",
            "mutations",
            "typescript",
            "javascript",
            "rust",
            "python",
            "go",
        ],
    );
    let non_code_markers = contains_any(
        &lower,
        &[
            "tell me",
            "what is ",
            "what's ",
            "write ",
            "draft ",
            "draft email",
            "write email",
            "linkedin",
            "in simple terms",
            "in general",
            "write a poem",
            "tell me a joke",
            "movie recommendation",
            "translate this",
            "weather",
            "recipe",
            "small talk",
        ],
    );
    let repo_anchor_markers = has_path_syntax
        || has_symbol_tokens
        || contains_any_word(
            &lower,
            &[
                "repo",
                "codebase",
                "project",
                "file",
                "files",
                "path",
                "directory",
                "module",
                "component",
                "function",
                "hook",
                "class",
                "route",
                "routes",
                "routing",
                "auth",
                "session",
                "store",
                "state",
                "api",
                "service",
                "client",
                "controller",
                "websocket",
                "socket",
                "middleware",
                "handler",
                "handlers",
                "reducer",
                "selector",
                "login",
                "redirect",
                "redirects",
                "import",
                "imports",
                "query",
                "params",
                "parse",
                "localization",
                "translation",
                "translations",
                "i18n",
                "intl",
                "asset",
                "assets",
                "script",
                "scripts",
                "build",
                "tooling",
                "pipeline",
                "mutation",
                "mutations",
                "test",
                "tests",
                "spec",
                "e2e",
            ],
        );

    if non_code_markers && !code_markers && !repo_anchor_markers {
        return QueryIntent {
            kind: QueryIntentKind::NonCode,
            code_related: false,
            allow_docs: true,
            weights: IntentWeights {
                lexical: 0.7,
                vector: 0.5,
                symbol: 0.2,
                path: 0.2,
                graph: 0.1,
            },
        };
    }

    if docs_intent {
        return QueryIntent {
            kind: QueryIntentKind::Docs,
            code_related: true,
            allow_docs: true,
            weights: IntentWeights {
                lexical: 1.0,
                vector: 0.8,
                symbol: 0.4,
                path: 0.8,
                graph: 0.3,
            },
        };
    }

    if test_intent {
        return QueryIntent {
            kind: QueryIntentKind::TestLookup,
            code_related: true,
            allow_docs: false,
            weights: IntentWeights {
                lexical: 1.1,
                vector: 0.6,
                symbol: 1.0,
                path: 1.8,
                graph: 0.9,
            },
        };
    }

    if tooling_intent {
        return QueryIntent {
            kind: QueryIntentKind::PathLookup,
            code_related: true,
            allow_docs: false,
            weights: IntentWeights {
                lexical: 1.1,
                vector: 0.5,
                symbol: 0.8,
                path: 1.9,
                graph: 0.8,
            },
        };
    }

    if definition_intent {
        return QueryIntent {
            kind: QueryIntentKind::SymbolDefinition,
            code_related: true,
            allow_docs: false,
            weights: IntentWeights {
                lexical: 1.1,
                vector: 0.45,
                symbol: 2.2,
                path: 1.2,
                graph: 1.0,
            },
        };
    }

    if usage_intent {
        return QueryIntent {
            kind: QueryIntentKind::SymbolUsage,
            code_related: true,
            allow_docs: false,
            weights: IntentWeights {
                lexical: 0.9,
                vector: 0.5,
                symbol: 2.0,
                path: 1.4,
                graph: 1.7,
            },
        };
    }
    if architecture_intent && !definition_intent {
        return QueryIntent {
            kind: QueryIntentKind::Architecture,
            code_related: true,
            allow_docs: true,
            weights: IntentWeights {
                lexical: 1.2,
                vector: 1.1,
                symbol: 0.7,
                path: 0.9,
                graph: 0.5,
            },
        };
    }
    if path_intent {
        return QueryIntent {
            kind: QueryIntentKind::PathLookup,
            code_related: true,
            allow_docs: false,
            weights: IntentWeights {
                lexical: 1.0,
                vector: 0.45,
                symbol: 1.0,
                path: 2.0,
                graph: 0.7,
            },
        };
    }
    if architecture_intent {
        return QueryIntent {
            kind: QueryIntentKind::Architecture,
            code_related: true,
            allow_docs: true,
            weights: IntentWeights {
                lexical: 1.2,
                vector: 1.1,
                symbol: 0.7,
                path: 0.9,
                graph: 0.5,
            },
        };
    }
    if !code_markers
        && !has_symbol_tokens
        && !path_intent
        && !usage_intent
        && !test_intent
        && !tooling_intent
        && !definition_intent
    {
        return QueryIntent {
            kind: QueryIntentKind::NonCode,
            code_related: false,
            allow_docs: true,
            weights: IntentWeights {
                lexical: 0.8,
                vector: 0.6,
                symbol: 0.3,
                path: 0.3,
                graph: 0.2,
            },
        };
    }
    QueryIntent {
        kind: QueryIntentKind::CodeNavigation,
        code_related: true,
        allow_docs: false,
        weights: IntentWeights {
            lexical: 1.0,
            vector: 0.9,
            symbol: 1.1,
            path: 1.0,
            graph: 0.8,
        },
    }
}

fn contains_any(input: &str, patterns: &[&str]) -> bool {
    patterns.iter().any(|p| input.contains(p))
}

fn has_query_path_syntax(query: &str) -> bool {
    if query.contains('/') || query.contains('\\') {
        return true;
    }
    query.split_whitespace().any(|raw| {
        let token = raw.trim_matches(|c: char| {
            matches!(
                c,
                ',' | ';' | ':' | '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '?' | '!'
            )
        });
        if token.is_empty() || !token.contains('.') {
            return false;
        }
        let token = token.trim_end_matches('.');
        if !token.contains('.') {
            return false;
        }
        let parts = token
            .split('.')
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>();
        parts.len() >= 2
            && parts.iter().any(|part| {
                part.chars()
                    .any(|ch| ch.is_ascii_alphabetic() || ch == '_' || ch == '-')
            })
            && parts.iter().all(|part| {
                part.chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
            })
    })
}

fn contains_any_word(input: &str, words: &[&str]) -> bool {
    input
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .any(|token| words.contains(&token))
}

fn extract_query_i18n_keys(query: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut seen = HashSet::new();
    for raw in query.split_whitespace() {
        let candidate = raw
            .trim_matches(|c: char| !c.is_ascii_alphanumeric() && !matches!(c, '.' | '_' | '-'))
            .to_ascii_lowercase();
        if candidate.len() < 8 || !candidate.contains('.') {
            continue;
        }
        let parts = candidate
            .split('.')
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>();
        if parts.len() < 3 {
            continue;
        }
        if parts.iter().all(|part| {
            part.chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
        }) && seen.insert(candidate.clone())
        {
            keys.push(candidate);
        }
    }
    keys
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

fn extract_query_clauses(query: &str) -> Vec<QueryClause> {
    let normalized = query
        .replace(" AND ", " and ")
        .replace(" Then ", " and ")
        .replace(" then ", " and ")
        .replace(" & ", " and ");
    let mut clauses = Vec::new();
    for raw_clause in normalized
        .split(" and ")
        .flat_map(|part| part.split(';'))
        .flat_map(|part| part.split(','))
    {
        let clause = raw_clause.trim();
        if clause.len() < 4 {
            continue;
        }
        let clause_lower = clause.to_ascii_lowercase();
        let kind = if contains_any(
            &clause_lower,
            &["define", "defined", "definition", "declared", "declaration"],
        ) {
            QueryClauseKind::Definition
        } else if contains_any(
            &clause_lower,
            &[
                "used",
                "usage",
                "consumed",
                "consumers",
                "callers",
                "called",
                "imported",
                "imports",
                "rendered",
                "renders",
            ],
        ) {
            QueryClauseKind::Usage
        } else {
            QueryClauseKind::Generic
        };
        let mut tokens = Vec::new();
        let mut seen = HashSet::new();
        for token in extract_query_symbol_tokens(clause)
            .into_iter()
            .chain(extract_query_path_tokens(clause).into_iter())
            .chain(extract_scope_path_hints(clause).into_iter())
        {
            if token.len() < 3 || is_query_noise_token(token.as_str()) {
                continue;
            }
            if seen.insert(token.clone()) {
                tokens.push(token);
            }
        }
        if !tokens.is_empty() {
            clauses.push(QueryClause { kind, tokens });
        }
    }
    clauses.truncate(4);
    clauses
}

fn count_clause_token_matches(
    path_lower: &str,
    text_lower: &str,
    clause: &QueryClause,
    definition_hits: usize,
    symbol_reference_hits: usize,
) -> usize {
    if matches!(clause.kind, QueryClauseKind::Definition) && definition_hits == 0 {
        return 0;
    }
    if matches!(clause.kind, QueryClauseKind::Usage) && symbol_reference_hits == 0 {
        return 0;
    }
    let mut matched = 0usize;
    for token in &clause.tokens {
        if token.len() < 3 {
            continue;
        }
        if path_lower.contains(token) || text_lower.contains(token) {
            matched += 1;
        }
    }
    matched
}

fn contains_path_fragment(path: &str, fragments: &[&str]) -> bool {
    fragments.iter().any(|fragment| path.contains(fragment))
}

fn is_test_like_path(path: &str) -> bool {
    contains_path_fragment(
        path,
        &[
            "/test",
            "/tests/",
            ".test.",
            ".tests.",
            ".spec.",
            ".unit.",
            "mock",
            "fixture",
            "/e2e/",
            "integration",
            "playwright",
        ],
    )
}

fn extract_query_symbol_tokens(query: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
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
    )
}

fn is_generic_path_token(token: &str) -> bool {
    should_keep_plain_path_token(token) || is_low_signal_plain_query_token(token)
}

fn is_experimental_path(lower_path: &str) -> bool {
    lower_path.starts_with("experimental/") || lower_path.contains("/experimental/")
}

fn is_low_signal_code_path(lower_path: &str) -> bool {
    lower_path.starts_with(".semgrep/")
        || lower_path.contains("/.semgrep/")
        || lower_path.ends_with(".semgrep.yaml")
        || lower_path.ends_with(".semgrep.yml")
}

fn is_workflow_docs_query(query_lower: &str) -> bool {
    contains_any(
        query_lower,
        &[
            "workflow",
            "development workflow",
            "daily workflow",
            "onboarding",
            "getting started",
            "developer workflow",
        ],
    )
}

fn is_route_ownership_query(
    query_lower: &str,
    symbol_tokens: &[String],
    intent_kind: QueryIntentKind,
) -> bool {
    !symbol_tokens.is_empty()
        && contains_any(query_lower, &["render", "renders", "rendered"])
        && contains_any(query_lower, &["route", "routes", "router", "routing"])
        && (contains_any(query_lower, &["define", "defined", "definition"])
            || matches!(intent_kind, QueryIntentKind::SymbolDefinition))
}

fn augment_path_tokens_for_intent(
    query: &str,
    intent: &QueryIntent,
    path_tokens: &mut Vec<String>,
) {
    let lower = query.to_ascii_lowercase();
    match intent.kind {
        QueryIntentKind::PathLookup
        | QueryIntentKind::CodeNavigation
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
                        "bootstrap",
                        "state",
                        "selector",
                        "selectors",
                        "reducer",
                        "reducers",
                    ],
                );
            }
            if contains_any(
                &lower,
                &[
                    "route",
                    "routing",
                    "router",
                    "page",
                    "endpoint",
                    "endpoints",
                    "blueprint",
                ],
            ) {
                add_path_tokens(
                    path_tokens,
                    &[
                        "route",
                        "routes",
                        "router",
                        "routing",
                        "pages",
                        "endpoint",
                        "endpoints",
                        "blueprint",
                    ],
                );
            }
            if contains_any(&lower, &["auth", "session", "login", "token"]) {
                add_path_tokens(
                    path_tokens,
                    &["auth", "session", "sessions", "login", "token", "tokens"],
                );
            }
            if contains_any(
                &lower,
                &[
                    "redirect",
                    "redirects",
                    "query param",
                    "query params",
                    "import",
                    "imports",
                    "parse",
                    "parsed",
                ],
            ) {
                add_path_tokens(
                    path_tokens,
                    &[
                        "redirect",
                        "redirects",
                        "route",
                        "routes",
                        "query",
                        "params",
                        "search",
                        "path",
                        "paths",
                        "import",
                        "imports",
                    ],
                );
            }
            if contains_any(&lower, &["websocket", "socket", "ws", "message handler"]) {
                add_path_tokens(
                    path_tokens,
                    &[
                        "websocket",
                        "socket",
                        "ws",
                        "connection",
                        "connections",
                        "handler",
                        "handlers",
                        "listener",
                        "listeners",
                    ],
                );
            }
            if contains_any(&lower, &["feature flag", "feature flags"]) {
                add_path_tokens(
                    path_tokens,
                    &[
                        "feature",
                        "features",
                        "flag",
                        "flags",
                        "featureflags",
                        "feature-flags",
                        "feature_flags",
                        "usefeatureflag",
                        "isfeatureenabled",
                    ],
                );
            }
            if contains_any(
                &lower,
                &[
                    "mutation",
                    "mutations",
                    "mutate",
                    "release plan",
                    "release plans",
                    "graphql",
                ],
            ) {
                add_path_tokens(
                    path_tokens,
                    &[
                        "mutation",
                        "mutations",
                        "mutate",
                        "graphql",
                        "hooks",
                        "hook",
                        "releaseplan",
                        "release-plan",
                        "release_plan",
                    ],
                );
            }
            if contains_any(
                &lower,
                &[
                    "translation",
                    "translations",
                    "locale",
                    "locales",
                    "i18n",
                    "intl",
                    "message key",
                    "message keys",
                ],
            ) {
                add_path_tokens(
                    path_tokens,
                    &[
                        "locale",
                        "locales",
                        "i18n",
                        "intl",
                        "message",
                        "messages",
                        "translation",
                        "translations",
                        "assets",
                        "json",
                    ],
                );
            }
            if contains_any(
                &lower,
                &[
                    "test",
                    "tests",
                    "spec",
                    "e2e",
                    "integration",
                    "test case",
                    "test cases",
                ],
            ) {
                add_path_tokens(
                    path_tokens,
                    &[
                        "test",
                        "tests",
                        "spec",
                        "specs",
                        "unit",
                        "e2e",
                        "integration",
                        "playwright",
                        "mock",
                        "fixture",
                    ],
                );
            }
            if matches!(intent.kind, QueryIntentKind::Architecture) {
                add_path_tokens(
                    path_tokens,
                    &[
                        "docs",
                        "readme",
                        "architecture",
                        "overview",
                        "bootstrap",
                        "routing",
                        "router",
                        "app",
                        "apps",
                        "store",
                    ],
                );
            }
        }
        QueryIntentKind::Docs => {
            add_path_tokens(
                path_tokens,
                &[
                    "docs",
                    "readme",
                    "architecture",
                    "guide",
                    "adr",
                    "spec",
                    "design",
                ],
            );
        }
        _ => {}
    }
}

fn add_path_tokens(path_tokens: &mut Vec<String>, additions: &[&str]) {
    let mut seen: HashSet<String> = path_tokens.iter().cloned().collect();
    for token in additions {
        let owned = (*token).to_string();
        if seen.insert(owned.clone()) {
            path_tokens.push(owned);
        }
    }
}

fn add_dynamic_path_tokens(path_tokens: &mut Vec<String>, additions: &[String]) {
    let mut seen: HashSet<String> = path_tokens.iter().cloned().collect();
    for token in additions {
        if token.is_empty() {
            continue;
        }
        let owned = token.to_ascii_lowercase();
        if seen.insert(owned.clone()) {
            path_tokens.push(owned);
        }
    }
}

fn count_symbol_definition_hits(chunk_text_lower: &str, symbol_tokens: &[String]) -> usize {
    symbol_tokens
        .iter()
        .filter(|symbol| chunk_contains_symbol_definition(chunk_text_lower, symbol.as_str()))
        .count()
}

fn count_symbol_import_hits(chunk_text_lower: &str, symbol_tokens: &[String]) -> usize {
    symbol_tokens
        .iter()
        .filter(|symbol| chunk_contains_symbol_import(chunk_text_lower, symbol.as_str()))
        .count()
}

fn count_symbol_reference_hits(chunk_text_lower: &str, symbol_tokens: &[String]) -> usize {
    symbol_tokens
        .iter()
        .filter(|symbol| chunk_contains_symbol_reference(chunk_text_lower, symbol.as_str()))
        .count()
}

fn chunk_contains_symbol_definition(chunk_text_lower: &str, symbol: &str) -> bool {
    if symbol.len() < 3 {
        return false;
    }
    for raw_line in chunk_text_lower.lines() {
        let line = raw_line.trim_start();
        if line_starts_with_symbol_binding(line, "const ", symbol)
            || line_starts_with_symbol_binding(line, "let ", symbol)
            || line_starts_with_symbol_binding(line, "var ", symbol)
            || line_starts_with_symbol_binding(line, "function ", symbol)
            || line_starts_with_symbol_binding(line, "class ", symbol)
            || line_starts_with_symbol_binding(line, "type ", symbol)
            || line_starts_with_symbol_binding(line, "interface ", symbol)
            || line_starts_with_symbol_binding(line, "enum ", symbol)
            || line_starts_with_symbol_binding(line, "export const ", symbol)
            || line_starts_with_symbol_binding(line, "export function ", symbol)
            || line_starts_with_symbol_binding(line, "export default ", symbol)
            || line_starts_with_symbol_binding(line, "pub fn ", symbol)
            || line_starts_with_symbol_binding(line, "fn ", symbol)
            || line_starts_with_symbol_family_binding(line, "const ", symbol)
            || line_starts_with_symbol_family_binding(line, "let ", symbol)
            || line_starts_with_symbol_family_binding(line, "var ", symbol)
            || line_starts_with_symbol_family_binding(line, "function ", symbol)
            || line_starts_with_symbol_family_binding(line, "class ", symbol)
            || line_starts_with_symbol_family_binding(line, "type ", symbol)
            || line_starts_with_symbol_family_binding(line, "interface ", symbol)
            || line_starts_with_symbol_family_binding(line, "enum ", symbol)
            || line_starts_with_symbol_family_binding(line, "export const ", symbol)
            || line_starts_with_symbol_family_binding(line, "export function ", symbol)
            || line_starts_with_symbol_family_binding(line, "export default ", symbol)
            || line_starts_with_symbol_family_binding(line, "pub fn ", symbol)
            || line_starts_with_symbol_family_binding(line, "fn ", symbol)
        {
            return true;
        }
    }
    false
}

fn chunk_contains_symbol_import(chunk_text_lower: &str, symbol: &str) -> bool {
    if symbol.len() < 3 {
        return false;
    }
    for raw_line in chunk_text_lower.lines() {
        let line = raw_line.trim_start();
        let is_import_like = line.starts_with("import ")
            || line.starts_with("export {")
            || line.starts_with("export type {");
        if !is_import_like {
            continue;
        }
        if contains_identifier_with_boundaries(line, symbol)
            || contains_symbol_family_token(line, symbol)
        {
            return true;
        }
    }
    false
}

fn chunk_contains_symbol_reference(chunk_text_lower: &str, symbol: &str) -> bool {
    contains_identifier_with_boundaries(chunk_text_lower, symbol)
        || contains_symbol_family_token(chunk_text_lower, symbol)
}

fn contains_identifier_with_boundaries(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let bytes = haystack.as_bytes();
    let mut start = 0usize;
    while start < haystack.len() {
        let Some(offset) = haystack[start..].find(needle) else {
            break;
        };
        let idx = start + offset;
        let before_ok = if idx == 0 {
            true
        } else {
            let ch = bytes[idx - 1] as char;
            !ch.is_ascii_alphanumeric() && ch != '_'
        };
        let after_idx = idx + needle.len();
        let after_ok = if after_idx >= bytes.len() {
            true
        } else {
            let ch = bytes[after_idx] as char;
            !ch.is_ascii_alphanumeric() && ch != '_'
        };
        if before_ok && after_ok {
            return true;
        }
        start = idx + 1;
    }
    false
}

fn contains_symbol_family_token(haystack: &str, symbol: &str) -> bool {
    if symbol.len() < 7 {
        return false;
    }
    haystack
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|token| !token.is_empty())
        .any(|token| is_symbol_family_match(token, symbol))
}

fn line_starts_with_symbol_binding(line: &str, prefix: &str, symbol: &str) -> bool {
    let Some(rest) = line.strip_prefix(prefix) else {
        return false;
    };
    let rest = rest.trim_start();
    if !rest.starts_with(symbol) {
        return false;
    }
    let after = rest[symbol.len()..].chars().next();
    after.is_none_or(|ch| !ch.is_ascii_alphanumeric() && ch != '_')
}

fn line_starts_with_symbol_family_binding(line: &str, prefix: &str, symbol: &str) -> bool {
    let Some(rest) = line.strip_prefix(prefix) else {
        return false;
    };
    let rest = rest.trim_start();
    let identifier = rest
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
        .collect::<String>();
    !identifier.is_empty() && is_symbol_family_match(identifier.as_str(), symbol)
}

fn is_symbol_family_match(candidate: &str, query_symbol: &str) -> bool {
    let candidate_norm = normalize_symbol_family(candidate);
    let query_norm = normalize_symbol_family(query_symbol);
    if candidate_norm.is_empty() || query_norm.is_empty() {
        return false;
    }
    if candidate_norm == query_norm {
        return true;
    }
    if candidate_norm.len() < 7 || query_norm.len() < 7 {
        return false;
    }
    candidate_norm.starts_with(query_norm.as_str())
        || query_norm.starts_with(candidate_norm.as_str())
}

fn normalize_symbol_family(token: &str) -> String {
    let mut normalized = token.trim_matches('_').to_ascii_lowercase();
    if normalized.ends_with('s') && normalized.len() > 4 {
        normalized.pop();
    }
    normalized
}

fn extract_explicit_doc_path_hints(query: &str) -> Vec<String> {
    let mut hints = Vec::new();
    let mut seen = HashSet::new();
    for raw in query.split_whitespace() {
        let candidate = raw
            .trim_matches(|c: char| {
                !c.is_ascii_alphanumeric() && !matches!(c, '/' | '_' | '-' | '.')
            })
            .trim_start_matches("./")
            .to_ascii_lowercase();
        if candidate.len() < 4 {
            continue;
        }
        let is_doc_path_like = candidate.contains('/')
            || candidate.ends_with(".md")
            || candidate.ends_with(".mdx")
            || candidate.contains("readme");
        if is_doc_path_like && seen.insert(candidate.clone()) {
            hints.push(candidate);
        }
    }
    hints
}

fn matches_doc_path_hint(path: &str, hints: &[String]) -> bool {
    if hints.is_empty() {
        return false;
    }
    let lower_path = path.to_ascii_lowercase();
    hints.iter().any(|hint| lower_path.contains(hint))
}

fn build_context(snippets: &[QueryResultItem], budget: usize) -> String {
    let mut out = String::new();
    out.push_str("[budi deterministic context]\n");
    out.push_str("snippets:\n");

    for snippet in snippets {
        let header = format!(
            "### {}:{}-{} score={:.4} reason={}\n",
            snippet.path, snippet.start_line, snippet.end_line, snippet.score, snippet.reason
        );
        if out.len() + header.len() >= budget {
            break;
        }
        out.push_str(&header);
        let mut body = snippet.text.clone();
        body.push('\n');
        if out.len() + body.len() > budget {
            let remaining = budget.saturating_sub(out.len());
            let truncated = body.chars().take(remaining).collect::<String>();
            out.push_str(&truncated);
            break;
        }
        out.push_str(&body);
    }
    out
}

fn normalize_path(input: &str) -> String {
    input
        .replace('\\', "/")
        .trim_start_matches("./")
        .to_string()
}

fn snippet_fingerprint(text: &str) -> String {
    let normalized = text
        .split_whitespace()
        .take(80)
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    blake3::hash(normalized.as_bytes()).to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_symbol_usage_intent() {
        let intent = analyze_query_intent("Where is CameraPreviewPanel used?");
        assert!(matches!(intent.kind, QueryIntentKind::SymbolUsage));
        assert!(intent.code_related);
        assert!(!intent.allow_docs);
    }

    #[test]
    fn detects_non_code_intent() {
        let intent = analyze_query_intent("Tell me a joke about cats");
        assert!(matches!(intent.kind, QueryIntentKind::NonCode));
        assert!(!intent.code_related);
    }

    #[test]
    fn detects_general_non_repo_prompt_as_non_code() {
        let intent = analyze_query_intent("Write a polite email asking for deadline extension.");
        assert!(matches!(intent.kind, QueryIntentKind::NonCode));
        assert!(!intent.code_related);
    }

    #[test]
    fn detects_websocket_prompt_as_code_related() {
        let intent = analyze_query_intent("Where is websocket connection initialized?");
        assert!(!matches!(intent.kind, QueryIntentKind::NonCode));
        assert!(intent.code_related);
    }

    #[test]
    fn detects_test_lookup_prompt() {
        let intent = analyze_query_intent("Find tests for LoginWithPasskey");
        assert!(matches!(intent.kind, QueryIntentKind::TestLookup));
        assert!(intent.code_related);
    }

    #[test]
    fn detects_symbol_definition_intent() {
        let intent =
            analyze_query_intent("Which file defines DEFAULT_ENTRY_PATH and where is it imported?");
        assert!(matches!(intent.kind, QueryIntentKind::SymbolDefinition));
        assert!(intent.code_related);
    }

    #[test]
    fn non_code_marker_with_symbol_anchor_stays_code_related() {
        let intent = analyze_query_intent("What is LoginWithPasskey?");
        assert!(!matches!(intent.kind, QueryIntentKind::NonCode));
    }

    #[test]
    fn redirect_query_param_prompt_is_code_related() {
        let intent = analyze_query_intent("Where do we parse query params for login redirects?");
        assert!(!matches!(intent.kind, QueryIntentKind::NonCode));
        assert!(intent.code_related);
    }

    #[test]
    fn extracts_scope_hints_for_in_phrase() {
        let hints = extract_scope_path_hints("Where is useFeatureToggle consumed in portal-app?");
        assert!(hints.iter().any(|hint| hint == "portal-app"));
    }

    #[test]
    fn extracts_multi_clause_query_tokens() {
        let clauses = extract_query_clauses(
            "Where is useFeatureToggle defined and where is it consumed in portal-app?",
        );
        assert!(clauses.len() >= 2);
        assert!(
            clauses
                .iter()
                .any(|clause| clause.tokens.iter().any(|token| token == "portal-app"))
        );
    }

    #[test]
    fn extract_symbol_tokens_for_use_feature_flag_query() {
        let tokens = extract_query_symbol_tokens(
            "Where is useFeatureToggle defined and where is it consumed in portal-app?",
        );
        assert_eq!(tokens, vec!["usefeaturetoggle".to_string()]);
    }

    #[test]
    fn extracts_i18n_key_from_query() {
        let keys = extract_query_i18n_keys(
            "Where are translation keys for app.dashboard.ui.SettingsPage?",
        );
        assert!(
            keys.iter()
                .any(|key| key == "app.dashboard.ui.settingspage")
        );
    }

    #[test]
    fn symbol_definition_detection_uses_identifier_boundaries() {
        assert!(!chunk_contains_symbol_definition(
            "const enabled = usefeaturetoggle(featuretogglekey);",
            "usefeaturetoggle",
        ));
        assert!(chunk_contains_symbol_definition(
            "export const usefeaturetogglessetreleasebatchmutation = () => {};",
            "usefeaturetoggle",
        ));
        assert!(chunk_contains_symbol_definition(
            "export function usefeaturetoggle(featuretogglekey: string) { return true; }",
            "usefeaturetoggle",
        ));
    }

    #[test]
    fn symbol_family_match_handles_feature_flag_pluralization() {
        assert!(is_symbol_family_match(
            "usefeaturetogglessetreleasebatchmutation",
            "usefeaturetoggle"
        ));
    }

    #[test]
    fn symbol_definition_does_not_treat_import_usage_as_definition() {
        let chunk = r#"
import react from 'react';
import { featuretogglekeys, usefeaturetoggle } from '@acme/feature-toggles';

export default function devicerowcell() {
  const isopensearchenabled = usefeaturetoggle(featuretogglekeys.foo);
  return null;
}
"#;
        assert!(!chunk_contains_symbol_definition(chunk, "usefeaturetoggle"));
        assert!(chunk_contains_symbol_import(chunk, "usefeaturetoggle"));
    }

    #[test]
    fn favors_symbol_channel_for_usage_queries() {
        let intent = QueryIntent {
            kind: QueryIntentKind::SymbolUsage,
            code_related: true,
            allow_docs: false,
            weights: IntentWeights {
                lexical: 0.9,
                vector: 0.5,
                symbol: 2.0,
                path: 1.4,
                graph: 1.7,
            },
        };
        // Candidate 1 wins lexical, candidate 2 wins symbol. Usage intent should favor candidate 2.
        let lexical = vec![(1, 1.0), (2, 0.3)];
        let vector = vec![(1, 0.6)];
        let symbol = vec![(2, 1.5)];
        let path = vec![(2, 0.9)];
        let graph = vec![(2, 1.2)];
        let fused = fuse_channel_scores(&lexical, &vector, &symbol, &path, &graph, &intent);
        let c1 = fused.get(&1).map(|c| c.score).unwrap_or_default();
        let c2 = fused.get(&2).map(|c| c.score).unwrap_or_default();
        assert!(
            c2 > c1,
            "expected symbol/path-heavy candidate to outrank lexical one"
        );
    }

    #[test]
    fn graph_channel_can_beat_lexical_for_usage_intent() {
        let intent = QueryIntent {
            kind: QueryIntentKind::SymbolUsage,
            code_related: true,
            allow_docs: false,
            weights: IntentWeights {
                lexical: 0.9,
                vector: 0.5,
                symbol: 2.0,
                path: 1.4,
                graph: 1.7,
            },
        };
        let lexical = vec![(1, 1.0)];
        let vector = Vec::new();
        let symbol = Vec::new();
        let path = Vec::new();
        let graph = vec![(2, 1.3)];
        let fused = fuse_channel_scores(&lexical, &vector, &symbol, &path, &graph, &intent);
        let lexical_candidate = fused.get(&1).map(|c| c.score).unwrap_or_default();
        let graph_candidate = fused.get(&2).map(|c| c.score).unwrap_or_default();
        assert!(graph_candidate > lexical_candidate);
    }

    #[test]
    fn docs_query_allows_docs() {
        let docs = analyze_query_intent("Show me docs/readme for auth bootstrap");
        let code = analyze_query_intent("Where is auth bootstrap implemented?");
        assert!(docs.allow_docs);
        assert!(!code.allow_docs);
    }

    #[test]
    fn path_tokens_keep_plain_route_keywords() {
        let tokens = extract_query_path_tokens("Where are route definitions for command pages?");
        assert!(tokens.iter().any(|t| t == "route" || t == "routes"));
    }

    #[test]
    fn path_tokens_keep_specific_plain_terms() {
        let tokens = extract_query_path_tokens("Where are SCIM endpoints defined?");
        assert!(tokens.iter().any(|t| t == "scim"));
        assert!(tokens.iter().all(|t| t != "defined"));
    }

    #[test]
    fn path_tokens_drop_low_signal_plain_terms() {
        let tokens = extract_query_path_tokens("Where is request handling implemented?");
        assert!(tokens.iter().any(|t| t == "request"));
        assert!(tokens.iter().all(|t| t != "implemented"));
        assert!(tokens.iter().all(|t| t != "handling"));
    }

    #[test]
    fn path_tokens_keep_short_specific_terms() {
        let tokens = extract_query_path_tokens("Show tests for moq API endpoints.");
        assert!(tokens.iter().any(|t| t == "moq"));
        assert!(tokens.iter().all(|t| t != "unit"));
    }

    #[test]
    fn path_tokens_generate_compound_tokens_for_hyphenated_query() {
        let tokens = extract_query_path_tokens("Where are logged-out routes defined?");
        assert!(tokens.iter().any(|t| t == "loggedout"));
        assert!(tokens.iter().any(|t| t == "loggedoutroutes"));
    }

    #[test]
    fn doc_path_hint_matching_works() {
        let hints =
            extract_explicit_doc_path_hints("Summarize src/command/alarms-v3/docs/models.md");
        assert!(matches_doc_path_hint(
            "src/command/alarms-v3/docs/models.md",
            &hints
        ));
    }

    #[test]
    fn docs_signals_raise_confidence() {
        let intent = QueryIntent {
            kind: QueryIntentKind::Docs,
            code_related: true,
            allow_docs: true,
            weights: IntentWeights {
                lexical: 1.0,
                vector: 0.8,
                symbol: 0.4,
                path: 0.8,
                graph: 0.3,
            },
        };
        let confidence = estimate_confidence(
            0.30,
            0.01,
            1,
            &["lexical-hit".to_string(), "doc-path-hit".to_string()],
            &intent,
        );
        assert!(confidence >= 0.45);
    }

    #[test]
    fn path_signals_raise_path_lookup_confidence() {
        let intent = QueryIntent {
            kind: QueryIntentKind::PathLookup,
            code_related: true,
            allow_docs: false,
            weights: IntentWeights {
                lexical: 1.0,
                vector: 1.0,
                symbol: 1.0,
                path: 1.0,
                graph: 0.7,
            },
        };
        let confidence = estimate_confidence(
            0.18,
            0.03,
            3,
            &["path-hit".to_string(), "symbol-hit".to_string()],
            &intent,
        );
        assert!(confidence >= 0.45);
    }

    #[test]
    fn confidence_below_threshold_recommends_skip() {
        let intent = QueryIntent {
            kind: QueryIntentKind::CodeNavigation,
            code_related: true,
            allow_docs: false,
            weights: IntentWeights {
                lexical: 1.0,
                vector: 1.0,
                symbol: 1.0,
                path: 1.0,
                graph: 0.8,
            },
        };
        let diagnostics = build_diagnostics(&intent, &[], true, true, 0.45);
        assert!(!diagnostics.recommended_injection);
        assert!(diagnostics.skip_reason.is_some());
    }

    #[test]
    fn recognizes_experimental_paths() {
        assert!(is_experimental_path("experimental/notes/readme.md"));
        assert!(is_experimental_path("foo/experimental/bar/readme.md"));
        assert!(!is_experimental_path("docs/architecture/readme.md"));
    }

    #[test]
    fn recognizes_low_signal_semgrep_paths() {
        assert!(is_low_signal_code_path(".semgrep/routes.yaml"));
        assert!(is_low_signal_code_path("vinter/.semgrep/routes.yaml"));
        assert!(!is_low_signal_code_path("docs/semgrep-guide.md"));
    }

    #[test]
    fn localization_asset_generation_prompt_is_code_related() {
        let intent = analyze_query_intent("How is localization asset generation wired?");
        assert!(!matches!(intent.kind, QueryIntentKind::NonCode));
        assert!(intent.code_related);
    }

    #[test]
    fn retrieval_policy_prunes_advanced_focus_and_config_rules() {
        let policy = retrieval_policy(QueryIntentKind::PathLookup);
        assert!(!policy.apply_route_ownership_rules);
    }

    #[test]
    fn retrieval_policy_keeps_symbol_definition_route_ownership() {
        let policy = retrieval_policy(QueryIntentKind::SymbolDefinition);
        assert!(policy.apply_route_ownership_rules);
    }

    #[test]
    fn detects_workflow_docs_queries() {
        assert!(is_workflow_docs_query(
            "show docs explaining daily development workflow"
        ));
        assert!(!is_workflow_docs_query(
            "show docs for camera auth endpoints"
        ));
    }

    #[test]
    fn detects_route_ownership_queries() {
        let symbols = vec!["settingsrouterpage".to_string()];
        assert!(is_route_ownership_query(
            "which route renders settingsrouterpage and where is that page defined?",
            &symbols,
            QueryIntentKind::SymbolDefinition
        ));
        assert!(!is_route_ownership_query(
            "which route renders the page and where is that page defined?",
            &[],
            QueryIntentKind::PathLookup
        ));
    }

    #[test]
    fn build_context_excludes_git_metadata_lines() {
        let snippets = vec![QueryResultItem {
            path: "src/app.ts".to_string(),
            start_line: 10,
            end_line: 14,
            score: 0.87,
            reason: "symbol-hit".to_string(),
            text: "export const app = true;".to_string(),
        }];
        let context = build_context(&snippets, 4_000);
        assert!(context.contains("[budi deterministic context]"));
        assert!(context.contains("snippets:"));
        assert!(!context.contains("branch:"));
        assert!(!context.contains("recent_commits:"));
        assert!(!context.contains("dirty_files:"));
    }

    #[test]
    fn path_diversity_bucket_uses_first_two_segments() {
        assert_eq!(path_diversity_bucket("src/routes/home.tsx"), "src/routes");
        assert_eq!(path_diversity_bucket("README.md"), "readme.md");
    }
}
