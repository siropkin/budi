# budi Architecture

## Overview

`budi` is a local-first analytics dashboard for Claude Code — "WakaTime for Claude Code." It tracks where your tokens go by parsing Claude Code's JSONL transcripts and presenting usage data through a CLI and web dashboard.

### Data flow

1. Claude Code writes session transcripts to `~/.claude/projects/*/conversations/*.jsonl`.
2. `budi sync` (or daemon `POST /sync`) incrementally parses new JSONL entries.
3. Parsed data is stored in a local SQLite database (`~/.local/share/budi/analytics.sqlite`).
4. `budi stats`, `budi insights`, and the web dashboard (`GET /dashboard`) query the SQLite DB.

## Components

- `budi-cli`: CLI commands — init, doctor, stats, insights, sync, hooks, statusline.
- `budi-daemon`: local HTTP daemon serving hooks, stats, analytics, and the web dashboard.
- `budi-core`: shared logic:
  - `config.rs` — daemon config (host, port, debug_io)
  - `daemon.rs` — session tracking + query stats
  - `hooks.rs` — hook input/output types for Claude Code integration
  - `pre_filter.rs` — non-code and conversational prompt detection
  - `jsonl.rs` — Claude Code JSONL transcript parser (tokens, tools, sessions)
  - `analytics.rs` — SQLite storage, incremental sync, usage queries
  - `insights.rs` — actionable usage insights (search efficiency, MCP tools, cache, CLAUDE.md)
  - `rpc.rs` — StatusRequest/StatusResponse types

## Analytics Storage

- `~/.local/share/budi/analytics.sqlite`: sessions, messages, tool_usage, sync_state tables
- Incremental ingestion tracks byte offsets per JSONL file — only new entries are parsed on each sync
- Date-filterable queries: today, week, month, all

## Daemon Endpoints

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/health` | GET | Health check |
| `/status` | POST | Repo status |
| `/stats` | GET | Query stats |
| `/session-stats` | POST | Per-session stats |
| `/sync` | POST | Trigger JSONL → SQLite sync |
| `/analytics/summary` | GET | Usage summary (tokens, tools, sessions) |
| `/analytics/sessions` | GET | Session list with filters |
| `/analytics/session/{id}` | GET | Session detail by ID |
| `/analytics/cwd` | GET | Working directory usage |
| `/analytics/insights` | GET | Actionable insights |
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

Features: summary cards, tool usage charts, project charts, session table with drill-down, period tabs (today/week/month/all), one-click sync.
