# budi

[![CI](https://github.com/siropkin/budi/actions/workflows/ci.yml/badge.svg)](https://github.com/siropkin/budi/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/siropkin/budi)](https://github.com/siropkin/budi/releases/latest)
[![License](https://img.shields.io/github/license/siropkin/budi)](https://github.com/siropkin/budi/blob/main/LICENSE)
[![GitHub stars](https://img.shields.io/github/stars/siropkin/budi?style=social)](https://github.com/siropkin/budi)

**Local-first cost analytics for AI coding agents.** See where your tokens and money go across Claude Code, Cursor, and more.

```bash
brew install siropkin/budi/budi && budi init
```

Everything stays on your machine by default. Optional cloud sync pushes aggregated daily cost metrics to a team dashboard at `app.getbudi.dev` — prompts, code, and responses never leave your machine.

<p align="center">
  <img src="assets/demo.gif" alt="budi CLI demo" width="800">
</p>

<details>
<summary>Cloud Dashboard Screenshots</summary>

**Overview** — team-wide cost visibility
<p align="center">
  <img src="assets/dashboard-overview.png" alt="budi dashboard — cost overview" width="800">
</p>

**Repos & Tickets** — cost breakdown by project, branch, and ticket
<p align="center">
  <img src="assets/dashboard-repos.png" alt="budi repos" width="800">
</p>

**Sessions** — recent sessions across the team
<p align="center">
  <img src="assets/dashboard-sessions.png" alt="budi sessions" width="800">
</p>

</details>

## What it does

- Tracks tokens, costs, and usage per message across AI coding agents
- **Local transcript tailer** watches the JSONL/session files agents already write on disk and ingests them live
- Attributes cost to repos, branches, tickets, and custom tags
- **Session health** — detects context bloat, cache degradation, cost acceleration, and retry loops with actionable, provider-aware tips
- **Cloud dashboard** at [`app.getbudi.dev`](https://app.getbudi.dev) — team-wide cost visibility across users, repos, models, branches, and tickets (daily granularity, opt-in via `~/.config/budi/cloud.toml`)
- Provider-scoped cost status line in Claude Code and Cursor (quiet rolling `1d` / `7d` / `30d`)
- **One-time import** of historical transcripts via `budi db import` (Claude Code JSONL, Codex Desktop/CLI sessions, Copilot CLI sessions, Cursor Usage API)
- ~6 MB Rust binary, minimal footprint

## Platforms

budi targets **macOS**, **Linux** (glibc), and **Windows 10+**. Prebuilt release binaries are published for macOS (Intel + Apple Silicon), Linux (x86_64 + aarch64 glibc), and Windows x86_64. On Windows ARM64, the installer currently uses the x86_64 binary via emulation. Paths follow OS conventions (`HOME` / `USERPROFILE`, data under `~/.local/share/budi` on Unix and `%LOCALAPPDATA%\budi` on Windows). Daemon port takeover after upgrade uses `lsof`/`ps`/`kill` on Unix and **PowerShell `Get-NetTCPConnection`** plus `taskkill` on Windows (requires PowerShell, which is default on supported Windows versions).

## Supported agents

| Agent | Tier | Status | How |
|-------|------|--------|-----|
| **Claude Code** | Tier 1 | Supported | Live transcript tailing from local JSONL files |
| **Codex CLI** | Tier 1 | Supported | Live transcript tailing from local session files |
| **Cursor** | Tier 2 | Supported | Live transcript tailing plus Usage API cost reconciliation |
| **Copilot CLI** | Tier 2 | Supported | Live transcript tailing from local session files |
| **Gemini CLI** | Tier 3 | Deferred | Not part of the 8.2 scope |

Supported means Budi can observe the agent's normal local transcript/session artifacts. `budi init` does not wrap the agent, patch shell profiles, or rewrite editor settings in 8.2.

All agents also support one-time historical import via `budi db import` (Claude Code JSONL transcripts, Codex Desktop/CLI sessions, Copilot CLI sessions, Cursor Usage API).

## Ecosystem

- **[budi](https://github.com/siropkin/budi)** — The core Rust daemon and CLI tool (you are here)
- **[budi-cloud](https://github.com/siropkin/budi-cloud)** — Cloud dashboard and ingest API for team-wide cost visibility
- **[budi-cursor](https://github.com/siropkin/budi-cursor)** — VS Code/Cursor extension: a provider-scoped Cursor-only status bar (no sidebar) that mirrors the Claude Code statusline

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for local setup, quality checks, and PR workflow.
Architecture and module boundaries are documented in [SOUL.md](SOUL.md).
Architecture decision records live in [`docs/adr/`](docs/adr/). The 8.x local-developer-first product contract (persona priority, local/cloud boundary, classification principles, statusline contract, explicit 8.2 / 9.0 deferrals) is [ADR-0088](docs/adr/0088-8x-local-developer-first-product-contract.md).

Quick validation: `cargo fmt --all && cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --locked`

To report a bug or request a feature, open a GitHub issue using the repository templates so maintainers get reproducible details quickly.

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

Windows notes: binaries install to `%LOCALAPPDATA%\budi\bin`. Stopping or upgrading the daemon uses `taskkill` (or PowerShell) instead of Unix `pkill`. On startup, budi-daemon asks PowerShell for listeners on its port and terminates another `budi-daemon` if present. PATH is updated in the user environment — restart your terminal after install.

**From source:** requires [Rust toolchain](https://rustup.rs/)

```bash
git clone https://github.com/siropkin/budi.git && cd budi && ./scripts/install.sh
```

Windows source build (PowerShell):

```powershell
git clone https://github.com/siropkin/budi.git
cd budi
cargo build --release --locked
$BinDir = Join-Path $env:LOCALAPPDATA "budi\bin"
New-Item -ItemType Directory -Path $BinDir -Force | Out-Null
Copy-Item .\target\release\budi.exe $BinDir -Force
Copy-Item .\target\release\budi-daemon.exe $BinDir -Force
& (Join-Path $BinDir "budi.exe") init
```

**Or paste this into your AI coding agent:**

> Install budi from https://github.com/siropkin/budi following the install instructions in the README

`budi init` behavior by install method:

| Method | Runs `budi init` automatically? |
|---|---|
| Homebrew (`brew install ...`) | No — run `budi init` manually |
| Standalone shell script (`curl ... \| bash`) | Yes |
| Standalone PowerShell script (`irm ... \| iex`) | Yes |
| From source (`./scripts/install.sh`) | Yes |

If you install with Homebrew, run `budi init` right after `brew install`.

**One install on PATH.** Do not mix Homebrew with `~/.local/bin` (macOS/Linux) or with `%LOCALAPPDATA%\budi\bin` (Windows): you can end up with different `budi` and `budi-daemon` versions and confusing restarts. Keep a single install directory ahead of others on `PATH` (or remove duplicates). `budi init` warns if it detects multiple binaries.

`budi init` is intentionally small in 8.2: it creates the data directory, validates schema/binary state, starts the daemon on port 7878, installs the platform-native autostart service, prints any detected agents based on local transcript roots, and exits.

It does **not** patch shell profiles, Cursor settings, or Codex config files. Run your agent normally (`claude`, `codex`, `cursor`, `gh copilot`) and Budi will tail the transcripts those tools already write locally.

If you are upgrading from 8.0/8.1 and `budi doctor` reports leftover proxy-era config, run `budi init --cleanup` to review and remove managed Budi blocks with explicit consent.

To install a specific version, set the `VERSION` environment variable: `VERSION=v7.1.0 curl -fsSL ... | bash` (or `$env:VERSION="v7.1.0"` on PowerShell).

Run `budi doctor` to verify everything is set up correctly.

### First run checklist (5 minutes)

Use this sequence if you want the fastest "did setup really work?" path:

1. **Install and initialize**
   - Homebrew: `brew install siropkin/budi/budi` then `budi init`
   - Standalone installers and `./scripts/install.sh` already run `budi init` for you
   - `budi init` starts the daemon, installs autostart, shows detected agents, and exits
2. **Send your first prompt**
   - Open your agent as usual (`claude`, `codex`, `cursor`, `gh copilot`) and send a prompt
   - Budi tails the local transcript/session files the agent already writes
3. **Verify end-to-end** with `budi doctor`
   - Checks daemon, tailer readiness, schema, transcript visibility, and any leftover 8.0/8.1 proxy residue
   - The top of the report says "All checks passed." when first-run setup is healthy — and adds a friendly nudge if no activity has been recorded yet
4. **See today's cost** with `budi status`
   - Quick snapshot: daemon health, today's cost, and first-run hints when the DB is still quiet
5. **Import historical data** (optional)
   - Run `budi db import` to backfill from Claude Code JSONL transcripts, Codex Desktop/CLI sessions, Copilot CLI sessions, and Cursor Usage API
6. **Upgraded from 8.0/8.1?** (only if `budi doctor` warns)
   - Run `budi init --cleanup` to preview and remove managed proxy-era shell/editor config residue

### PATH and duplicate binary checks

If `budi` is "not found" or behavior looks inconsistent after an update, verify which binary is being executed:

- macOS/Linux:
  - `command -v budi`
  - `which -a budi`
- Windows (PowerShell):
  - `Get-Command budi -All`

Keep only one install source first on PATH (Homebrew **or** standalone path), not both.

## Status line

Budi adds a live cost display to Claude Code (optional in `budi init`). The default is intentionally quiet, stable, and scoped to the current agent surface — the Claude Code statusline shows Claude Code usage only (ADR-0088 §4):

`budi · $1.24 1d · $8.50 7d · $32.10 30d`

Rolling `1d` / `7d` / `30d` windows are the primary signal. They tell you what you spent in the last 24 hours, the last 7 days, and the last 30 days — not calendar-today or calendar-month. `budi stats` keeps calendar semantics if you want those.

### Shared status contract

`budi statusline --format json` emits the shared provider-scoped status contract (see [`docs/statusline-contract.md`](docs/statusline-contract.md)) that the Cursor extension and cloud dashboard reuse:

```bash
budi statusline --format json                      # auto-scoped to claude_code for --format claude
budi statusline --format json --provider cursor    # Cursor surface reuses the same shape
budi statusline --format json --provider codex     # any other supported provider
```

### Advanced variants

Power users can pick a different preset by creating `~/.config/budi/statusline.toml`:

```toml
# Default quiet preset (same as no file).
slots = ["1d", "7d", "30d"]

# `coach` — show the active session's cost and health:
#   budi · $1.24 session · session healthy
# preset = "coach"

# `full` — session cost + health + today's rolling 1d total:
# preset = "full"
```

Available slots: `1d`, `7d`, `30d`, `session`, `branch`, `project`, `provider`, `health`. The legacy `today` / `week` / `month` slot names still work — they resolve to the same rolling `1d` / `7d` / `30d` values so existing configs keep rendering.

## Cursor extension

Budi includes a Cursor/VS Code extension that shows Cursor-only spend in a single status bar item. It can be installed during `budi init` and later via `budi integrations install --with cursor-extension`.

As of v1.1.0 the extension is intentionally statusline-only (ADR-0088 §7, [#232](https://github.com/siropkin/budi/issues/232)) — no sidebar, no session list, no vitals/tips panel. The status bar renders the shared provider-scoped status contract filtered to `provider=cursor` and mirrors the Claude Code statusline byte-for-byte: `🟢 budi · $X 1d · $Y 7d · $Z 30d`. A leading dot glyph reports health (🟢 active, 🟡 reachable but quiet, 🔴 daemon unreachable, ⚪ first run / not installed yet). Click the status bar item to open the cloud dashboard — session list when a Cursor session is active, dashboard root otherwise — matching the Claude Code click-through.

The extension also works as a **first-run onboarding entry point**: if you discover budi through the VS Code Marketplace before installing the CLI, the extension shows a welcome view with a pre-filled install command and hands you off to `budi init` in an integrated terminal. The hand-off is tracked by local-only integer counters in `~/.local/share/budi/cursor-onboarding.json` (no remote telemetry, ADR-0083 privacy limits preserved) and `budi doctor` prints a one-line summary of those counters so install-funnel health is visible locally.

**Manual install** (if auto-install was skipped or you want to rebuild):

```bash
git clone https://github.com/siropkin/budi-cursor.git && cd budi-cursor
npm ci && npm run build
npx vsce package --no-dependencies -o cursor-budi.vsix
cursor --install-extension cursor-budi.vsix --force
```

Then reload Cursor: **Cmd+Shift+P** → **Developer: Reload Window**.

## Update

```bash
budi update                      # downloads latest release, migrates DB, restarts daemon
budi update --version 7.1.0     # update to a specific version
```

Works for all installation methods — automatically detects Homebrew and runs `brew upgrade` when appropriate. Update refreshes integrations you previously enabled (stored in `~/.config/budi/integrations.toml`). Agent enablement is stored separately in `~/.config/budi/agents.toml`.

## Integrations

Manage integrations anytime (especially if you skipped some during first init):

```bash
budi integrations list
budi integrations install --with cursor-extension
```

**Restart Claude Code and Cursor** after updating to pick up any changes.

## Cloud sync (optional)

Cloud sync is disabled by default. To enable it, sign up at [app.getbudi.dev](https://app.getbudi.dev), copy your API key from Settings, and create `~/.config/budi/cloud.toml`:

```toml
[cloud]
enabled = true
api_key = "budi_your_key_here"
```

The daemon picks up the config on next restart (`budi init`). Only pre-aggregated daily rollups are synced — prompts, code, and responses never leave your machine. See the [Privacy](#privacy) section for full details.

Environment variable overrides: `BUDI_CLOUD_ENABLED=true`, `BUDI_CLOUD_API_KEY=budi_...`, `BUDI_CLOUD_ENDPOINT=https://...`.

## CLI

**Launch and onboarding:**

```bash
budi init                          # start daemon + install autostart + show detected agents
budi init --cleanup                # review/remove managed 8.0/8.1 proxy residue
budi db import                     # one-time import of historical transcripts
```

**Monitoring and analytics:**

```bash
budi status                        # quick overview: daemon and today's cost
budi stats                         # usage summary with cost breakdown
budi stats --models                # model usage breakdown
budi stats --projects              # repos ranked by cost
budi stats --branches              # branches ranked by cost
budi stats --branch <name>         # cost for a specific branch
budi stats --tickets               # tickets ranked by cost (sourced from ticket_id tag)
budi stats --ticket <id>           # cost for a specific ticket, with per-branch breakdown
budi stats --activities            # activities ranked by cost (bugfix, refactor, …)
budi stats --activity <name>       # cost for a specific activity, with per-branch breakdown
budi stats --files                 # files ranked by cost (repo-relative paths only — ADR-0083)
budi stats --file <path>           # cost for a specific file, with per-branch + per-ticket breakdown
budi stats --provider codex        # filter stats to a single provider
budi stats --tag ticket_id         # cost per ticket value (raw tag view)
budi stats --tag ticket_prefix     # cost per team prefix
budi sessions                      # list recent sessions with cost and health
budi sessions --ticket <id>        # sessions tagged with a ticket id
budi sessions --activity <name>    # sessions tagged with an activity (bugfix, refactor, …)
budi sessions <id>                 # session detail: cost, models, health, tags, work outcome
budi vitals                        # session health vitals for most recent session
budi vitals --session <id>         # health vitals for a specific session
                                    # (the old `budi health` spelling still works in 8.2
                                    #  and prints a one-per-day deprecation hint)
```

**Diagnostics and maintenance:**

```bash
budi doctor                        # check health: daemon, tailer, schema, transcript visibility
budi doctor --deep                 # run full SQLite integrity_check (slower)
budi cloud status                  # cloud sync readiness + last-synced-at + queued records
budi cloud sync                    # push queued local rollups/sessions to the cloud now
budi autostart status              # check daemon autostart service
budi autostart install             # install the autostart service
budi autostart uninstall           # remove the autostart service
budi db import --force             # re-ingest all data from scratch (use after upgrades)
budi db repair                     # repair schema drift + run migration checks
budi db migrate                    # run database migration explicitly (usually automatic)
budi update                        # check for updates (auto-detects Homebrew)
budi update --version <name>       # update to a specific version
budi integrations list             # show what is installed vs available
budi integrations install ...      # install integrations later
budi uninstall                     # remove status line, config, and data
budi uninstall --keep-data         # uninstall but keep analytics database
```

All data commands support `--period today|week|month|all` and `--format json`.

## Windows: rolling vs calendar

Budi's `1d` / `7d` / `30d` labels show up on two surfaces that deliberately mean different things, so a developer clicking from the statusline into the cloud dashboard with the same window selected can legitimately see different totals:

- **Rolling** — `budi statusline` (and, by inheritance, the Cursor extension that renders the shared status contract) reports the last 24 hours, the last 7 days, and the last 30 days ending at "now". These are the primary live signals and never reset at midnight (ADR-0088 §4, [`docs/statusline-contract.md`](docs/statusline-contract.md)).
- **Calendar** — `budi stats --period today|week|month` and the cloud dashboard's cost charts use calendar-day ranges (`today` = today-so-far, `week` / `month` = the last 7 / 30 calendar days including today). These are the reporting views and reset with the local calendar.

Same chip label, different denominator: a fresh-morning `1d` rolling window still carries yesterday's afternoon spend; a `today` calendar window does not. Pick the surface that matches the question you are asking — "am I spending too fast right now?" is rolling, "what did this week cost?" is calendar.

## Tags & cost attribution

Assistant messages are tagged with core attribution keys: `provider`, `model`, `ticket_id`, `ticket_prefix`, `ticket_source`, `activity`, `activity_source`, `activity_confidence`, `file_path`, `file_path_source`, `file_path_confidence`, `tool_outcome`, `tool_outcome_source`, `tool_outcome_confidence`, `composer_mode`, `permission_mode`, `duration`, `tool`, `tool_use_id`, `user_email`, `platform`, `machine`, `user`, `git_user`.

Every first-class attribution dimension (ticket, activity, file, tool outcome) carries a sibling `*_source` tag that explains how the value was derived (`branch` / `branch_numeric` for tickets, `rule` for activity, `tool_arg` / `cwd_relative` for file paths, `jsonl_tool_result` / `heuristic_retry` for tool outcomes) and, where applicable, an `*_confidence` tag (`high` / `medium` / `low`). File-path attribution is strictly repo-relative: absolute paths, paths outside the repo root, `file://` and other URL schemes, and `..` escapes are dropped at extraction time so nothing outside the repo can land in the database (see [ADR-0083](docs/adr/0083-cloud-ingest-identity-and-privacy-contract.md)).

Conditional tags:
- `cost_confidence` is added when `cost_cents` is present.
- `speed` is added only for non-`standard` speed modes.

Identity tag semantics:
- `platform`: OS platform (`macos`, `linux`, `windows`)
- `machine`: host/machine name
- `user`: local OS username
- `git_user`: Git identity (`user.name`/`user.email` fallback)

`repo_id` and `git_branch` are stored as canonical message/session fields (not tags), so repo/branch analytics stay single-source and do not double-count.

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

## Session health

Budi monitors four vitals for every active session and turns them into plain-language tips.

The scoring is intentionally conservative:
- New sessions start **green** — the default is always positive. Vitals only turn yellow or red when there is clear evidence of a problem.
- It measures the current working stretch, so a `/compact` resets context-based checks.
- It looks at the active model stretch for cache reuse, so model switches do not poison the whole session.
- Cost acceleration uses per-user-turn costs when prompt boundaries are available, and falls back to per-reply costs otherwise.
- When `budi vitals` runs without `--session`, it picks the latest session by assistant activity first, then falls back to session timestamps.
- It prefers concrete next steps over internal jargon.

Tips are provider-aware: Claude Code suggestions mention `/compact` or `/clear`, Cursor suggestions point you toward a fresh composer session, and unknown providers receive neutral advice. Different providers may intentionally get different recommendations for the same health issue.

| Vital | What it detects | Yellow | Red |
|-------|----------------|--------|-----|
| **Context Growth** | Context size is growing enough to add noise | 3x+ growth with meaningful absolute growth | 6x+ growth with large absolute context size |
| **Cache Reuse** | Recent cache reuse is low for the active model stretch | Below 60% recent reuse | Below 35% recent reuse |
| **Cost Acceleration** | Later turns/replies cost much more than earlier ones | 2x+ growth and meaningful cost per unit | 4x+ growth and high cost per unit |
| **Retry Loops** | Agent is stuck in a failing tool loop (disabled since 8.0 — hook-event source removed; will be re-enabled on top of R1.5 tool-outcome signals) | One suspicious retry loop | Repeated or severe retry loops |

Health state appears in the statusline's opt-in `coach` / `full` presets (see above; the default statusline is quiet and cost-only) and on the session detail page in the cloud dashboard. Yellow means "pay attention soon"; red means "intervene now or start fresh." The Cursor extension is statusline-only as of v1.1.0 and does not render per-session health.

## Privacy

Budi is local-first. All data stays on your machine by default (`~/.local/share/budi/` on Unix, `%LOCALAPPDATA%\budi` on Windows). In 8.2, Budi reads the local transcripts/session files the agent already wrote to disk and stores derived analytics metadata locally: timestamps, token counts, model names, costs, and attribution tags. Prompts, code, and responses never leave the machine; cloud sync sends only aggregated rollups and session summaries.

**Cloud sync** (optional, disabled by default) pushes pre-aggregated daily rollups and session summaries to a team dashboard at `app.getbudi.dev`. Only numeric metrics cross the wire: token counts, costs, model names, hashed repo IDs, branch names, and ticket IDs. Prompts, code, responses, file paths, email addresses, raw payloads, and tag values are structurally excluded from the sync payload — there is no "full upload" mode.

Cloud sync details:
- **Transport**: HTTPS only — the daemon refuses to sync over plain HTTP
- **Direction**: Push-only — the cloud never initiates connections to your machine; there is no webhook, pull, or remote command channel
- **Granularity**: Daily aggregates; no per-message, per-hour, or real-time streaming views
- **Retention**: 90 days in the cloud alpha
- **Team model**: Manager/member roles for small teams (1–20 developers)
- **Auth**: API key auth for daemon sync; Supabase Auth (GitHub, Google, magic link) for the web dashboard. No SSO/SAML in v1

See [ADR-0083](docs/adr/0083-cloud-ingest-identity-and-privacy-contract.md) for the complete privacy contract.

## How it works

A lightweight Rust daemon (port 7878) manages a single SQLite database. The daemon watches each supported provider's local transcript/session roots, tails incremental appends through the shared pipeline, and writes canonical `messages` + tag rows. Cursor cost/token reconciliation still comes from the Usage API on a pull cadence; the CLI is a thin HTTP client and all queries go through the daemon.

## Details

<details>
<summary>How budi compares</summary>

| | budi | ccusage | Claude `/cost` |
|---|---|---|---|
| Multi-agent support | **Yes** (Claude Code, Codex CLI, Cursor, Copilot CLI) | Claude Code only | Claude Code only |
| Live local transcript tailing | **Yes** | No | No |
| Cost history | **Per-message + daily** | Per-session | Current session |
| Cloud dashboard | **Yes** ([app.getbudi.dev](https://app.getbudi.dev)) | No | No |
| Status line + session health | **Yes** (with actionable tips) | No | No |
| Per-repo breakdown | **Yes** | No | No |
| Cost attribution (branch/ticket) | **Yes** | No | No |
| Privacy | Local-first (optional cloud sync for aggregated metrics only) | Local | Built-in |
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
                         │  - analytics │    Pipeline       │
                         │  - tailer    │ ──────────────────┘
                         │  - import    │    Extract → Normalize
                         └──────────────┘      → Enrich → Load
                                ▲
                                │
       Claude/Codex/Copilot JSONL/session files  ─┐
       Cursor transcripts + Usage API pull        ├──▶ shared ingest path
       Historical import (`budi db import`)       ┘
```

The daemon is the single source of truth — the CLI never opens the database directly. The transcript tailer is the sole live data path in 8.2. Historical data from Claude Code JSONL transcripts, Codex Desktop/CLI sessions, Copilot CLI sessions, and Cursor Usage API can be imported via `budi db import` for one-time backfill.

**Data model** — nine tables, seven data entities + two supporting:

| Table | Role |
|-------|------|
| **messages** | Single cost entity — all token/cost data lives here (one row per API call) |
| **sessions** | Lifecycle context (start/end, duration, mode) without mixing cost concerns |
| **tags** | Flexible key-value pairs per message (repo, ticket, activity, user, etc.) |
| **sync_state** | Tracks incremental ingestion progress per file for progressive sync, plus cloud sync watermarks |
| **message_rollups_hourly** | Derived hourly aggregates (provider/model/repo/branch/role) for low-latency analytics reads |
| **message_rollups_daily** | Derived daily aggregates for summary/filter scans |

`messages` remains the source of truth; rollup tables are derived caches maintained incrementally during ingest/update/delete.

</details>

<details>
<summary>Privacy & Retention</summary>

budi is local-first, but you can now enforce tighter storage controls for raw payloads and session metadata.

**Privacy mode (`BUDI_PRIVACY_MODE`):**

| Value | Behavior |
|------|----------|
| `full` (default) | Store raw values as-is |
| `hash` | Hash sensitive fields (for example `user_email`, `cwd`, and workspace paths) before storage |
| `omit` | Do not store sensitive raw/session fields |

**Retention controls:**

| Env var | Default | Scope |
|--------|---------|-------|
| `BUDI_RETENTION_RAW_DAYS` | `30` | `sessions.raw_json` |
| `BUDI_RETENTION_SESSION_METADATA_DAYS` | `90` | `sessions.user_email`, `sessions.workspace_root` |

Use `off` to disable a retention window for a category.

Retention cleanup runs automatically after sync and queued realtime ingestion processing.

**At-rest protection (SQLCipher strategy):**
- Current default uses bundled SQLite (WAL) for broad compatibility and easy installs.
- If you need encrypted-at-rest local DBs (shared/managed machines), use one of these strategies:
  - build budi against SQLCipher-enabled SQLite (`libsqlite3-sys` SQLCipher build),
  - or place the budi data directory on an encrypted volume (FileVault, LUKS, BitLocker).
- SQLCipher integration is feasible but has tradeoffs (key management UX, packaging complexity, migration path from existing plaintext DBs), so default remains plain SQLite for now.

</details>

<details>
<summary>Hooks (removed in 8.0)</summary>

Hook-based ingestion (`budi hook`) and the `hook_events` table have been removed. In 8.2, the transcript tailer is the sole live data source.

</details>

<details>
<summary>Cost confidence levels</summary>

Every message carries a `cost_confidence` tag that indicates how the cost was derived:

| Level | Source | Accuracy |
|-------|--------|----------|
| `proxy_estimated` | Retained 8.1 proxy-era rows (historical only) | Estimated from response body / SSE stream |
| `exact` | Cursor Usage API / Claude Code JSONL tokens | Exact tokens, calculated cost |
| `estimated` | JSONL tokens x model pricing | ~92-96% accurate (missing thinking tokens) |
| `estimated_unknown_model` | JSONL tokens × **unknown** model (8.3+) | `cost_cents = 0` — model id not in pricing manifest; backfilled automatically when upstream catches up ([ADR-0091](docs/adr/0091-model-pricing-manifest-source-of-truth.md)) |

Messages with `exact` confidence show exact cost in the dashboard. Estimated costs are prefixed with `~`.

In 8.3+ pricing is sourced from the community-maintained [LiteLLM pricing manifest](https://github.com/BerriAI/litellm/blob/main/model_prices_and_context_window.json) via a three-layer lookup (on-disk cache → embedded baseline → hard-fail to `unknown`), refreshed daily by the daemon (opt-out: `BUDI_PRICING_REFRESH=0`), with every row tagged `pricing_source` so history is auditable and immutable. See [ADR-0091](docs/adr/0091-model-pricing-manifest-source-of-truth.md) for the full contract and `budi pricing status` for the operator surface.

</details>

<details>
<summary>OpenTelemetry (removed in 8.0)</summary>

OTEL ingestion endpoints (`POST /v1/logs`, `POST /v1/metrics`) and the `otel_events` table have been removed. As of 8.2.0 live cost capture is handled by the JSONL tailer (per-provider `Provider::watch_roots()`, [ADR-0089](docs/adr/0089-reverse-proxy-first-jsonl-tailing-as-sole-live-path.md)); 8.1.x used the proxy on port 9878 in this slot, which is removed in 8.2 R2.1 (#322).

</details>

<details>
<summary>Daemon API</summary>

The daemon runs on `http://127.0.0.1:7878` and exposes a REST API.
Privileged routes are loopback-only (`127.0.0.1` / `::1`): all `/admin/*` endpoints plus `POST /sync`, `POST /sync/all`, and `POST /sync/reset`.

**System:**

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/health` | Health check |
| POST | `/sync` | Sync recent data (last 30 days, loopback-only) |
| POST | `/sync/all` | Load full transcript history (loopback-only) |
| POST | `/sync/reset` | Wipe sync state + full re-sync (loopback-only) |
| GET | `/sync/status` | Syncing flag + last_synced |
| GET | `/health/integrations` | Statusline/extension status + DB stats |
| GET | `/health/check-update` | Check for updates via GitHub |

**Analytics:**

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/analytics/summary` | Cost and token totals |
| GET | `/analytics/filter-options` | Filter values for providers/models/projects/branches |
| GET | `/analytics/messages` | Message list (paginated, searchable) |
| GET | `/analytics/messages/{message_uuid}/detail` | Full detail for a specific message |
| GET | `/analytics/projects` | Repos ranked by usage |
| GET | `/analytics/branches` | Cost per git branch |
| GET | `/analytics/branches/{branch}` | Cost for a specific branch |
| GET | `/analytics/tickets` | Cost per ticket (first-class dimension, #304) |
| GET | `/analytics/tickets/{ticket_id}` | Cost for a specific ticket, with per-branch breakdown |
| GET | `/analytics/activities` | Cost per activity bucket (bugfix, refactor, …) (#305) |
| GET | `/analytics/activities/{name}` | Cost for a specific activity, with per-branch breakdown |
| GET | `/analytics/files` | Cost per repo-relative file (#292) |
| GET | `/analytics/files/{*path}` | Cost for a specific file, with per-branch + per-ticket breakdown |
| GET | `/analytics/cost` | Cost breakdown |
| GET | `/analytics/models` | Model usage breakdown |
| GET | `/analytics/providers` | Per-provider breakdown |
| GET | `/analytics/activity` | Token activity over time |
| GET | `/analytics/tags` | Cost breakdown by tag |
| GET | `/analytics/statusline` | Status line data |
| GET | `/analytics/cache-efficiency` | Cache hit rates and savings |
| GET | `/analytics/session-cost-curve` | Cost per message by session length |
| GET | `/analytics/cost-confidence` | Breakdown by cost confidence level |
| GET | `/analytics/subagent-cost` | Subagent vs main agent cost |
| GET | `/analytics/sessions` | Session list (paginated, searchable) |
| GET | `/analytics/sessions/{id}` | Session metadata and aggregate stats |
| GET | `/analytics/sessions/{id}/messages` | Messages for a specific session |
| GET | `/analytics/sessions/{id}/curve` | Session input token growth curve |
| GET | `/analytics/sessions/{id}/tags` | Tags for a specific session |
| GET | `/analytics/session-health` | Session health vitals and tips |
| GET | `/analytics/session-audit` | Session attribution/linking audit stats |
| GET | `/admin/providers` | Registered providers (loopback-only) |
| GET | `/admin/schema` | Database schema version (loopback-only) |
| POST | `/admin/migrate` | Run database migration (loopback-only) |
| POST | `/admin/repair` | Repair schema drift + run migration (loopback-only) |
| POST | `/admin/integrations/install` | Install/update integrations from daemon (loopback-only) |
| GET | `/pricing/status` | Pricing manifest snapshot ([ADR-0091](docs/adr/0091-model-pricing-manifest-source-of-truth.md)) |
| POST | `/pricing/refresh` | Trigger an immediate LiteLLM manifest refresh (loopback-only) |

Most endpoints accept `?since=<ISO>&until=<ISO>` for date filtering.

</details>

## Troubleshooting

**No data after setup:**
1. Run `budi status` to check daemon health and today's cost
2. Run `budi doctor` to verify transcript visibility and any leftover 8.0/8.1 proxy residue
3. If `budi doctor` warns about legacy proxy residue, run `budi init --cleanup` and follow the consent flow
4. Send a prompt and check `budi stats` for non-zero usage
5. For historical data: `budi db import` (one-time backfill from Claude Code JSONL, Codex sessions, Copilot CLI sessions, Cursor Usage API)

**Daemon won't start:**
1. Check if port 7878 is in use: `lsof -i :7878`
2. Kill stale processes: `pkill -f "budi-daemon serve"`
3. Restart: `budi init`

Windows equivalent:
1. Check listeners: `Get-NetTCPConnection -LocalPort 7878 -State Listen`
2. Kill stale daemon: `taskkill /IM budi-daemon.exe /F`
3. Restart: `budi init`

**Daemon doesn't survive reboots:**
Run `budi autostart status` — if it shows "not installed", run `budi autostart install` to install the platform-native service (launchd on macOS, systemd on Linux, Task Scheduler on Windows). `budi init` also installs the autostart service.

**Legacy proxy residue warning after upgrade:**
1. Run `budi doctor` to see which shell/editor files still contain managed 8.0/8.1 proxy blocks
2. Run `budi init --cleanup` to preview and remove those managed blocks with explicit consent
3. Open a fresh terminal after cleanup if the current shell still has old proxy env vars loaded

**Status line not showing:**
1. Restart Claude Code after `budi init`
2. Check: `budi statusline` should output cost data

**Cursor extension status bar shows offline (red dot) or stays quiet (yellow):**
1. Run `budi doctor` to verify daemon health and Cursor transcript visibility.
2. In Cursor, run **Budi: Refresh Status**.
3. If needed, reload Cursor window (`Developer: Reload Window`) after `budi init` or daemon URL changes.
4. Open a Cursor chat/composer once so Cursor creates its local session artifacts, then send a prompt and recheck `budi status`.

## Uninstall

```bash
budi uninstall          # stops daemon, removes autostart service, status line, config, and data
```

`budi uninstall` removes the autostart service, status line, config, and data but **not** the binaries themselves. Remove binaries separately:

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

`budi init` returns 0 on success, 2 on partial success (init completed with warnings), 1 on hard error.

## License

[MIT](LICENSE)
