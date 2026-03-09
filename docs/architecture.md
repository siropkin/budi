# budi Architecture

## Overview

`budi` uses Claude Code hooks to inject deterministic repository context at prompt submit time.

### Data flow

1. Claude Code fires `UserPromptSubmit`.
2. Hook command `budi hook user-prompt-submit` parses stdin payload.
3. CLI calls local daemon `/query`.
4. Daemon performs hybrid retrieval on local index:
   - lexical search via Tantivy (BM25)
   - vector ANN via HNSW (`hnsw_rs`)
   - symbol/path and resolver-backed call-graph channels
   - intent-policy re-ranking with pruned core rules (dirty files, scope/path, docs/symbol intent)
5. Daemon returns context pack.
6. Hook emits JSON with `hookSpecificOutput.additionalContext`.

## Components

- `budi-cli`: init, index, status, doctor, preview, and hook entrypoints.
- `budi-daemon`: local HTTP daemon serving query/index/status/update.
  - `/index` schedules async full-index jobs; clients poll `/progress` for job state/outcome
- `budi-core`: shared logic:
  - file discovery from `git ls-files` (tracked + untracked), respecting `.gitignore`, repo-root `*.ignore` files (for example `.cursorignore`/`.codeiumignore`/`.contextignore`), global `~/.local/share/budi/global.budiignore`, and repo-local `.budiignore` rules (`!unignore` supported), with code-first type filtering via extension and basename allowlists and one-shot CLI overrides (`--ignore-pattern`, `--include-ext`)
  - chunking
  - embedding engine (fastembed with lexical-only fallback when unavailable)
  - persistent state
  - hybrid retrieval and context packing

## Index state

- `~/.local/share/budi/repos/<repo-id>/index/index.sqlite`: transactional catalog for files + chunks + embeddings
  - also stores persisted index progress + job snapshot (`queued`/`running`/`succeeded`/`failed`/`interrupted`) for daemon restarts
- `~/.local/share/budi/repos/<repo-id>/index/tantivy/`: lexical index files
- `~/.local/share/budi/fastembed-cache/`: embedding model cache (kept outside repos)
- `~/.local/share/budi/embedding-cache.sqlite`: global content-hash embedding reuse cache
- in-memory daemon cache:
  - HNSW vector graph
  - chunk id map
  - Tantivy reader

## Incremental updates

- Async `PostToolUse` hook sends changed file path hints to daemon `/update`.
- Daemon also starts a repo file watcher after first repo request, pre-filters events with compiled index scope policy (extensions + layered ignore rules), and batches accepted paths with debounce.
- A periodic reconcile signal triggers metadata-based re-scan to recover from missed watcher/hook events.
- Re-index computes changed hashes and re-embeds only changed files unless a reconcile pass is requested.
- HNSW graph is rebuilt in-memory from current chunk set.
- `/status` and `budi doctor --deep` expose watcher health counters (`watch_events_seen`, `watch_events_accepted`, `watch_events_dropped`).
- Incremental updates short-circuit no-op batches (unsupported/ignored/unchanged hints) and surface `updates_noop` / `updates_applied` counters.

## Project Map (Phase D)

- After each full index job, `generate_project_map()` writes local `.claude/budi-project-map.md` into the repo with a high-level file-tree summary grouped by directory.
- The project map is generated state, so `.claude/` is treated as local-only rather than tracked documentation.
- `budi hook session-start` reads this file and outputs it as an `AsyncSystemMessageOutput`, injecting it into the Claude Code system prompt at session start.

## Call Graph / Structural Oracle (Phase E)

- `RuntimeIndex` maintains `chunk_to_graph_tokens` (forward index: chunk → callee symbol tokens).
- `callers_of(symbol)` and `callees_of(chunk_id)` navigate the graph bidirectionally.
- `build_call_graph_summary(runtime, snippets)` generates a `[structural context]` block appended to context when intent warrants it.
- Call graph budget is gated by intent and top-snippet confidence (see Context Budget below).

## PostToolUse Prefetch (Phase B)

- When Claude reads or globs a file, the `PostToolUse` hook fires immediately.
- The hook POSTs `/prefetch-neighbors` with the file path, and the daemon returns graph neighbors of that file.
- Neighbors are delivered as `AsyncSystemMessageOutput` — low latency, zero interference with the main prompt flow.

## Query Intent Routing (Phase H/K)

`classify_intent(prompt)` maps each query to one of 7 intent kinds:

| Intent | Trigger keywords (examples) |
|--------|----------------------------|
| `SymbolUsage` | "what calls", "who calls", "where is X used" |
| `SymbolDefinition` | "where is X defined", "show me the implementation of" |
| `FlowTrace` | "how does X work", "trace the flow", "what order" |
| `Architecture` | "explain the architecture", "how is the system structured" |
| `TestLookup` | "unit test for", "test coverage of" |
| `RuntimeConfig` | "config file", "env var", "load config" |
| `NonCode` | everything else (injection skipped) |

`weights_for_intent(kind)` adjusts the 5-channel blend (lexical/vector/symbol/path/graph) per intent. `intent_retrieval_limit(kind)` sets per-intent snippet caps (5–8).

## Evidence-First Output

- `build_context()` emits structured `[budi context]` payloads with `rules` plus `evidence_cards`, rather than dumping raw scored snippets into the prompt.
- Each evidence card contains:
  - exact `file`
  - `span`
  - one `anchor` line
  - a short `proof` list
  - optional SLM relevance note
- Score/debug metadata is stripped before injection so the global model sees only grounded evidence, not ranking internals.
- Runtime-config queries have an additional guardrail path:
  - if full context is skipped, `build_runtime_guard_context()` can still emit a narrow `[budi runtime guard]` block
  - this fallback includes only verified production file paths with runtime-config signals
  - tests/examples/fixtures are filtered out unless the user explicitly asks for them

## Context Budget Discipline (Phase L)

- Total injected context is hard-capped at `context_char_budget` (default 12,000 chars).
- Call graph budget is subtracted from the base budget before evidence-card context is assembled.
- Per-intent call graph budgets: FlowTrace → 1200 chars (gated on top-snippet score ≥ 0.30, else 600); SymbolDefinition/SymbolUsage → 800; Architecture/TestLookup/RuntimeConfig → 0 (suppressed).
- Per-intent snippet budgets further narrow the payload for precision intents: SymbolDefinition → 3000 chars, SymbolUsage/RuntimeConfig → 4000, FlowTrace → 5500, TestLookup → 5000, Architecture/default → full configured budget.
- Progressive truncation in `build_context()`: top snippet ≤ 40% of budget, each next ≤ 60% of remaining.

## Broad-Query Skip Logic

Certain broad/overview queries are better served by Claude exploring on its own than by injecting a few code snippets that anchor it to a narrow subset:

- **Design/test-gen Architecture queries** (top < 0.55): "would you add", "I want to implement", etc.
- **Module-layout Architecture queries** (top < 0.55): "module layout", "directory structure", "codebase structure"
- **Env-var listing RuntimeConfig queries**: "which env vars", "what env vars"
- **Lifecycle-overview FlowTrace queries** (top < 0.55): "lifecycle hook execution order", "cleanup order for effects"

When these patterns fire, injection is skipped entirely. Additionally, when a synthetic condenser pack is the top card for FlowTrace, remaining HNSW cards are filtered to pack_score × 0.95 to reduce context noise.

## Score Floors and Boosts (Phases N/P/R/S/T)

- `min_selection_score(candidates, intent)` returns a per-intent floor:
  - FlowTrace: max(top×0.40, 0.25), SymbolDefinition: max(top×0.40, 0.30), SymbolUsage: max(top×0.40, 0.22), TestLookup: max(top×0.40, 0.22), RuntimeConfig: 0.40 when top≥0.60 else max(top×0.40, 0.18), Architecture: 0.40 when top≥0.60 else max(top×0.40, 0.30)
- `is_test_path(path)` — detects `/test`, `/spec`, `__tests__/`, `__spec__/` — used for `+0.15` test-path boost on TestLookup queries.
- **Hint-match boost (S1)**: `+0.30` when intent is SymbolDefinition and the chunk's `symbol_hint` exactly matches a query token, surfacing the definition chunk over reference noise.
- `dominant_symbol_hint(lines)` — picks the symbol spanning the most lines in a window, preventing short local functions from stealing the hint from the dominant definition.
- `truncate_to(s, max)` — UTF-8 safe: walks back to the nearest char boundary rather than slicing at a byte offset.

## Repo Plugins and Ecosystem Detection

`budi-core` includes a built-in repo-plugin registry (`crates/budi-core/src/repo_plugins/`) that keeps framework-specific heuristics out of the generic retrieval pipeline.

Each plugin declares:
- **Chunk matcher**: path/text/language patterns for per-chunk ecosystem tagging
- **Query matcher**: keywords that indicate the query targets a specific framework
- **Repo shape hint** (optional): manifest file patterns (package.json, pyproject.toml) and structural path patterns to detect the framework from project structure
- **Context pack** (optional): synthetic evidence card builder for framework-specific condensers

Built-in plugins: React, Next.js, Flask, Django, FastAPI, Express.

At `RuntimeIndex` construction, `detect_repo_ecosystems()` scans manifest files on disk and indexed file paths to identify the repo's primary frameworks. These repo-level ecosystems merge with query-detected ecosystems during retrieval, so the `+0.08` ecosystem-match boost fires even when queries don't mention the framework by name.

## Cross-Session File Affinity (Phase J/M)

- After each successful injection, `update_session_affinity(repo_root, snippets)` persists injected file paths + anchor lines to `session-affinity.json` (top 50 by recency, stored outside the repo).
- Format: `HashMap<path, AffinityEntry { ts: u64, anchors: Vec<String> }>`.
- `budi hook session-start` appends a `## Recently Relevant Files` block (top 5 paths with anchors) to the session-start system message.

## Per-Step Timing (Phase I)

- When `config.debug_io = true`, `QueryResponse.timing_ms` is populated with millisecond durations for each pipeline step: `load`, `embed`, `retrieval`, `dedup`, `callgraph`.
- The CLI logs timing as a `"timing"` key in `hook-io.jsonl`.
