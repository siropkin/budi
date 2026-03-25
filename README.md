# budi

[![CI](https://github.com/siropkin/budi/actions/workflows/ci.yml/badge.svg)](https://github.com/siropkin/budi/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/siropkin/budi)](https://github.com/siropkin/budi/releases/latest)
[![License](https://img.shields.io/github/license/siropkin/budi)](https://github.com/siropkin/budi/blob/main/LICENSE)
[![GitHub stars](https://img.shields.io/github/stars/siropkin/budi?style=social)](https://github.com/siropkin/budi)

**AI cost analytics for coding agents.** Know where your AI tokens and money go.

`budi` is a local-first cost analytics tool for AI coding agents. It tracks tokens, costs, and usage across Claude Code, Cursor, and more — so you can answer "how much did this feature cost?" No cloud. No uploads.

### Agent integrations

budi is built on a pluggable provider architecture — each AI coding agent is a provider that's auto-detected at runtime. Today it fully supports Claude Code and Cursor; more agents are coming.

| Agent | Status | Tokens | Cost | Detection |
|-------|--------|--------|------|-----------|
| **Claude Code** | Supported | Per-message | Per-model pricing | JSONL transcripts + hooks |
| **Cursor** | Supported | Per-request | Exact from API | Usage API + hooks |
| **GitHub Copilot CLI** | Planned | | | `~/.copilot/` |
| **Codex CLI** | Planned | | | `~/.codex/` |
| **Cline** | Planned | | | VS Code globalStorage |
| **Aider** | Planned | | | `.aider.chat.history.md` |
| **Gemini CLI** | Planned | | | `~/.gemini/` |

Agents are detected automatically — when a new agent's data directory appears, the next sync picks it up with zero config.

<p align="center">
  <img src="assets/dashboard-stats.png" alt="budi dashboard — stats page" width="800">
</p>

<p align="center">
  <img src="assets/cli-demo.gif" alt="budi CLI — stats, cost, sessions" width="800">
</p>

## How it works

Budi has a pluggable **provider** architecture. Each AI coding agent is a provider that knows how to discover and parse that agent's local data. A lightweight Rust daemon (port 7878) syncs data from all detected providers into a single SQLite database every 30 seconds, powering the dashboard and CLI. The CLI is a thin HTTP client — all queries go through the daemon.

**What budi does NOT collect:** file contents, prompt responses, or anything from the AI's output. Only metadata — timestamps, token counts, model names, and costs.

### Claude Code

Budi reads Claude Code's JSONL transcript files under `~/.claude/projects/`. Each conversation turn is a line in the transcript with full token usage (input, output, cache read/write), model name, timestamps, and session metadata. The daemon syncs these files every 30 seconds. Additionally, budi installs hooks that capture real-time events: session lifecycle, tool usage durations, context pressure, and prompt classification.

### Cursor

Budi fetches exact per-request token usage and cost from Cursor's Usage API. Authentication is extracted automatically from Cursor's local `state.vscdb` database — no manual token setup needed. Each API event includes exact `inputTokens`, `outputTokens`, `cacheReadTokens`, and `totalCents`. Events are correlated to hook sessions for git branch, repo, and workspace attribution. JSONL agent transcripts serve as a fallback when the API is unavailable.

### Hooks

Both Claude Code and Cursor support lifecycle hooks that budi uses for real-time event capture. Hooks are installed automatically by `budi init` and provide:

| Data | Claude Code | Cursor |
|------|-------------|--------|
| Session start/end | SessionStart, SessionEnd | sessionStart, sessionEnd |
| Tool usage + duration | PostToolUse | postToolUse |
| Context pressure | PreCompact | preCompact |
| Subagent tracking | SubagentStop | subagentStop |
| Prompt classification | UserPromptSubmit | — |
| File modifications | — | afterFileEdit |

Hook data is stored in a `hook_events` table and used to generate additional tags (activity type, session duration, dominant tool, composer mode).

| Data | Source | Quality |
|------|--------|---------|
| **Tokens** | Usage API (`inputTokens`, `outputTokens`, `cacheReadTokens`) | Exact per-request |
| **Cost** | Usage API (`totalCents`) | Exact from Cursor billing |
| **Models** | Usage API (`model` field) | 100% coverage |
| **Git branch** | Hook sessions (`workspace_root` → `.git/HEAD`) | From session context |

Cursor is auto-detected. Run `budi sync` and Cursor data appears alongside Claude Code in all views.

## Features

- **Built with Rust** — ~6 MB binary, minimal CPU/memory footprint
- **Local-first** — all data stays on your machine in a SQLite database, no cloud, no uploads
- **Automatic** — data syncs every 30 seconds in the background, no workflow changes needed
- **Per-repo tracking** — automatically identifies repos by git remote, merges worktrees and clones
- **Cost attribution** — cost per branch, ticket (auto-extracted from branch names), team, and custom tags
- **Message-level analytics** — per-message token usage and cost attribution
- **Multi-agent dashboard** — unified stats view across Claude Code, Cursor, and more
- **Live status line** — cost stats in Claude Code, with customizable data slots and format templates
- **Web dashboard** — analytics UI at `http://localhost:7878/dashboard`
- **Tags system** — flexible tagging with auto-detected tags (repo, branch, ticket, model) and custom rules via `~/.config/budi/tags.toml`
- **Hooks integration** — real-time event capture from Claude Code and Cursor hooks for session lifecycle, tool usage, and prompt classification

## How budi compares

| | budi | ccusage | Claude `/cost` |
|---|---|---|---|
| Multi-agent support | **Yes** (Claude Code + Cursor) | Claude Code only | Claude Code only |
| Cost history | **Per-message + daily** | Per-session | Current session |
| Web dashboard | **Yes** | No | No |
| Status line | **Yes** (Claude Code + Starship) | No | No |
| Per-repo breakdown | **Yes** | No | No |
| Cost attribution (branch/ticket) | **Yes** | No | No |
| Privacy | 100% local | Local | Built-in |
| Setup | `budi init` | `npx ccusage` | Built-in |
| Built with | Rust | TypeScript | — |

## Install

### Quick start (paste into your AI coding agent)

> Install budi from https://github.com/siropkin/budi following the install instructions in the README

Your AI agent will clone the repo, run the installer, and set up your project automatically.

### Manual install

**Step 1 — Install binaries**

macOS / Linux (installs binaries and adds them to your PATH automatically):
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

**Step 2 — Initialize budi**

```bash
cd /path/to/your/repo
budi init
```

This starts the daemon, installs the status line, sets up hooks for Claude Code and Cursor, and syncs existing transcripts. Data syncs automatically every 30 seconds.

**Step 3 — Use your AI coding agent normally.** Budi tracks your sessions in the background.

## Status line

Budi adds a live status line to Claude Code that shows cost metrics at a glance. It is installed automatically when you run `budi init`.

<p align="center">
  <img src="assets/statusline.png" alt="budi status line in Claude Code">
</p>

### Default layout

Example: `📊 budi · $12.50 today · $87.30 week · $1.2K month`

| Field | Description |
|-------|-------------|
| **📊 budi** | Clickable link to open the web dashboard |
| **today** | Total cost across all agents today |
| **week** | Total cost this week (Monday–Sunday) |
| **month** | Total cost this month |

### Configurable slots

You can customize what the status line displays by creating `~/.config/budi/statusline.toml`:

```toml
# Choose which data slots to display (in order)
slots = ["today", "week", "month", "branch"]
```

Available slots:

| Slot | Description |
|------|-------------|
| `today` | Today's cost |
| `week` | This week's cost |
| `month` | This month's cost |
| `session` | Current session's cost |
| `branch` | Current git branch's total cost |
| `project` | Current project's total cost |
| `provider` | Active provider name |

### Format templates

For full control over the output, use a format template:

```toml
slots = ["today", "branch"]
format = "{today} | {branch}"
```

Use `--format=custom` to render the template:

```bash
budi statusline --format=custom
```

### Manual install / reinstall

If you skipped `budi init` or need to reinstall the status line:

```bash
budi statusline --install
```

This writes the status line config to `~/.claude/settings.json`. Restart Claude Code to activate.

### Starship integration

If you use [Starship](https://starship.rs/), add this to `~/.config/starship.toml`:

```toml
[custom.budi]
command = "budi statusline --format=starship"
when = "command -v budi-daemon"
format = "[$output]($style) "
style = "cyan"
shell = ["sh"]
```

### Output formats

```bash
budi statusline                    # Claude Code format (ANSI + OSC 8 links)
budi statusline --format=starship  # plain text for shell prompts
budi statusline --format=json      # JSON for scripting
budi statusline --format=custom    # custom format from ~/.config/budi/statusline.toml
```

## Web dashboard

Run `budi open` to open the web UI in your browser, or click "budi" in the status line.

The dashboard shows: cost cards, activity chart, agents breakdown, models (per-provider), projects, branches (cost per git branch), tickets (cost per ticket ID), and a messages table with search and sorting.

## CLI commands

```bash
budi init                     # start daemon, install statusline, sync data
budi doctor                   # check health: daemon, database, config
budi open                     # open the web dashboard in the browser
budi stats                    # usage summary with cost breakdown
budi stats --models           # model usage breakdown
budi stats --projects         # repositories ranked by usage
budi stats --branches         # branches ranked by cost
budi stats --branch <name>    # cost for a specific branch
budi stats --tag ticket_id    # cost per ticket (auto-extracted from branch names)
budi stats --tag ticket_prefix # cost per team prefix (e.g. PAVA, SEN)
budi stats --provider <name>  # filter by provider (e.g. claude_code, cursor)
budi sync                     # sync recent transcripts (last 7 days)
budi history                  # load full transcript history (all time)
budi update                   # check for updates and install the latest version
budi --version                # print version information
```

All data commands support `--period today|week|month|all` and `--json` for scripting:

```bash
budi stats --period today --json          # pipe to jq, scripts, or dashboards
```

## Tags & cost attribution

Budi automatically tags every message with metadata extracted during ingestion:

| Tag | Source | Example |
|-----|--------|---------|
| `provider` | Agent name | `claude_code`, `cursor` |
| `model` | Model used | `claude-opus-4-6` |
| `repo` | Git remote URL | `github.com/user/repo` |
| `branch` | Git branch name | `feature/PAVA-2057-auth` |
| `ticket_id` | Extracted from branch (`[A-Z]+-\d+`) | `PAVA-2057` |
| `ticket_prefix` | Ticket prefix | `PAVA` |
| `activity` | Prompt classification (hooks) | `feature`, `bugfix`, `refactor`, `question` |
| `composer_mode` | Cursor session mode (hooks) | `agent`, `ask`, `edit` |
| `permission_mode` | Claude Code mode (hooks) | `default`, `auto`, `plan` |
| `duration` | Session duration bucket (hooks) | `short`, `medium`, `long` |
| `dominant_tool` | Most-used tool in session (hooks) | `Bash`, `Edit`, `Read` |
| `user_email` | User email from hooks | `user@example.com` |

### Custom tag rules

Create `~/.config/budi/tags.toml` to add your own tags:

```toml
[[rules]]
key = "team"
value = "platform"
match_repo = "github.com/verkada/Verkada-Web"

[[rules]]
key = "team"
value = "backend"
match_repo = "*Verkada-Backend*"

[[rules]]
key = "org"
value = "mycompany"
# No match_repo → applies to all messages
```

Query tags via CLI or API:

```bash
budi stats --tag ticket_id              # cost per ticket
budi stats --tag ticket_prefix          # cost per team prefix
budi stats --tag ticket_id --json       # JSON output for scripting
```

## Daemon API

The daemon (`budi-daemon`) runs on `http://127.0.0.1:7878` and exposes a REST API. The dashboard and CLI both use these endpoints.

### System

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/health` | Health check |
| POST | `/sync` | Sync recent data (last 7 days) |
| POST | `/history` | Load full transcript history (all time) |
| POST | `/migrate` | Run database schema migration |
| POST | `/hooks/ingest` | Receive hook events from Claude Code / Cursor |

### Analytics

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/analytics/summary` | Cost and token totals |
| GET | `/analytics/messages` | Message list (paginated, searchable) |
| GET | `/analytics/projects` | Repositories ranked by usage |
| GET | `/analytics/branches` | Cost per git branch |
| GET | `/analytics/branches/{branch}` | Cost for a specific branch |
| GET | `/analytics/cost` | Cost breakdown |
| GET | `/analytics/models` | Model usage breakdown |
| GET | `/analytics/providers` | Per-provider breakdown |
| GET | `/analytics/provider-count` | Number of distinct providers |
| GET | `/analytics/registered-providers` | Available providers |
| GET | `/analytics/activity` | Token activity over time (bucketed) |
| GET | `/analytics/context-usage` | Context window stats |
| GET | `/analytics/tags` | Cost breakdown by tag |
| GET | `/analytics/sessions` | Session list with lifecycle metadata |
| GET | `/analytics/tools` | Tool usage frequency and duration |
| GET | `/analytics/statusline` | Day/week/month/session/branch/project costs |
| GET | `/analytics/schema-version` | Current and target schema version |

Most analytics endpoints accept `?since=<ISO>&until=<ISO>` for date filtering.

## Architecture

```
┌──────────┐    HTTP     ┌──────────────┐    SQLite    ┌──────────┐
│ budi CLI │ ──────────▶ │ budi-daemon  │ ───────────▶ │  budi.db │
└──────────┘             │  (port 7878) │              └──────────┘
                         │              │                    ▲
┌──────────┐    HTTP     │  - 30s sync  │    Pipeline       │
│ Dashboard│ ──────────▶ │  - analytics │ ──────────────────┘
└──────────┘             │  - hooks     │    Extract → Normalize
                         └──────────────┘      → Enrich → Load
                            ▲   ▲   ▲
                 JSONL ─────┘   │   └───── Cursor API
               (transcripts)    │       (usage events)
                                │
┌──────────┐  hooks    ┌──────────┐  hooks
│ Claude   │ ──────────│ budi hook│──────── Cursor
│ Code     │  (stdin)  │  (CLI)   │ (stdin)
└──────────┘           └──────────┘
```

The daemon is the single source of truth — the CLI never opens the database directly.

## Roadmap

- **More agents** — Copilot CLI, Codex CLI, Cline, Aider, Gemini CLI (see [agent integrations](#agent-integrations) above)
- **Budget alerts** — threshold notifications for daily/weekly/monthly spend
- **Homebrew distribution** — `brew install budi`
- **Team features** — shared dashboards, per-developer breakdown

## Privacy

Everything runs locally. No cloud services. No data leaves your machine. Budi only stores metadata (timestamps, token counts, model names, costs) — never file contents or prompt responses.

## License

[MIT](LICENSE)
