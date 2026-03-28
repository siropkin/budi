# CLAUDE.md

Local-first cost analytics for AI coding agents (Claude Code, Cursor). Tracks tokens, costs, and usage per message. No cloud — everything on-machine.

## Build & Test

```bash
cargo build                                    # dev build
cargo build --release                          # release build
cargo test                                     # all tests (172: 156 core + 14 cli + 2 daemon)
cargo test -p budi-core                        # core tests only
./scripts/install.sh                           # build release + install to ~/.local/bin/
python3 scripts/validate-cost.py               # validate cost accuracy vs raw data
```

**Important**: Always install both `budi` and `budi-daemon` together. Version mismatch causes daemon restart failure.

After installing: `pkill -f "budi-daemon serve" && budi sync`

## Architecture

### Crates

- **budi-core** — Business logic: analytics (SQLite queries), providers (Claude Code, Cursor), pipeline (enrichment), cost calculation, OTEL ingestion, hooks, config, migrations
- **budi-cli** — Thin HTTP client to the daemon. Commands: init, stats, sync, statusline, hook, doctor, open, update, uninstall, migrate, mcp-serve
- **budi-daemon** — axum HTTP server (port 7878). Owns SQLite exclusively. Serves dashboard, analytics API, hook ingestion, OTEL ingestion

### Data flow

```
Sources (JSONL files, OTEL spans, Cursor API, Hooks)
  → Providers discover + parse → ParsedMessage structs
  → Pipeline: HookEnricher → IdentityEnricher → GitEnricher → CostEnricher → TagEnricher
  → SQLite (messages + tags tables)
  → Dashboard / CLI stats / Statusline
```

Enricher order is critical — each depends on prior enrichers. Do not reorder.

### Database (SQLite, WAL mode, schema v13)

Four entities:
- **messages** — One row per API call. Primary cost entity. Fields: uuid, session_id, role, model, provider, timestamp, input/output/cache tokens, cost_cents, cost_confidence, git_branch, repo_id, cwd, request_id
- **tags** — Key-value pairs per message (repo, ticket_id, activity, user, etc.)
- **sessions** — One row per conversation. Lifecycle metadata from hooks (started_at, ended_at, composer_mode, permission_mode)
- **otel_events** — Raw OpenTelemetry event storage

### Cost sources

| Source | Confidence | What it provides |
|--------|-----------|-----------------|
| **OTEL** (Claude Code) | `otel_exact` | Per-request tokens including thinking, exact cost |
| **JSONL** (Claude Code) | `estimated` | Per-message tokens (no thinking), cost calculated from pricing |
| **Cursor Usage API** | `exact` | Per-request tokens + totalCents from Cursor's API |

OTEL and JSONL deduplicate: same API call matched by session_id + model + timestamp ±1s. OTEL upgrades JSONL rows in-place.

### Key concepts

- **cost_confidence**: determines `~` prefix in dashboard for non-exact costs
- **Session context propagation**: git_branch/repo_id flow from user → assistant messages within a session
- **Progressive sync**: files processed newest-first so dashboard shows recent data quickly
- **Sync split**: `budi sync` = 7-day window (fast), `budi sync --all` = full history
- **Hook system**: `budi hook` reads stdin JSON, POSTs to daemon. Fire-and-forget, <50ms

## Key files

- `crates/budi-core/src/analytics.rs` — SQLite storage, sync pipeline, all query functions
- `crates/budi-core/src/pipeline/mod.rs` — Pipeline struct, Enricher trait, default_pipeline()
- `crates/budi-core/src/pipeline/enrichers.rs` — All 5 enricher implementations
- `crates/budi-core/src/cost.rs` — Cost estimation, ModelPricing, per-provider pricing tables
- `crates/budi-core/src/hooks.rs` — HookEvent parsing, session upsert, prompt classification
- `crates/budi-core/src/otel.rs` — OTLP JSON parsing, OTEL→JSONL dedup
- `crates/budi-core/src/jsonl.rs` — JSONL transcript parser, ParsedMessage struct
- `crates/budi-core/src/providers/claude_code.rs` — Claude Code provider (JSONL discovery, pricing)
- `crates/budi-core/src/providers/cursor.rs` — Cursor provider (Usage API, auth from state.vscdb)
- `crates/budi-core/src/migration.rs` — Schema v13, all migration paths
- `crates/budi-core/src/config.rs` — BudiConfig, StatuslineConfig, TagsConfig
- `crates/budi-daemon/src/main.rs` — HTTP server, ~38 routes
- `crates/budi-daemon/src/routes/hooks.rs` — /hooks/ingest, /sync, /sync/all, /sync/reset, /sync/status, /health, /health/integrations, /health/check-update endpoints
- `crates/budi-daemon/src/routes/analytics.rs` — All analytics + admin endpoints (summary, messages, projects, cost, models, activity, branches, tags, providers, statusline, tools, mcp, cache-efficiency, session-cost-curve, cost-confidence, subagent-cost, sessions, admin/providers, admin/schema, admin/migrate)
- `crates/budi-daemon/src/routes/otel.rs` — /v1/logs OTLP ingestion
- `crates/budi-cli/src/commands/statusline.rs` — Statusline rendering + installation
- `crates/budi-cli/src/mcp.rs` — MCP server handler (14 tools: analytics + config)
- `crates/budi-cli/src/commands/mcp.rs` — `mcp-serve` subcommand (stdio transport)
- `crates/budi-daemon/static/js/` — Dashboard JS (vanilla, no framework)

## Dev notes

- CLI never touches SQLite directly — all queries go through the daemon HTTP API
- CostEnricher is the single source of truth for cost — sets cost_cents during pipeline. Skips if cost already set (API data)
- `budi init` installs hooks in `~/.claude/settings.json` (CC) and `~/.cursor/hooks.json` (Cursor), plus OTEL env vars and MCP server
- **MCP server**: `budi mcp-serve` runs an MCP server over stdio. Installed into `~/.claude/settings.json` mcpServers by `budi init`. 14 tools for analytics (cost summary, models, projects, branches, tags, providers, tools, activity) and config (get_config, set_tag_rules, set_statusline_config, sync_data, get_status). Thin HTTP client to daemon — stdout is JSON-RPC only, logging to stderr
- Tags are auto-detected (provider, model, repo, ticket_id, etc.) + custom rules via `~/.config/budi/tags.toml`
- git_branch is a column on messages (not a tag) for fast queries
- **Dashboard** is multi-page at `/dashboard` with URL-based routing (vanilla JS, no framework):
  - `/dashboard` (Overview) — Summary cards (cost/tokens/messages), activity timeline, agents/models, projects/branches, tickets/activity types
  - `/dashboard/insights` — Cost confidence, cache efficiency, session cost curve (split: cost + count), speed mode, subagent vs main, tools, MCP servers
  - `/dashboard/sessions` — Session list with sort/search/pagination, drill-down to `/dashboard/sessions/:id` with session meta, tags, input token growth chart, message table
  - `/dashboard/settings` — Status, integrations, database info, paths, actions (sync/re-sync/migrate/check updates), help links
- Dashboard JS files: `state.js`, `utils.js`, `api.js`, `stats.js` (shared components), `views.js` (overview), `views-insights.js`, `views-sessions.js`, `views-settings.js`, `events.js` (routing/lifecycle)
- Analytics endpoints: `/analytics/summary`, `/analytics/messages`, `/analytics/projects`, `/analytics/cost`, `/analytics/models`, `/analytics/activity`, `/analytics/branches`, `/analytics/branches/{branch}`, `/analytics/tags`, `/analytics/providers`, `/analytics/statusline`, `/analytics/tools`, `/analytics/mcp`, `/analytics/cache-efficiency`, `/analytics/session-cost-curve`, `/analytics/cost-confidence`, `/analytics/subagent-cost`, `/analytics/sessions`, `/analytics/sessions/{id}/messages`, `/analytics/sessions/{id}/tags`
- Admin endpoints: `/admin/providers` (registered providers), `/admin/schema` (schema version), `/admin/migrate` (run migration)
- Sync endpoints: `/sync` (7-day), `/sync/all` (full history), `/sync/reset` (wipe sync state + full re-sync), `/sync/status` (syncing flag + last_synced)
- Health endpoints: `/health` (ok + version), `/health/integrations` (hooks/MCP/OTEL/statusline status + DB stats + paths), `/health/check-update` (GitHub releases)
