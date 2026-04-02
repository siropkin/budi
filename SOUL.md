# SOUL.md

Local-first cost analytics for AI coding agents (Claude Code, Cursor). Tracks tokens, costs, and usage per message. No cloud - everything on-machine.

## Build & Test

```bash
cargo build              # dev build
cargo build --release    # release build
cargo test               # all workspace tests
cargo test -p budi-core  # core tests only
./scripts/install.sh     # build release + install to ~/.local/bin/
```

**Important**: Install **`budi` and `budi-daemon` from the same build** and keep **only one copy on PATH** (do not mix Homebrew with `~/.local/bin` or another prefix). Version mismatch breaks daemon restarts; `budi init` warns if multiple binaries are found.

After upgrading: restart the daemon (stop the old process, then `budi init` or `budi sync`). On Unix you can use `pkill -f budi-daemon`; on Windows use `taskkill /IM budi-daemon.exe /F` if needed.

## Platforms

macOS and Linux use the Unix daemon startup path (`lsof`, `ps`, `kill`) to replace an existing listener on the same port. Windows uses PowerShell **`Get-NetTCPConnection`** and **`taskkill`** for the same behavior. Unsupported or minimal environments may skip automatic takeover - stop the old daemon manually if the new one cannot bind.

## Architecture

### Crates

- **budi-core** - Business logic: analytics (SQLite queries), providers (Claude Code, Cursor), pipeline (enrichment), cost calculation, OTEL ingestion, hooks, config, migrations
- **budi-cli** - Thin HTTP client to the daemon. Commands: init, stats, sync, statusline, hook, doctor, open, update, uninstall, migrate, repair, health, mcp-serve
- **budi-daemon** - axum HTTP server (port 7878). Owns SQLite exclusively. Serves dashboard, analytics API, hook ingestion, OTEL ingestion

### Data flow

```
Sources (JSONL files, OTEL spans, Cursor API, Hooks)
  -> Providers discover + parse -> ParsedMessage structs
  -> Pipeline: HookEnricher -> IdentityEnricher -> GitEnricher -> CostEnricher -> TagEnricher
  -> SQLite (messages + tags tables)
  -> Dashboard / CLI stats / Statusline
```

Enricher order is critical - each depends on prior enrichers. Do not reorder.

### Database (SQLite, WAL mode, schema v15)

Six tables, four data entities + two supporting:
- **messages** - Single cost entity. One row per API call. All token/cost data lives here. Fields: uuid, session_id, role, model, provider, timestamp, input/output/cache tokens, cost_cents, cost_confidence, git_branch, repo_id, cwd, request_id
- **sessions** - Lifecycle context (start/end, duration, mode, title) without mixing cost concerns. One row per conversation from hooks
- **hook_events** - Raw event log for tool stats and MCP tracking. One row per hook event
- **otel_events** - Raw OpenTelemetry event storage for debugging/audit
- **tags** - Flexible key-value pairs per message (repo, ticket_id, activity, user, etc.) with FK to messages
- **sync_state** - Tracks incremental ingestion progress per file for progressive sync

### Cost sources

| Source | Confidence | What it provides |
|--------|-----------|-----------------|
| **OTEL** (Claude Code) | `otel_exact` | Per-request tokens including thinking, exact cost |
| **JSONL** (Claude Code) | `estimated` | Per-message tokens (no thinking), cost calculated from pricing |
| **Cursor Usage API** | `exact` | Per-request tokens + totalCents from Cursor's API |

OTEL and JSONL deduplicate: same API call matched by session_id + model + timestamp +/-1s. OTEL upgrades JSONL rows in-place.

### Key concepts

- **cost_confidence**: determines `~` prefix in dashboard for non-exact costs
- **Session context propagation**: git_branch/repo_id flow from user -> assistant messages within a session
- **Progressive sync**: files processed newest-first so dashboard shows recent data quickly
- **Sync split**: `budi sync` = 30-day window (fast), `budi sync --all` = full history
- **Hook system**: `budi hook` reads stdin JSON, POSTs to daemon. Fire-and-forget, <50ms

## Key files

- `crates/budi-core/src/analytics/mod.rs` - SQLite storage, sync pipeline, all query functions
- `crates/budi-core/src/analytics/health.rs` - Session health vitals, ProviderKind-aware tips, overall-state logic
- `crates/budi-core/src/analytics/tests.rs` - Analytics + session health unit tests
- `crates/budi-core/src/pipeline/mod.rs` - Pipeline struct, Enricher trait, default_pipeline()
- `crates/budi-core/src/pipeline/enrichers.rs` - All 5 enricher implementations
- `crates/budi-core/src/cost.rs` - Cost estimation, ModelPricing, per-provider pricing tables
- `crates/budi-core/src/hooks.rs` - HookEvent parsing, session upsert, prompt classification
- `crates/budi-core/src/otel.rs` - OTLP JSON parsing, OTEL->JSONL dedup
- `crates/budi-core/src/jsonl.rs` - JSONL transcript parser, ParsedMessage struct
- `crates/budi-core/src/providers/claude_code.rs` - Claude Code provider (JSONL discovery, pricing)
- `crates/budi-core/src/providers/cursor.rs` - Cursor provider (Usage API, auth from state.vscdb)
- `crates/budi-core/src/migration.rs` - Schema v15, all migration paths
- `crates/budi-core/src/config.rs` - BudiConfig, StatuslineConfig, TagsConfig
- `crates/budi-cli/build.rs` - Build script: creates empty vsix placeholder if not pre-built
- `crates/budi-daemon/src/main.rs` - HTTP server, ~38 routes
- `crates/budi-daemon/src/routes/hooks.rs` - /hooks/ingest, /sync, /sync/all, /sync/reset, /sync/status, /health, /health/integrations, /health/check-update endpoints
- `crates/budi-daemon/src/routes/analytics.rs` - All analytics + admin endpoints (summary, messages, projects, cost, models, activity, branches, tags, providers, statusline, tools, mcp, cache-efficiency, session-cost-curve, cost-confidence, subagent-cost, sessions, session-health, session-audit, admin/providers, admin/schema, admin/migrate, admin/repair)
- `crates/budi-daemon/src/routes/otel.rs` - /v1/logs OTLP ingestion
- `crates/budi-cli/src/commands/statusline.rs` - Statusline rendering (coach mode with health tips) + installation
- `crates/budi-cli/src/mcp.rs` - MCP server handler (15 tools: analytics + config + health)
- `crates/budi-cli/src/commands/mcp.rs` - `mcp-serve` subcommand (stdio transport)
- `crates/budi-daemon/static/js/` - Dashboard JS (vanilla, no framework)
- `extensions/cursor-budi/src/extension.ts` - Cursor extension entry point (status bar, commands, polling)
- `extensions/cursor-budi/src/panel.ts` - Side panel webview (session details, vitals, session list)
- `extensions/cursor-budi/src/budiClient.ts` - Daemon HTTP client + health aggregation logic

## Dev notes

- CLI never touches SQLite directly - all queries go through the daemon HTTP API
- CostEnricher is the single source of truth for cost - sets cost_cents during pipeline. Skips if cost already set (API data)
- `budi init` installs hooks in `~/.claude/settings.json` (CC) and `~/.cursor/hooks.json` (Cursor), plus OTEL env vars and MCP server
- **MCP server**: `budi mcp-serve` runs an MCP server over stdio. Installed into `~/.claude/settings.json` mcpServers by `budi init`. 15 tools for analytics (cost summary, models, projects, branches, tags, providers, tools, activity), config (get_config, set_tag_rules, set_statusline_config, sync_data, get_status), and health (session_health). Thin HTTP client to daemon - stdout is JSON-RPC only, logging to stderr
- Tags are auto-detected (provider, model, repo, ticket_id, etc.) + custom rules via `~/.config/budi/tags.toml`
- git_branch is a column on messages (not a tag) for fast queries
- **Session health**: Four vitals computed per session - context growth (context-size growth), cache reuse (cache hit rate), cost acceleration (per-turn or per-reply cost growth), retry loops (tool failure loops from hook_events). Each vital has green/yellow/red state. New sessions start green - the default is always positive; vitals only degrade to yellow/red when there is clear evidence of a problem. Tips are provider-aware via `ProviderKind` enum (Claude Code -> `/compact`/`/clear`, Cursor -> "new composer session", Other -> neutral). Statusline "coach" mode shows health icon + session cost + tip. Dashboard session detail page has a health panel with vitals grid and tips section.
- **Cursor extension** (`extensions/cursor-budi/`): VS Code extension that shows session health in the status bar (aggregated health circles) and a side panel (session details, vitals, tips, session list). Auto-installed by `budi init` when Cursor CLI is on PATH (`.vsix` embedded in binary via `include_bytes!`). Communicates with daemon via HTTP. Tracks active session via `~/.local/share/budi/cursor-sessions.json` (written by hooks, watched by extension). `budi doctor` and `/health/integrations` both check extension install status.
- **Dashboard** is multi-page at `/dashboard` with URL-based routing (vanilla JS, no framework):
  - `/dashboard` (Overview) - Summary cards (cost/tokens/messages), activity timeline, agents/models, projects/branches, tickets/activity types
  - `/dashboard/insights` - Cost confidence, cache efficiency, session cost curve (split: cost + count), speed mode, subagent vs main, tools, MCP servers
  - `/dashboard/sessions` - Session list with sort/search/pagination, drill-down to `/dashboard/sessions/:id` with session meta, tags, health panel (vitals + tips), input token growth chart, message table
  - `/dashboard/settings` - Status, integrations, database info, paths, actions (sync/re-sync/migrate/check updates), help links
- Dashboard JS files: `state.js`, `utils.js`, `api.js`, `stats.js` (shared components), `views.js` (overview), `views-insights.js`, `views-sessions.js`, `views-settings.js`, `events.js` (routing/lifecycle)
- Analytics endpoints: `/analytics/summary`, `/analytics/messages`, `/analytics/projects`, `/analytics/cost`, `/analytics/models`, `/analytics/activity`, `/analytics/branches`, `/analytics/branches/{branch}`, `/analytics/tags`, `/analytics/providers`, `/analytics/statusline`, `/analytics/tools`, `/analytics/mcp`, `/analytics/cache-efficiency`, `/analytics/session-cost-curve`, `/analytics/cost-confidence`, `/analytics/subagent-cost`, `/analytics/sessions`, `/analytics/sessions/{id}/messages`, `/analytics/sessions/{id}/tags`, `/analytics/session-health`, `/analytics/session-audit` (session attribution stats for debugging ingestion - not used by dashboard/MCP)
- Admin endpoints: `/admin/providers` (registered providers), `/admin/schema` (schema version), `/admin/migrate` (run migration), `/admin/repair` (repair schema drift + run migration)
- Sync endpoints: `/sync` (30-day), `/sync/all` (full history), `/sync/reset` (wipe sync state + full re-sync), `/sync/status` (syncing flag + last_synced)
- Health endpoints: `/health` (ok + version), `/health/integrations` (hooks/MCP/OTEL/statusline status + DB stats + paths), `/health/check-update` (GitHub releases)
