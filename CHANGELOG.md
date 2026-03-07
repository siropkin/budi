# Changelog

All notable changes to budi are documented here.

## [2.8.0] — Phase X: Stress Tests + Stability

- Stress test suite (`scripts/dev/stress_test.py`): 7 concurrent scenarios covering query storms, incremental indexing, session dedup, and file churn
- Fixed latent UTF-8 panic in `truncate_to()` — now walks back to char boundary
- Fixed `is_test_path()` to detect `__tests__/` and `__spec__/` (Jest/Mocha conventions)
- Added 75+ unit tests across `retrieval.rs`, `context.rs`, and `chunking.rs`
- Enabled budi-on-budi telemetry for self-dogfooding

## [2.7.0] — Phase T: Score Floor Tuning

- Raised FlowTrace score floor: 0.20 → 0.25
- Added SymbolUsage score floor: 0.22
- Reduced SymbolUsage retrieval limit: 6 → 5
- Fixes: P7 (reconcileChildFibers) context reduced from 1968 to 845 chars; P15 reduced from 3154 to 1851
- React benchmark: **15/18 wins (83%)**

## [2.6.0] — Phase S: Symbol-Definition Accuracy

- `dominant_symbol_hint()`: picks the symbol spanning the most lines in a chunk window — prevents short helpers from stealing the hint from a dominant function
- Hint-match boost (+0.30): fires when intent is SymbolDefinition and chunk's symbol_hint exactly matches a query token
- Extended `looks_like_symbol()` for exported/async JS/TS functions
- Fixed scheduleUpdateOnFiber detection (P3 now wins consistently)

## [2.5.0] — Phase R: Test Path Boost

- `is_test_path()`: detects `/test`, `/spec`, `__tests__/`, `__spec__/` — test files get +0.15 score boost on TestLookup queries
- TestLookup score floor: 0.22
- Multi-pass judge support in benchmark runner (`--judge-passes 3`)

## [2.4.0] — Phase P: Classifier Fixes

- SymbolUsage keywords added: "who constructs", "who creates", "who instantiates", "who builds"
- RuntimeConfig tightened: removed bare `"config"` and `"env"` triggers; replaced with `"config file"`, `"load config"`, `"read config"`, `"env var"`
- Fixes false-positive routing of "configured"/"configuration" queries to RuntimeConfig

## [2.3.0] — Phase N: FlowTrace + SymbolDefinition Classifier Fixes

- FlowTrace keywords added: "cleanup order", "cleanup sequence", "unmount order", "lifecycle order", "removal order", "what order" — fixes P10 misclassification
- SymbolDefinition score floor: 0.20 minimum
- 18-prompt benchmark suite (expanded from 12)
- React benchmark: **16/18 wins**

## [2.2.0] — Phase M: Richer Session Memory

- `AffinityEntry { ts, anchors }`: stores up to 2 representative code lines per file alongside the timestamp
- Session-start format: `- src/file.js — anchor1; anchor2`
- Auto-migration from old flat `HashMap<String, u64>` format

## [2.1.0] — Phase L: Context Budget Discipline

- Total injected context hard-capped at `context_char_budget` (12k chars)
- Call graph budget subtracted before snippet context assembly
- FlowTrace call graph gated on top-snippet confidence: score ≥ 0.30 → 1200 chars, else 600

## [2.0.0] — Phase K: Intent-Aware Context Assembly

- Per-intent call graph budgets (FlowTrace 1200, SymbolDef/Usage 800, Architecture/Test 0)
- Per-intent retrieval limits via `intent_retrieval_limit()`
- Extended proof needles: `call(`, `invoke`, `schedule`, `commit`

## [1.9.0] — Phase J: Cross-Session File Affinity

- `session-affinity.json`: persists injected file paths across sessions (top 50 by recency)
- Session-start system message includes "## Recently Relevant Files" block (top 5)
- `update_session_affinity()` runs via `spawn_blocking` — zero latency impact on queries

## [1.8.0] — Phase I: Per-Step Timing

- `QueryResponse.timing_ms`: pipeline step durations (load/embed/retrieval/dedup/callgraph)
- Emitted when `debug_io = true`
- CLI logs timing as `"timing"` key in `hook-io.jsonl`

## [1.7.0] — Phase H: Query Intent Routing

- `classify_intent(prompt)` → 7 intent kinds
- `weights_for_intent(kind)`: per-intent 5-channel blend adjustments
- `detected_intent` field in `QueryResponse`
- React benchmark: **9/12 wins**

## [1.6.0] — Phase G: Expanded Benchmark Prompts

- 18-prompt React benchmark suite covering 6 intent categories
- `fixtures/benchmarks/react-structural-v1.prompts.json`

## [1.5.0] — Phase F: Evidence Cards Format

- Context output format changed to YAML-like evidence cards
- Rules block explains to Claude how to use the injected snippets
- Score and signal annotations per snippet

## [1.4.0] — Phase E: Call Graph / Structural Oracle

- `chunk_to_graph_tokens` forward index in RuntimeIndex
- `build_call_graph_summary()` generates `[structural context]` block
- Progressive truncation: top snippet ≤ 40% of budget, each next ≤ 60% of remaining
- React benchmark: **5/5 wins** (Phase E AB test)

## [1.3.0] — Phase D: Project Map

- `generate_project_map()` writes `.claude/budi-project-map.md` after each full index
- `budi hook session-start` serves the project map as system prompt injection

## [1.2.0] — Phase B: PostToolUse Prefetch

- `/prefetch-neighbors` endpoint: graph neighbors for a file, triggered by Read/Glob hooks
- `PostToolUse` hook installed by `budi init`

## [1.1.0] — Phase A: Session Deduplication

- `session_id` in `QueryRequest` enables cross-prompt dedup within a session
- Session state TTL: 30 minutes
- Deduplication by `path:start_line` to avoid re-injecting seen snippets

## [1.0.0] — Initial Release

- 5-channel hybrid retrieval: lexical (BM25/Tantivy), vector (HNSW/fastembed), symbol, path, graph
- `budi init` + `budi index` setup flow
- `UserPromptSubmit` hook for context injection
- `debug_io` telemetry flag
