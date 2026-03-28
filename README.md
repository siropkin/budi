# budi

[![CI](https://github.com/siropkin/budi/actions/workflows/ci.yml/badge.svg)](https://github.com/siropkin/budi/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/siropkin/budi)](https://github.com/siropkin/budi/releases/latest)
[![License](https://img.shields.io/github/license/siropkin/budi)](https://github.com/siropkin/budi/blob/main/LICENSE)
[![GitHub stars](https://img.shields.io/github/stars/siropkin/budi?style=social)](https://github.com/siropkin/budi)

**Local-first cost analytics for AI coding agents.** See where your tokens and money go across Claude Code, Cursor, and more.

```bash
brew install siropkin/budi/budi && budi init
```

No cloud. No uploads. Everything stays on your machine.

<p align="center">
  <img src="assets/dashboard-overview.png" alt="budi dashboard — cost overview" width="800">
</p>

<details>
<summary>More dashboard pages</summary>

**Insights** — cache efficiency, session cost curve, tool usage, subagent costs

<p align="center">
  <img src="assets/dashboard-insights.png" alt="budi insights" width="800">
</p>

**Sessions** — searchable session list with drill-down to individual messages

<p align="center">
  <img src="assets/dashboard-sessions.png" alt="budi sessions" width="800">
</p>

**Settings** — integration status, database info, sync controls

<p align="center">
  <img src="assets/dashboard-settings.png" alt="budi settings" width="800">
</p>

</details>

## What it does

- Tracks tokens, costs, and usage per message across AI coding agents
- **Exact cost** via OpenTelemetry for Claude Code (includes thinking tokens)
- Attributes cost to repos, branches, tickets, and custom tags
- Web dashboard at `http://localhost:7878/dashboard`
- Live cost status line in Claude Code
- Background sync every 30 seconds — no workflow changes needed
- ~6 MB Rust binary, minimal footprint

## Supported agents

| Agent | Status | How |
|-------|--------|-----|
| **Claude Code** | Supported | OpenTelemetry (exact cost) + JSONL transcripts + hooks |
| **Cursor** | Supported | Usage API + hooks |
| **Copilot CLI, Codex CLI, Cline, Aider, Gemini CLI** | Planned | |

## Install

Use Homebrew if you have it. Otherwise use the shell script (macOS/Linux) or PowerShell script (Windows). Build from source only if you want to contribute.

**Homebrew (macOS / Linux):** requires [Homebrew](https://brew.sh/)

```bash
brew install siropkin/budi/budi && budi init
```

**Shell script (macOS / Linux):** requires `curl` and `tar` (glibc-based systems only; Alpine/musl users should build from source)

```bash
curl -fsSL https://raw.githubusercontent.com/siropkin/budi/main/scripts/install-standalone.sh | bash
```

**Windows (PowerShell):** requires PowerShell 5.1+

```powershell
irm https://raw.githubusercontent.com/siropkin/budi/main/scripts/install-standalone.ps1 | iex
```

Windows notes: binaries install to `%LOCALAPPDATA%\budi\bin`. The daemon uses `taskkill` instead of `pkill`. PATH is updated in the user environment — restart your terminal after install.

**From source:** requires [Rust toolchain](https://rustup.rs/) — clones the repo and builds release binaries

```bash
git clone https://github.com/siropkin/budi.git && cd budi && ./scripts/install.sh
```

**Or paste this into your AI coding agent:**

> Install budi from https://github.com/siropkin/budi following the install instructions in the README

All installers automatically run `budi init` after installation. Homebrew users need to run `budi init` manually.

`budi init` starts the daemon, installs hooks for Claude Code and Cursor, configures OpenTelemetry for exact cost tracking, sets up the status line, and syncs existing data. **Restart Claude Code and Cursor** after install to activate hooks, telemetry, and the status line. The daemon uses port 7878 by default — make sure it's available (customize in `~/.config/budi/config.toml` with `daemon_port`).

To install a specific version, set the `VERSION` environment variable: `VERSION=v7.1.0 curl -fsSL ... | bash` (or `$env:VERSION="v7.1.0"` on PowerShell).

Run `budi doctor` to verify everything is set up correctly.

## Status line

Budi adds a live cost display to Claude Code, installed automatically by `budi init`:

`📊 budi · $12.50 today · $87.30 week · $1.2K month`

Customize slots in `~/.config/budi/statusline.toml`:

```toml
slots = ["today", "week", "month", "branch"]
```

Available slots: `today`, `week`, `month`, `session`, `branch`, `project`, `provider`.

For Starship integration, add to `~/.config/starship.toml`:

```toml
[custom.budi]
command = "budi statusline --format=starship"
when = "curl -sf http://localhost:7878/health >/dev/null 2>&1"
format = "[$output]($style) "
style = "cyan"
shell = ["sh"]
```

## Update

**Homebrew:**

```bash
brew upgrade budi && budi init
```

**Shell script / Windows / from source:**

```bash
budi update                      # downloads latest release, migrates DB, restarts daemon
budi update --version 7.1.0     # update to a specific version
```

**Restart Claude Code and Cursor** after updating to pick up any changes.

## CLI

```bash
budi init                     # start daemon, install hooks, sync data
budi open                     # open web dashboard
budi doctor                   # check health: daemon, database, config
budi stats                    # usage summary with cost breakdown
budi stats --models           # model usage breakdown
budi stats --projects         # repos ranked by cost
budi stats --branches         # branches ranked by cost
budi stats --branch <name>    # cost for a specific branch
budi stats --tag ticket_id    # cost per ticket
budi stats --tag ticket_prefix # cost per team prefix
budi sync                     # sync recent data (last 30 days)
budi sync --all               # load full history (all time)
budi sync --force             # re-ingest all data from scratch (use after upgrades)
budi update                   # check for updates (detects Homebrew)
budi update --version 7.1.0  # update to a specific version
budi uninstall                # remove hooks, status line, config, and data
budi uninstall --keep-data    # uninstall but keep analytics database
budi mcp-serve                # run MCP server (used by Claude Code, not called directly)
```

All data commands support `--period today|week|month|all` and `--format json`.

## Tags & cost attribution

Every message is automatically tagged with: `provider`, `model`, `repo`, `branch`, `ticket_id`, `ticket_prefix`, `activity`, `composer_mode`, `permission_mode`, `duration`, `dominant_tool`, `user_email`.

Add custom tags in `~/.config/budi/tags.toml`:

```toml
[[rules]]
key = "team"
value = "platform"
match_repo = "github.com/org/repo"

[[rules]]
key = "team"
value = "backend"
match_repo = "*Backend*"
```

## MCP server

Budi includes an MCP (Model Context Protocol) server so AI agents can query your cost data and configure budi directly from conversation. Installed automatically by `budi init` into `~/.claude/settings.json`.

**Example prompts:**
- "What's my AI coding cost this week?"
- "Which model is costing me the most?"
- "Show me cost per branch this month"
- "Set up tag rules for my team repos"

**Available tools (14):**

| Tool | Description |
|------|-------------|
| `get_cost_summary` | Total cost, tokens, messages for a period |
| `get_model_breakdown` | Cost breakdown by model |
| `get_project_costs` | Cost breakdown by repo/project |
| `get_branch_costs` | Cost breakdown by git branch |
| `get_branch_detail` | Detailed stats for a specific branch |
| `get_tag_breakdown` | Cost breakdown by any tag key |
| `get_provider_breakdown` | Cost breakdown by agent (Claude Code, Cursor) |
| `get_tool_usage` | Tool call frequency + MCP server stats |
| `get_activity` | Daily activity chart data |
| `get_config` | Current budi configuration |
| `set_tag_rules` | Configure custom tag rules |
| `set_statusline_config` | Configure statusline slots |
| `sync_data` | Trigger data sync |
| `get_status` | Daemon health, schema, sync state |

All analytics tools accept a `period` parameter: `today`, `week`, `month`, `all` (default: `month`).

The MCP server is a thin HTTP client to the daemon — it never touches the database directly. Communication uses stdio (JSON-RPC), and all logging goes to stderr.

## Privacy

Budi is 100% local — no cloud, no uploads, no telemetry. All data stays on your machine in `~/.local/share/budi/`. Budi only stores metadata: timestamps, token counts, model names, and costs. It **never** reads, stores, or transmits file contents, prompt text, or AI responses.

## How it works

A lightweight Rust daemon (port 7878) receives real-time OpenTelemetry events, syncs JSONL transcripts, and processes hook events — merging all sources into a single SQLite database. The CLI is a thin HTTP client — all queries go through the daemon.

## Details

<details>
<summary>How budi compares</summary>

| | budi | ccusage | Claude `/cost` |
|---|---|---|---|
| Multi-agent support | **Yes** (Claude Code + Cursor) | Claude Code only | Claude Code only |
| Exact cost (incl. thinking tokens) | **Yes** (via OTEL) | No | Approximate |
| Cost history | **Per-message + daily** | Per-session | Current session |
| Web dashboard | **Yes** | No | No |
| Status line | **Yes** (Claude Code + Starship) | No | No |
| Per-repo breakdown | **Yes** | No | No |
| Cost attribution (branch/ticket) | **Yes** | No | No |
| Privacy | 100% local | Local | Built-in |
| Setup | `budi init` | `npx ccusage` | Built-in |
| Built with | Rust | TypeScript | — |

</details>

<details>
<summary>Architecture</summary>

```
┌──────────┐    HTTP     ┌──────────────┐    SQLite    ┌──────────┐
│ budi CLI │ ──────────▶ │ budi-daemon  │ ───────────▶ │  budi.db │
└──────────┘             │  (port 7878) │              └──────────┘
                         │              │                    ▲
┌──────────┐    HTTP     │  - OTEL recv │    Pipeline       │
│ Dashboard│ ──────────▶ │  - 30s sync  │ ──────────────────┘
└──────────┘             │  - analytics │    Extract → Normalize
                         │  - hooks     │      → Enrich → Load
┌──────────┐    HTTP     └──────────────┘
│ MCP      │ ──────────▶ (stdio JSON-RPC, 14 tools)
│ Server   │  thin client
└──────────┘
                          ▲   ▲   ▲   ▲
             OTEL ────────┘   │   │   └───── Cursor API
         (exact cost)         │   │       (usage events)
                   JSONL ─────┘   │
                 (transcripts)    │
                                  │
┌──────────┐  hooks    ┌──────────┐  hooks
│ Claude   │ ──────────│ budi hook│──────── Cursor
│ Code     │  (stdin)  │  (CLI)   │ (stdin)
└──────────┘           └──────────┘
  │
  └── OTLP HTTP/JSON ──▶ POST /v1/logs (auto-configured)
```

The daemon is the single source of truth — the CLI never opens the database directly. Each message row is enriched from multiple sources: OTEL provides exact cost, JSONL provides context (parent messages, working directory), and hooks provide session metadata (repo, branch, user).

**Data model** — six tables, four data entities + two supporting:

| Table | Role |
|-------|------|
| **messages** | Single cost entity — all token/cost data lives here (one row per API call) |
| **sessions** | Lifecycle context (start/end, duration, mode) without mixing cost concerns |
| **hook_events** | Raw event log for tool stats and MCP tracking |
| **otel_events** | Raw OpenTelemetry event storage for debugging/audit |
| **tags** | Flexible key-value pairs per message (repo, ticket, activity, user, etc.) |
| **sync_state** | Tracks incremental ingestion progress per file for progressive sync |

</details>

<details>
<summary>Hooks</summary>

Both Claude Code and Cursor support lifecycle hooks that budi uses for real-time event capture. Hooks are installed automatically by `budi init` into `~/.claude/settings.json` and `~/.cursor/hooks.json`. They are non-blocking (`async: true`) and wrapped with `|| true` so that budi can never interfere with your coding agent — even if budi crashes or is uninstalled.

| Data | Claude Code | Cursor |
|------|-------------|--------|
| Session start/end | SessionStart, SessionEnd | sessionStart, sessionEnd |
| Tool usage + duration | PostToolUse | postToolUse |
| Context pressure | PreCompact | preCompact |
| Subagent tracking | SubagentStop | subagentStop |
| Prompt classification | UserPromptSubmit | — |
| File modifications | — | afterFileEdit |

</details>

<details>
<summary>OpenTelemetry (Claude Code)</summary>

When Claude Code has telemetry enabled, it sends OTLP HTTP/JSON events to budi's daemon for every API request. This provides **exact cost data** including thinking tokens — closing the accuracy gap that JSONL-only parsing has (JSONL's `output_tokens` doesn't include thinking tokens).

`budi init` automatically configures the following env vars in `~/.claude/settings.json`:

```json
{
  "env": {
    "CLAUDE_CODE_ENABLE_TELEMETRY": "1",
    "OTEL_EXPORTER_OTLP_ENDPOINT": "http://127.0.0.1:7878",
    "OTEL_EXPORTER_OTLP_PROTOCOL": "http/json",
    "OTEL_METRICS_EXPORTER": "otlp",
    "OTEL_LOGS_EXPORTER": "otlp"
  }
}
```

All telemetry stays local — it goes directly from Claude Code to budi's daemon on localhost. No data leaves your machine.

**How the data merges:** Each API call produces data from three sources. OTEL provides exact cost and token counts (including thinking tokens). JSONL provides message context (parent UUID, working directory, git branch). Hooks provide session metadata (repo, branch, user email). Budi merges all three into a single message row — regardless of which source arrives first.

**Cost confidence levels:**

| Level | Source | Accuracy |
|-------|--------|----------|
| `otel_exact` | OTEL `api_request` event | Exact (includes thinking tokens) |
| `exact` | Cursor Usage API / Claude Code JSONL tokens | Exact tokens, calculated cost |
| `estimated` | JSONL tokens x model pricing | ~92-96% accurate (missing thinking tokens) |

Messages with `otel_exact` or `exact` confidence show exact cost in the dashboard. Estimated costs are prefixed with `~`.

**If you already use OTEL elsewhere:** If `OTEL_EXPORTER_OTLP_ENDPOINT` is already set to a non-localhost URL, `budi init` won't overwrite it. You can use an [OTEL Collector](https://opentelemetry.io/docs/collector/) with multiple exporters to send data to both budi and your existing endpoint.

</details>

<details>
<summary>Daemon API</summary>

The daemon runs on `http://127.0.0.1:7878` and exposes a REST API.

**System:**

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/health` | Health check |
| POST | `/sync` | Sync recent data (last 30 days) |
| POST | `/sync/all` | Load full transcript history |
| POST | `/sync/reset` | Wipe sync state + full re-sync |
| GET | `/sync/status` | Syncing flag + last_synced |
| POST | `/hooks/ingest` | Receive hook events |
| GET | `/health/integrations` | Hooks/MCP/OTEL/statusline status + DB stats |
| GET | `/health/check-update` | Check for updates via GitHub |
| POST | `/v1/logs` | OTLP logs ingestion (exact cost from Claude Code) |
| POST | `/v1/metrics` | OTLP metrics ingestion (stub for future use) |

**Analytics:**

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/analytics/summary` | Cost and token totals |
| GET | `/analytics/messages` | Message list (paginated, searchable) |
| GET | `/analytics/projects` | Repos ranked by usage |
| GET | `/analytics/branches` | Cost per git branch |
| GET | `/analytics/branches/{branch}` | Cost for a specific branch |
| GET | `/analytics/cost` | Cost breakdown |
| GET | `/analytics/models` | Model usage breakdown |
| GET | `/analytics/providers` | Per-provider breakdown |
| GET | `/analytics/activity` | Token activity over time |
| GET | `/analytics/tags` | Cost breakdown by tag |
| GET | `/analytics/tools` | Tool usage frequency and duration |
| GET | `/analytics/mcp` | MCP server usage stats |
| GET | `/analytics/statusline` | Status line data |
| GET | `/analytics/cache-efficiency` | Cache hit rates and savings |
| GET | `/analytics/session-cost-curve` | Cost per message by session length |
| GET | `/analytics/cost-confidence` | Breakdown by cost confidence level |
| GET | `/analytics/subagent-cost` | Subagent vs main agent cost |
| GET | `/analytics/sessions` | Session list (paginated, searchable) |
| GET | `/analytics/sessions/{id}/messages` | Messages for a specific session |
| GET | `/analytics/sessions/{id}/tags` | Tags for a specific session |
| GET | `/admin/providers` | Registered providers |
| GET | `/admin/schema` | Database schema version |
| POST | `/admin/migrate` | Run database migration |

Most endpoints accept `?since=<ISO>&until=<ISO>` for date filtering.

</details>

## Troubleshooting

**Dashboard shows no data:**
1. Run `budi doctor` to check health
2. Run `budi sync` to sync recent transcripts
3. For full history: `budi sync --all`

**Daemon won't start:**
1. Check if port 7878 is in use: `lsof -i :7878`
2. Kill stale processes: `pkill -f "budi-daemon serve"`
3. Restart: `budi init`

**Hooks not working:**
1. Run `budi doctor` — it validates hook installation
2. Make sure you restarted Claude Code / Cursor after `budi init`
3. Re-install: `budi init` (safe to run multiple times)

**Status line not showing:**
1. Restart Claude Code after `budi init`
2. Check: `budi statusline` should output cost data

## Uninstall

```bash
budi uninstall          # stops daemon, removes hooks, status line, config, and data
```

`budi uninstall` removes hooks, status line, config, and data but **not** the binaries themselves. Remove binaries separately:

```bash
# Homebrew:
brew uninstall budi

# Shell script (macOS / Linux):
rm ~/.local/bin/budi ~/.local/bin/budi-daemon
# or use the full uninstall script:
curl -fsSL https://raw.githubusercontent.com/siropkin/budi/main/scripts/uninstall.sh | bash

# From source (cargo install):
cargo uninstall budi budi-daemon

# Windows (PowerShell):
irm https://raw.githubusercontent.com/siropkin/budi/main/scripts/uninstall-standalone.ps1 | iex
```

Options: `--keep-data` to preserve the analytics database and config, `--yes` to skip confirmation.

## Exit codes

`budi init` returns 0 on success, 2 on partial success (init completed but hooks had warnings), 1 on hard error.

## License

[MIT](LICENSE)
