# budi

[![CI](https://github.com/siropkin/budi/actions/workflows/ci.yml/badge.svg)](https://github.com/siropkin/budi/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/siropkin/budi)](https://github.com/siropkin/budi/releases/latest)
[![License](https://img.shields.io/github/license/siropkin/budi)](https://github.com/siropkin/budi/blob/main/LICENSE)
[![GitHub stars](https://img.shields.io/github/stars/siropkin/budi?style=social)](https://github.com/siropkin/budi)

**WakaTime for AI coding agents.** See where your tokens go.

`budi` tracks every AI coding session — tokens, costs, prompts, and context composition — in a local-first analytics dashboard. No cloud. No uploads. Just insight into your AI spend.

### Agent integrations

budi is built on a pluggable provider architecture — each AI coding agent is a provider that's auto-detected at runtime. Today it fully supports Claude Code; more agents are coming.

| Agent | Status | Tokens | Cost | Sessions | Lines | Detection |
|-------|--------|--------|------|----------|-------|-----------|
| **Claude Code** | Supported | Per-message | Per-model pricing | Via hooks | — | `~/.claude/` |
| **Cursor** | Supported | Per-session (contextTokensUsed) | Per-model pricing | Via state.vscdb | +/- lines | `~/Library/Application Support/Cursor/` |
| **GitHub Copilot CLI** | Planned | | | | | `~/.copilot/` |
| **Codex CLI** | Planned | | | | | `~/.codex/` |
| **Cline** | Planned | | | | | VS Code globalStorage |
| **Aider** | Planned | | | | | `.aider.chat.history.md` |
| **Gemini CLI** | Planned | | | | | `~/.gemini/` |

Agents are detected automatically — when a new agent's data directory appears, the next `budi sync` picks it up with zero config.

<p align="center">
  <img src="assets/dashboard-stats.png" alt="budi dashboard — stats page" width="800">
</p>

<p align="center">
  <img src="assets/dashboard-insights.png" alt="budi dashboard — insights page" width="800">
</p>

<p align="center">
  <img src="assets/cli-demo.gif" alt="budi CLI — stats, cost, sessions" width="800">
</p>

## How it works

Budi has a pluggable **provider** architecture. Each AI coding agent is a provider that knows how to discover and parse that agent's local data. A lightweight Rust daemon (port 7878) syncs data from all detected providers into a single SQLite database, powering the dashboard and CLI.

**What budi does NOT collect:** file contents, prompt responses, or anything from the AI's output. Only metadata — timestamps, token counts, tool names, file paths, and costs.

### Claude Code (full support)

Budi uses [Claude Code hooks](https://docs.anthropic.com/en/docs/claude-code/hooks) — the official event system that lets external tools observe what Claude Code does in real time. When you run `budi init`, it registers hooks in `.claude/settings.local.json`:

| Hook | What budi captures |
|------|-------------------|
| **SessionStart** | New session begins — records session ID, repo, timestamp |
| **UserPromptSubmit** | Every prompt you send — prompt text, model, token counts |
| **PostToolUse** | File operations (Read, Write, Edit, Glob) — which files Claude touches |
| **SubagentStart** | Sub-agent spawns — tracks parallel work |
| **Stop** | Session ends — finalizes duration, total cost |

Hooks fire as HTTP calls to the daemon. Hook responses return in sub-millisecond time, so you never notice them. The ~6 MB binary handles everything: data collection, analytics, web dashboard, and CLI.

### Cursor (full support)

Budi reads Cursor's `state.vscdb` SQLite database — the internal store where Cursor keeps composer session data. This provides ground-truth cost data, per-model usage breakdowns, lines changed, and session metadata. No proxy or API interception needed.

| Data | Source | Quality |
|------|--------|---------|
| **Cost** | Estimated from tokens × model pricing | Per-model API rates |
| **Models** | `composerData.modelConfig.modelName` | Exact model name |
| **Tokens** | `composerData.contextTokensUsed` (input) + estimated output | Session-level totals |
| **Lines changed** | `composerData.totalLinesAdded/Removed` | Per-session totals |
| **Sessions** | `composerData` entries in globalStorage + workspaceStorage | Titles, timestamps, agent vs composer mode |

Note: Cursor's `usageData` field (which previously contained per-model cost breakdowns) is empty in recent Cursor versions. Budi falls back to `contextTokensUsed` for token data and estimates cost using published API rates.

Cursor is auto-detected. Run `budi sync` and Cursor sessions appear alongside Claude Code data in all views.

## Features

- **Built with Rust** — ~6 MB binary, sub-millisecond hook latency, minimal CPU/memory footprint
- **Local-first** — all data stays on your machine in a SQLite database, no cloud, no uploads
- **Automatic** — data collection runs silently in the background, no workflow changes needed
- **Per-repo tracking** — automatically identifies repos by git remote, merges worktrees and clones
- **Session analytics** — prompt counts, token usage, and cost per session
- **Multi-agent dashboard** — unified view across Claude Code, Cursor, and more
- **Status line** — live session stats in your Claude Code terminal (shows all agents)
- **Web dashboard** — multi-page analytics UI at `http://localhost:7878/dashboard`
- **Insights** — actionable recommendations based on your usage patterns

## How budi compares

| | budi | ccusage | Sniffly | Claude `/cost` |
|---|---|---|---|---|
| Real-time tracking | **Yes** (Claude Code hooks) | No (parses logs) | No (parses logs) | Live only |
| Multi-agent support | **Yes** (Claude Code + Cursor) | Claude Code only | Claude Code only | Claude Code only |
| Cost history | **Per-session + daily** | Per-session | Per-session | Current session |
| Web dashboard | **Yes** (5 pages) | No | Yes | No |
| Status line | **Yes** (Claude Code + Starship) | No | No | No |
| Insights & recs | **Yes** | No | No | No |
| Per-repo breakdown | **Yes** | No | No | No |
| File activity tracking | **Yes** (Claude Code PostToolUse) | No | No | No |
| Multi-machine sync | **Planned** | No | No | No |
| Privacy | 100% local | Local | Local | Built-in |
| Setup | `budi init` | `npx ccusage` | `sniffly init` | Built-in |
| Built with | Rust | TypeScript | Python | — |

## Install

### Quick start (paste into your AI coding agent)

> Install budi from https://github.com/siropkin/budi following the install instructions in the README

Your AI agent will clone the repo, run the installer, and set up your project automatically.

### Manual install

**Step 1 — Install binaries**

macOS / Linux:
```bash
curl -fsSL https://raw.githubusercontent.com/siropkin/budi/main/scripts/install-standalone.sh | sh
```

Windows (PowerShell):
```powershell
irm https://raw.githubusercontent.com/siropkin/budi/main/scripts/install-standalone.ps1 | iex
```

Or build from source (requires Rust toolchain):

```bash
git clone https://github.com/siropkin/budi.git && cd budi && ./scripts/install.sh
```

**Step 2 — Set up Claude Code hooks**

Global (recommended — works for all repos and worktrees):
```bash
budi init --global
```

Or per-repo:
```bash
cd /path/to/your/repo
budi init
```

This installs Claude Code hooks, starts the daemon, and adds the status line to your Claude Code settings. Restart Claude Code so hook settings take effect.

**Step 3 — Use your AI coding agent normally.** Budi tracks your sessions in the background.

## Status line

Budi adds a live status line to Claude Code that shows key metrics at a glance. It is installed automatically when you run `budi init`.

<p align="center">
  <img src="assets/statusline.png" alt="budi status line in Claude Code" width="800">
</p>

### Fields

| Field | Description |
|-------|-------------|
| **today** | Total cost across all agents today |
| **week** | Total cost this week (Monday–Sunday) |
| **month** | Total cost this month |
| **↗ dashboard** | Clickable link to open the web dashboard |

Example: `📊 budi · $12.50 today · $87.30 week · $1.2K month · ↗ dashboard`

### Manual install / reinstall

If you skipped `budi init` or need to reinstall the status line:

```bash
budi statusline --install
```

This writes the status line config to `~/.claude/settings.json`. Restart Claude Code to activate.

## Starship integration

If you use [Starship](https://starship.rs/), budi automatically adds a shell prompt module so you see your AI spending in every terminal — not just inside Claude Code.

`budi init` detects Starship and appends a `[custom.budi]` module to `~/.config/starship.toml`. The result looks like:

```
~/projects/myapp on main via 🐍 v3.14.3 via 🦀 v1.93.1  $12.50 · $87.30 · $1.2K
❯
```

The three values are today / week / month cost, displayed in cyan to match Starship's style.

### Manual setup

If you installed Starship after `budi init`, run `budi init` again (it's idempotent) or add this to `~/.config/starship.toml`:

```toml
[custom.budi]
command = "budi statusline --format=starship"
when = "command -v budi-daemon"
format = "[$output]($style) "
style = "cyan"
shell = ["sh"]
```

`budi doctor` will warn if Starship is installed but the budi module is missing.

### Output formats

The `budi statusline` command supports multiple output formats:

```bash
budi statusline                    # Claude Code format (ANSI + OSC 8 links)
budi statusline --format=starship  # plain text for shell prompts
budi statusline --format=json      # JSON for scripting
```

## Web dashboard

Run `budi open` to open the web UI in your browser, or click the dashboard link in the status line.

| Page | What it shows |
|------|---------------|
| **Stats** | Cost, tokens, activity chart, agents, models (per-provider), projects, branches (cost per git branch), tools, MCP, sessions table with search |
| **Insights** | Recommendations, session patterns, tool diversity, daily cost trend, search/cache efficiency, context window usage, config health |
| **Setup** | Integrations (Claude Code statusline, Starship), config files, plugins, permissions — all with search |
| **Plans** | Plan files with server-side search and pagination |
| **Prompts** | Prompt history with server-side search and pagination |

All tables support search, sortable columns, and paginated "Show more". Sessions use server-side pagination (handles 20K+ sessions efficiently).

## CLI commands

```bash
budi init                     # set up hooks, daemon, and status line
budi init --global            # install hooks globally (all repos and worktrees)
budi doctor                   # check installation health
budi open                     # open the web dashboard in the browser
budi stats                    # usage summary with cost breakdown
budi stats --sessions         # list sessions with stats
budi stats --models           # model usage breakdown
budi stats --projects         # repositories ranked by usage
budi stats --session <id>     # per-session detail
budi stats --provider <name>  # filter by provider (e.g. claude_code, cursor)
budi insights                 # actionable recommendations
budi sync                     # sync all providers into the analytics database
budi update                   # check for updates and install the latest version
budi version                  # print version information
budi statusline               # print the status line (used internally by Claude Code)
budi statusline --install     # install status line in ~/.claude/settings.json
budi statusline --format=starship  # plain text output for Starship / shell prompts
budi statusline --format=json      # JSON output for scripting
```

All data commands support `--period today|week|month|all` and `--json` for scripting:

```bash
budi stats --period today --json          # pipe to jq, scripts, or dashboards
budi stats --sessions --json | jq '.[0]'  # get latest session as JSON
```

## Daemon API

The daemon (`budi-daemon`) runs on `http://127.0.0.1:7878` and exposes a REST API. The dashboard and CLI both use these endpoints.

### System

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/health` | Health check |
| POST | `/sync` | Trigger JSONL→SQLite sync |
| POST | `/status` | Repo status (used by hooks) |
| GET | `/system/integrations` | Integration status (statusline, Starship) |

### Hooks

| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | `/hook/prompt-submit` | Claude Code prompt hook |
| POST | `/hook/tool-use` | Claude Code tool use hook |

### Analytics (from SQLite)

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/analytics/summary` | Cost and token totals |
| GET | `/analytics/sessions` | Session list (paginated, searchable) |
| GET | `/analytics/session/{id}` | Single session detail |
| GET | `/analytics/projects` | Repositories ranked by usage |
| GET | `/analytics/branches` | Cost per git branch |
| GET | `/analytics/cost` | Cost breakdown |
| GET | `/analytics/models` | Model usage breakdown |
| GET | `/analytics/providers` | Per-provider breakdown |
| GET | `/analytics/activity` | Token activity over time (bucketed) |
| GET | `/analytics/top-tools` | Tool usage ranking |
| GET | `/analytics/mcp-tools` | MCP tool usage |
| GET | `/analytics/active-sessions` | Currently running sessions |
| GET | `/analytics/context-usage` | Context window stats |
| GET | `/analytics/interaction-modes` | Agent vs normal mode breakdown |
| GET | `/analytics/statusline` | Day/week/month costs (used by status line) |
| GET | `/analytics/insights` | Actionable recommendations |

### Analytics (from local files)

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/analytics/timeline` | Activity timeline |
| GET | `/analytics/config-files` | Config file listing |
| GET | `/analytics/plugins` | Installed plugins |
| GET | `/analytics/plans` | Plan files (paginated, searchable) |
| GET | `/analytics/prompts` | Prompt history (paginated, searchable) |
| GET | `/analytics/memory` | Memory files |
| GET | `/analytics/permissions` | Permission settings |
| GET | `/analytics/registered-providers` | Available providers |

Most analytics endpoints accept `?since=<ISO>&until=<ISO>` for date filtering and `?tz_offset=<minutes>` for timezone adjustment.

## Roadmap

- **More agents** — Copilot CLI, Codex CLI, Cline, Aider, Gemini CLI (see [agent integrations](#agent-integrations) above)
- **Multi-machine sync** — aggregate stats across devices
- **AI commit attribution** — per-commit AI contribution % from Cursor's `ai-code-tracking.db`

## Privacy

Everything runs locally. No cloud services. No data leaves your machine. Budi only stores metadata (timestamps, token counts, file paths, costs) — never file contents or prompt responses.

## License

[MIT](LICENSE)
