# Changelog

All notable changes to budi are documented here.

## [3.4.0] — Product Polish & Express Benchmark

Language-aware .budiignore templates, improved error messages, cleaner search output, and Express.js benchmark validation.

### Improvements

- **Language-aware .budiignore templates**: `budi init` now detects project languages (JS/TS, Python, Rust, Go, Java, Ruby, PHP, C#) and generates a tailored .budiignore with common exclusion patterns. Large repos get hints to exclude tests/docs.
- **Better error messages**: Daemon startup failures now show the daemon log path. Git repo detection errors suggest `git init` or `--repo-root`.
- **Cleaner search output**: `budi repo search` now shows numbered results with indented reasons. Channel scores moved to `--json` only. Removed misleading 0% confidence display.

### Benchmark

- **Express benchmark**: 18 prompts validated — 16/18 (89%) non-regressions, -5% cost, **0% injection regression**
- Total: 9 repos, 141 judged prompts, ~82% single-run non-regression rate

## [3.3.0] — Retrieval Precision & Stability

SymbolUsage test-path filtering, embedding warmup, daemon stability, and RuntimeConfig noise reduction.

### Retrieval improvements

- **SymbolUsage test-path penalty** (-0.25): Test files declaring variables like `let scheduleCallback;` no longer surface as caller results. All 6 intent types now have test-path handling.
- **RuntimeConfig lexical-only demotion** (-0.10): Pure-lexical RuntimeConfig hits with zero semantic signal are filtered, reducing keyword false positives
- **FlowTrace pipeline skip threshold** raised (0.50 → 0.55): Prevents low-confidence partial chain injection

### Fixes

- **Daemon startup warmup**: ONNX embedding runtime primed on daemon startup (~57ms), eliminating cold-cache score variance
- **Daemon panic on absolute paths**: Fixed autosync watcher crash when receiving absolute or parent-traversal paths
- **SessionStart hook**: Removed project map terminal output that printed during startup
- **Status line**: Updated logo placement and fixed cargo fmt formatting
- **Channel score diagnostics**: `dump_candidates` field now passed through hook queries for weight analysis

### Benchmark state

- 80% non-regression rate across 123 judged prompts (7 repos, single-run)
- When budi injects context, regression rate is ~10-12% (remaining losses are LLM variance on skipped queries)
- All remaining regressions are mild (Q -1) and irreducible

## [3.2.0] — Smarter Indexing & Hooks

HTTP hooks, BM25 identifier splitting, 5 new language grammars, and incremental index updates.

### New features

- **HTTP hooks**: UserPromptSubmit and PostToolUse hooks now use HTTP instead of CLI subprocesses, eliminating ~50-100ms per-prompt overhead
- **BM25 identifier splitting**: camelCase/PascalCase/snake_case identifiers split into lowercase words for better natural-language matching (e.g. "LiveVideo" → "live video")
- **Cross-encoder reranker** (opt-in): ms-marco-MiniLM-L-6-v2 ONNX int8 reranker, disabled by default. Config: `reranker_enabled`, `reranker_alpha`
- **Incremental index updates**: `apply_delta()` inserts new embeddings into existing HNSW graph instead of full rebuild (2.5x faster for single-file changes)
- **Git worktree support**: All worktrees share one index directory
- **5 new language grammars**: Ruby, Kotlin, Swift, Scala, PHP now have AST-aware chunking (13 languages total)
- **Local budiignore**: Per-repo `budiignore.local` stored in budi's data dir (no repo changes needed)
- **Status line**: `budi statusline` shows live stats in Claude Code status bar

### Retrieval improvements

- i18n `defineMessages()` demotion (-0.25) for React monorepos
- ESLint plugin path penalty extended
- TypeScript interface/struct/enum proof lines now extract property names
- Daemon hook error logging for silent failures

### Fixes

- Status line alignment with Claude Code (padding 1 → 0)
- Doctor no longer shows `[!!]` for optional config file
- Removed unused Ollama install from install.sh
- Fixed `embedding_batch_size` default in docs (96 → 32)

### Benchmark state

- 91% non-regression rate across 131 judged prompts (8 repos)
- BM25 identifier splitting validated: no new regressions, 26-37% cost savings on focused sweep
- Cross-encoder reranker evaluated at alpha=0.5: inflates scores but doesn't reliably improve rankings (needs code-aware base model)

## [3.1.0] — Polish & Precision

Post-v3.0.0 quality improvements, new features, and better project maps.

### New features

- **Session-end summary**: Stop hook prints injection rate and read hit rate to stderr
- **MCP server** (`budi-mcp`): Thin stdio MCP server for Cursor, Zed, Windsurf, and other editors
- **Post-injection feedback**: Tracks which files Claude reads that budi already injected (read hit rate)
- **Activity summary in `budi doctor`**: Shows queries, injections, and read hit rate

### Retrieval improvements

- Devtools-path penalty now applies to all intent types (was Architecture-only)
- Build-infra path penalty (-0.20) for rollup/webpack/jest/babel configs
- SymbolUsage no-call-site skip for wrong-direction retrieval
- FlowTrace callee-query skip for thin delegation functions
- Class preamble continuation for short definitions (e.g. Python `__init__`)
- PascalCase-aware hint-match-boost for class/type definitions
- Concurrent indexing guard (global semaphore prevents OOM)
- HNSW ef_search increased from 32 to 200 for more deterministic vector search

### Project map quality

- Symbols ranked by call graph centrality (reference count) instead of first-encountered
- Directory diversity cap (max 5 symbols per top-level directory)
- Entry points sorted by path depth (shallowest first)
- Expanded generic symbol filter (~80 common names excluded)
- Fixture, script, and devtools paths excluded from all project map sections

### Fixes

- Plugin PostToolUse matcher now includes Read|Glob (was Write|Edit only)
- Fixed stale React P5 benchmark prompt (function renamed in React repo)
- Fixed class preamble stub-body false positive
- Fixed sym-def path-relevance pruning with path-diversity guard

### Benchmark state

- 91% non-regression rate across 131 judged prompts (8 repos)
- React: 16/18 (89%), 11 wins, -10% cost
- Flask: 17/18 (94%), Django: 15/18 (83%), Terraform: 16/18 (89%)

## [3.0.0] — The Context Buster

budi v3.0.0 is the first version designed as a general-purpose context booster for Claude Code —
repo-agnostic, language-aware, and production-ready.

Validated across 8 open-source repos (React, Flask, ripgrep, Terraform, Fastify, FastAPI, Django, Express)
spanning 5 languages (JavaScript, Python, Rust, Go, TypeScript), with 129 judged prompts achieving
89% regression-free results and 3–32% cost savings.

### Highlights

**Repo-agnostic retrieval**
- Validated on 8 repos across 5 languages — no repo-specific logic in the pipeline
- Intent-aware score floors, skip patterns, and budget caps that generalize across codebases
- `.budiignore` support for excluding test/docs directories in large repos

**Language-aware chunking**
- Recursive boundary-node splitting for large AST nodes (functions, classes, modules)
- Selective TypeScript fallback for `.js` files with Flow type annotations
- Language-specific boundary kind exclusions to prevent tiny constant chunks

**Context condensation**
- Same-file card merging: multiple snippets from one file combine into a single evidence card
- Query-aware proof lines: proof lines prioritize tokens matching the query, skip anchor duplicates
- Word-boundary matching for short tokens to prevent false proof line matches
- Call-expression priority pass and low-value line filtering (bare braces, param declarations)
- Per-intent context budgets (architecture 8k, others 3–5.5k)

**Precision improvements**
- Stub-body demotion: `panic("not implemented")`, `unimplemented!()`, `todo!()`, `raise NotImplementedError`, and trivial returns are deprioritized
- Test/devtools/mock path detection and demotion across all intents
- Thin-caller skip: boilerplate entry-point files (≤15 lines, starting at line 1) are filtered
- Low-confidence skip patterns for broad overview queries (module layout, entry points, lifecycle overviews, env listings, design intent)
- Symbol-usage struct/class definition demotion for "what calls X" queries
- Two-tier low-confidence threshold: stricter when no symbol/graph signal supports the match

**Product readiness**
- `budi init --index`: one-command setup (register repo + build index)
- `budi doctor`: structured diagnostics with `[ok]`/`[!!]` checks and actionable fixes
- 256 tests (231 core + 21 CLI + 4 daemon), 0 clippy warnings, 0 TODOs
- Periodic embedding cache saves during index build (crash-safe for large repos)
- XML system prompt filtering to skip non-user prompts (task notifications, function calls)
- Project map noise filtering: test/devtools/config paths excluded from Top Files and Top Symbols

### Breaking changes

None. The index format is unchanged from v2.x. Existing indexes work without rebuilding.

## [2.8.0] — Stress Tests + Stability

- Stress test suite (`scripts/dev/stress_test.py`): 7 concurrent scenarios covering query storms, incremental indexing, session dedup, and file churn
- Fixed latent UTF-8 panic in `truncate_to()` — now walks back to char boundary
- Fixed `is_test_path()` to detect `__tests__/` and `__spec__/` (Jest/Mocha conventions)
- Added 75+ unit tests across `retrieval.rs`, `context.rs`, and `chunking.rs`
- Enabled budi-on-budi telemetry for self-dogfooding

## [2.7.0] — Score Floor Tuning

- Raised FlowTrace score floor: 0.20 → 0.25
- Added SymbolUsage score floor: 0.22
- Reduced SymbolUsage retrieval limit: 6 → 5
- Fixes: P7 (reconcileChildFibers) context reduced from 1968 to 845 chars; P15 reduced from 3154 to 1851
- React benchmark: **15/18 wins (83%)**

## [2.6.0] — Symbol-Definition Accuracy

- `dominant_symbol_hint()`: picks the symbol spanning the most lines in a chunk window — prevents short helpers from stealing the hint from a dominant function
- Hint-match boost (+0.30): fires when intent is SymbolDefinition and chunk's symbol_hint exactly matches a query token
- Extended `looks_like_symbol()` for exported/async JS/TS functions
- Fixed scheduleUpdateOnFiber detection (P3 now wins consistently)

## [2.5.0] — Test Path Boost

- `is_test_path()`: detects `/test`, `/spec`, `__tests__/`, `__spec__/` — test files get +0.15 score boost on TestLookup queries
- TestLookup score floor: 0.22
- Multi-pass judge support in benchmark runner (`--judge-passes 3`)

## [2.4.0] — Classifier Fixes

- SymbolUsage keywords added: "who constructs", "who creates", "who instantiates", "who builds"
- RuntimeConfig tightened: removed bare `"config"` and `"env"` triggers; replaced with `"config file"`, `"load config"`, `"read config"`, `"env var"`
- Fixes false-positive routing of "configured"/"configuration" queries to RuntimeConfig

## [2.3.0] — FlowTrace + SymbolDefinition Classifier Fixes

- FlowTrace keywords added: "cleanup order", "cleanup sequence", "unmount order", "lifecycle order", "removal order", "what order" — fixes P10 misclassification
- SymbolDefinition score floor: 0.20 minimum
- 18-prompt benchmark suite (expanded from 12)
- React benchmark: **16/18 wins**

## [2.2.0] — Richer Session Memory

- `AffinityEntry { ts, anchors }`: stores up to 2 representative code lines per file alongside the timestamp
- Session-start format: `- src/file.js — anchor1; anchor2`
- Auto-migration from old flat `HashMap<String, u64>` format

## [2.1.0] — Context Budget Discipline

- Total injected context hard-capped at `context_char_budget` (12k chars)
- Call graph budget subtracted before snippet context assembly
- FlowTrace call graph gated on top-snippet confidence: score ≥ 0.30 → 1200 chars, else 600

## [2.0.0] — Intent-Aware Context Assembly

- Per-intent call graph budgets (FlowTrace 1200, SymbolDef/Usage 800, Architecture/Test 0)
- Per-intent retrieval limits via `intent_retrieval_limit()`
- Extended proof needles: `call(`, `invoke`, `schedule`, `commit`

## [1.9.0] — Cross-Session File Affinity

- `session-affinity.json`: persists injected file paths across sessions (top 50 by recency)
- Session-start system message includes "## Recently Relevant Files" block (top 5)
- `update_session_affinity()` runs via `spawn_blocking` — zero latency impact on queries

## [1.8.0] — Per-Step Timing

- `QueryResponse.timing_ms`: pipeline step durations (load/embed/retrieval/dedup/callgraph)
- Emitted when `debug_io = true`
- CLI logs timing as `"timing"` key in `hook-io.jsonl`

## [1.7.0] — Query Intent Routing

- `classify_intent(prompt)` → 7 intent kinds
- `weights_for_intent(kind)`: per-intent 5-channel blend adjustments
- `detected_intent` field in `QueryResponse`
- React benchmark: **9/12 wins**

## [1.6.0] — Expanded Benchmark Prompts

- 18-prompt React benchmark suite covering 6 intent categories
- `fixtures/benchmarks/react-structural-v1.prompts.json`

## [1.5.0] — Evidence Cards Format

- Context output format changed to YAML-like evidence cards
- Rules block explains to Claude how to use the injected snippets
- Score and signal annotations per snippet

## [1.4.0] — Call Graph / Structural Oracle

- `chunk_to_graph_tokens` forward index in RuntimeIndex
- `build_call_graph_summary()` generates `[structural context]` block
- Progressive truncation: top snippet ≤ 40% of budget, each next ≤ 60% of remaining
- React benchmark: **5/5 wins**

## [1.3.0] — Project Map

- `generate_project_map()` writes `.claude/budi-project-map.md` after each full index
- `budi hook session-start` serves the project map as system prompt injection

## [1.2.0] — PostToolUse Prefetch

- `/prefetch-neighbors` endpoint: graph neighbors for a file, triggered by Read/Glob hooks
- `PostToolUse` hook installed by `budi init`

## [1.1.0] — Session Deduplication

- `session_id` in `QueryRequest` enables cross-prompt dedup within a session
- Session state TTL: 30 minutes
- Deduplication by `path:start_line` to avoid re-injecting seen snippets

## [1.0.0] — Initial Release

- 5-channel hybrid retrieval: lexical (BM25/Tantivy), vector (HNSW/fastembed), symbol, path, graph
- `budi init` + `budi index` setup flow
- `UserPromptSubmit` hook for context injection
- `debug_io` telemetry flag
