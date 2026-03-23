# budi Architecture

## Overview

`budi` is a local-first analytics dashboard for AI coding agents — "WakaTime for AI coding agents." It tracks where your tokens go by parsing data from multiple providers (Claude Code, Cursor, etc.) and presenting unified usage data through a CLI and web dashboard.

### Data flow

1. **Claude Code** writes session transcripts to `~/.claude/projects/*/conversations/*.jsonl`.
2. **Cursor** stores session data in `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb` (SQLite).
3. `budi sync` (or daemon `POST /sync`) incrementally parses new data from all detected providers.
4. Parsed data is stored in a local SQLite database (`~/.local/share/budi/analytics.db`).
5. `budi stats`, `budi insights`, and the web dashboard (`GET /dashboard`) query the SQLite DB.

## Components

- `budi-cli`: CLI commands — init, doctor, stats, cost, models, sessions, insights, sync, statusline, version.
- `budi-daemon`: local HTTP daemon serving hooks, stats, analytics, and the web dashboard.
- `budi-core`: shared logic:
  - `provider.rs` — Provider trait, ModelPricing, registry (all_providers/available_providers)
  - `providers/claude_code.rs` — ClaudeCodeProvider (hooks, JSONL parsing, pricing)
  - `providers/cursor.rs` — CursorProvider (state.vscdb parsing, JSONL fallback, pricing)
  - `config.rs` — daemon config (host, port)
  - `daemon.rs` — session tracking + query stats
  - `hooks.rs` — hook input/output types for Claude Code integration
  - `pre_filter.rs` — non-code and conversational prompt detection
  - `jsonl.rs` — JSONL transcript parser (tokens, tools, sessions, context usage)
  - `analytics.rs` — SQLite storage (schema v4), incremental sync, usage queries
  - `cost.rs` — token cost estimation dispatched through provider trait
  - `insights.rs` — actionable usage insights (search efficiency, MCP tools, cache, config health)
  - `claude_data.rs` — reads Claude Code local files (stats-cache, plugins, sessions, plans, memory)
  - `repo_id.rs` — canonical repository identity resolution
  - `rpc.rs` — StatusRequest/StatusResponse types

## Provider Architecture

Every AI coding agent implements the `Provider` trait:

```rust
trait Provider: Send + Sync {
    fn name(&self) -> &'static str;
    fn display_name(&self) -> &'static str;
    fn is_available(&self) -> bool;
    fn discover_files(&self) -> Result<Vec<DiscoveredFile>>;
    fn parse_file(&self, path, content, offset) -> Result<(Vec<ParsedMessage>, usize)>;
    fn pricing_for_model(&self, model: &str) -> ModelPricing;
    fn sync_direct(&self, conn: &mut Connection) -> Option<Result<(usize, usize)>>;
    // ... optional: setup_data, discover_plans, prompt_history, hook_support
}
```

Providers are auto-detected at runtime. `sync_direct()` allows providers to sync from structured data sources (e.g. Cursor's SQLite) instead of file-by-file JSONL parsing.

## Analytics Storage (Schema v4)

`~/.local/share/budi/analytics.db` with four tables:

| Table | Key columns |
|-------|------------|
| **sessions** | session_id, project_dir, first/last_seen, version, git_branch, repo_id, provider, session_title, interaction_mode, lines_added, lines_removed |
| **messages** | uuid, session_id, role, timestamp, model, input/output/cache tokens, provider, cost_cents, context_tokens_used, context_token_limit, interaction_mode |
| **tool_usage** | message_uuid, tool_name |
| **sync_state** | file_path, byte_offset, last_synced |

Incremental ingestion tracks byte offsets per JSONL file and watermarks per structured source.

## Daemon Endpoints

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/health` | GET | Health check |
| `/status` | POST | Repo status |
| `/stats` | GET | Query stats |
| `/session-stats` | POST | Per-session stats |
| `/sync` | POST | Trigger sync across all providers |
| `/analytics/summary` | GET | Usage summary (tokens, tools, sessions) |
| `/analytics/sessions` | GET | Session list with filters |
| `/analytics/session/{id}` | GET | Session detail by ID |
| `/analytics/cwd` | GET | Working directory usage |
| `/analytics/cost` | GET | Cost breakdown by model |
| `/analytics/models` | GET | Model usage breakdown |
| `/analytics/providers` | GET | Per-provider aggregate stats |
| `/analytics/statusline` | GET | Compact stats for status line |
| `/analytics/context-usage` | GET | Context window utilization stats |
| `/analytics/interaction-modes` | GET | Interaction mode breakdown |
| `/analytics/insights` | GET | Actionable insights |
| `/analytics/config-files` | GET | Config file inventory |
| `/analytics/activity-chart` | GET | Time-bucketed activity data |
| `/analytics/plans` | GET | Plan files |
| `/analytics/memory` | GET | Memory files |
| `/analytics/history` | GET | Prompt history |
| `/analytics/plugins` | GET | Installed plugins |
| `/analytics/permissions` | GET | Permission settings |
| `/dashboard` | GET | Web dashboard (embedded HTML) |
| `/hook/prompt-submit` | POST | Claude Code hook handler |
| `/hook/tool-use` | POST | Claude Code hook handler |

## Insights Engine

Five insight types generated from analytics data:

1. **Search Efficiency** — Grep/Glob ratio vs total tool calls
2. **MCP Tool Usage** — per-server breakdown of mcp__* tools
3. **CLAUDE.md Analysis** — scans project dirs, flags oversized files
4. **Cache Efficiency** — hit rate with threshold-based recommendations
5. **Token-Heavy Sessions** — flags sessions with input > 5x output

## Web Dashboard

Self-contained HTML/CSS/JS embedded in the daemon binary via `include_str!`. Zero external dependencies.

Five pages: Stats, Insights, Setup, Plans, Prompts.

Stats page features: summary cards, context window utilization, lines changed, interaction modes, per-agent breakdown, activity chart, model/project/tool charts, session table with provider badges and titles.
