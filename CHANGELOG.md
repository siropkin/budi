# Changelog

All notable changes to budi are documented here.

## [7.0.0] — Cost Accuracy & Cursor Fixes

Audited cost calculations against official APIs. Fixed two Cursor bugs, added pricing validation tests.

### Fixes

- **Cursor: dropped expensive events** — events over $10 (1000 totalCents) were silently skipped as "corrupt". Real Opus requests with large cache reads can legitimately cost $10+. Raised threshold to $1000.
- **Cursor: duplicate messages on re-sync** — sequential counter in UUID generation shifted when previously-skipped events were included, creating duplicates. Replaced with deterministic UUID using all 4 token fields.

### Validation

- **Claude Code**: 0.00% cost delta across 125K+ messages / 16 days vs raw JSONL
- **Cursor**: 0.00% cost delta across all days vs official Cursor Usage API
- **Pricing table**: verified against official Anthropic pricing page (2026-03-25)
- Added 8 pricing validation tests covering all model variants, cache multipliers, and real-world token counts
- 126 tests total (111 core + 13 cli + 2 daemon)

## [6.0.0] — Cursor Usage API & Cost Confidence

Replaced vscdb composerData parsing with Cursor's undocumented Usage API for exact per-request tokens and cost.

### New features

- **Cursor Usage API integration** — exact `inputTokens`, `outputTokens`, `cacheReadTokens`, `totalCents` per request, authenticated via JWT from state.vscdb
- **cost_confidence tracking** — "exact" (Cursor API), "estimated" (token-based calculation). Dashboard shows `~` prefix for estimated costs.
- **Session context propagation** — pipeline propagates git_branch, repo_id, cwd from user messages to subsequent assistant messages in the same session
- **Progressive sync** — files discovered newest-first (mtime descending) so dashboard shows recent data within seconds

### Improvements

- Sync split: `budi sync` = 7-day window (fast), `budi sync --all` = full history
- Projects chart sorted by cost descending
- Activity chart fills full period including future days with empty bars
- Dashboard shows cost_confidence indicators

### Removed

- composerData/vscdb parsing (replaced by Usage API)
- estimate_tokens_from_cost heuristics

## [5.0.0] — Hooks & Three-Entity Model

Real-time event capture from Claude Code and Cursor hooks. Schema v7.

### New features

- **Hook system** — `budi hook` CLI reads stdin JSON, POSTs to daemon. Fire-and-forget, silent failure, <50ms.
- **Hook installation** — `budi init` installs hooks in `~/.claude/settings.json` and `~/.cursor/hooks.json`
- **Three-entity model** — sessions + messages + hook_events. Sessions track lifecycle, messages track cost, hook_events track raw activity.
- **Prompt classification** — keyword heuristics classify prompts as bugfix/feature/refactor/question/ops
- **MCP server extraction** — `mcp__<server>__<tool>` → mcp_server column, aggregated in dashboard
- **Tools & MCP dashboard charts** — new visualization panels

### Hook events captured

- Claude Code: SessionStart, SessionEnd, PostToolUse, SubagentStop, PreCompact, Stop, UserPromptSubmit
- Cursor: sessionStart, sessionEnd, postToolUse, subagentStop, preCompact, stop, afterFileEdit

### Hook-derived tags

composer_mode, permission_mode, activity, user_email, duration (short/medium/long), dominant_tool

## [4.0.0] — Daemon Architecture & Lightweight Core

CLI as thin HTTP client, daemon as single source of truth. Major cleanup.

### Architecture

- **Daemon owns SQLite exclusively** — CLI never opens the database directly
- **All queries via HTTP** — CLI → daemon → SQLite → response
- **Removed 5 heavyweight features** — AI contribution tracking, scored commits, debug_io telemetry, pre_filter heuristics, Starship auto-detection (-1,200 lines)

### New features

- **git_branch column** — denormalized on messages table for fast branch cost queries
- **Tag cost splitting** — when multiple tags share a session, cost split evenly via pre-computed JOINs
- **Untagged cost** — all charts show "(untagged)" entry for unattributed cost

## [3.0.0] — Cost Analytics Pivot

Complete rewrite from context retrieval to cost analytics.

### What changed

budi pivoted from a code context injection tool to a local-first AI cost analytics platform. All retrieval, indexing, embedding, and benchmark infrastructure was removed. The new focus: show developers where their AI tokens and money go.

### New features

- **Multi-provider architecture** — pluggable Provider trait for any AI coding agent
- **Claude Code provider** — JSONL transcript parsing, per-model pricing
- **Cursor provider** — JSONL transcripts + Usage API
- **Pipeline system** — Extract → Normalize → Propagate → Enrich → Load
- **Enrichers** — Git, Identity, Cost, Tag, Hook
- **Web dashboard** — cost cards, activity chart, model/project/branch/ticket breakdowns, messages table
- **Status line** — day/week/month costs in Claude Code, clickable dashboard link
- **Tags system** — auto-detected + custom rules via `~/.config/budi/tags.toml`
- **Per-repo tracking** — canonical repo ID from git remote, merges worktrees and clones

### Technical

- SQLite with WAL, 40MB cache, 256MB mmap
- ~6 MB binary, minimal CPU/memory footprint
- 100% local, no cloud, no uploads
