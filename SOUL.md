# SOUL.md

Local-first cost analytics for AI coding agents (Claude Code, Codex CLI, Cursor, Copilot CLI). Tracks tokens, costs, and usage per message by tailing the JSONL transcript files those agents already write to disk (see [ADR-0089](docs/adr/0089-reverse-proxy-first-jsonl-tailing-as-sole-live-path.md)). Historical data from the same transcripts and the Cursor Usage API can be backfilled via `budi db import`. Optional cloud sync (disabled by default) pushes pre-aggregated daily rollups to a team dashboard — prompts, code, and responses never leave the machine (see [ADR-0083](docs/adr/0083-cloud-ingest-identity-and-privacy-contract.md)).

Architecture highlights:

- **JSONL tailing is the sole live ingestion path** ([ADR-0089](docs/adr/0089-reverse-proxy-first-jsonl-tailing-as-sole-live-path.md)). No proxy, no hooks, no OTEL. The 8.0/8.1 proxy was removed in 8.2; legacy `proxy_estimated` and `otel_exact` rows remain read-only in the DB for historical analytics.
- **Model pricing flows through a single manifest-backed `pricing::lookup`** ([ADR-0091](docs/adr/0091-model-pricing-manifest-source-of-truth.md)): embedded LiteLLM baseline at build time, daily refresh against the upstream manifest (opt out with `BUDI_PRICING_REFRESH=0`), every row tagged `pricing_source` so history is immutable.
- **`budi init` never mutates user config on the live path**: it creates the data dir, starts the daemon, installs autostart, and wires the recommended integrations (Claude Code statusline, Cursor extension) idempotently. `--no-integrations` opts out. `--cleanup` is a separate consent-first path for reviewing/removing managed 8.0/8.1 proxy residue.

## Build & Test

```bash
cargo build              # dev build
cargo build --release    # release build
cargo test               # all workspace tests
cargo test -p budi-core  # core tests only
./scripts/install.sh     # build release + install to ~/.local/bin/
```

If install scripts are blocked (for example by anti-virus), use Cargo-bin fallback:

```bash
cargo install --path crates/budi-cli --bin budi --force --locked
cargo install --path crates/budi-daemon --bin budi-daemon --force --locked
budi --version
budi init
```

**Important**: Install **`budi` and `budi-daemon` from the same build** and keep **only one copy on PATH** (do not mix Homebrew with `~/.local/bin` or another prefix). Version mismatch breaks daemon restarts; `budi init` warns if multiple binaries are found.

After upgrading: the first CLI command now verifies daemon version and auto-restarts stale daemons when needed. If automatic restart fails, stop the old process manually, then run `budi init`. On Unix you can use `pkill -f budi-daemon`; on Windows use `taskkill /IM budi-daemon.exe /F` if needed.

### Local end-to-end tests

Shell-driven end-to-end tests live under `scripts/e2e/`. They exercise the full stack — real release binaries (`budi` + `budi-daemon`), the transcript tailer, the CLI, and upgrade/compatibility edges — against an isolated `$HOME` so they never touch real user data.

```bash
cargo build --release                                 # once per change
bash scripts/e2e/test_<issue>_<slug>.sh               # run an individual regression guard
ls scripts/e2e/                                       # see the full set of guards
```

Each guard pins a specific bug or contract; see `scripts/e2e/README.md` for the index.

Each script is a single self-contained bash file that:

1. Builds a throwaway `HOME` in `mktemp` and exports it for the whole run.
2. Boots a tiny Python mock upstream on loopback.
3. Starts `budi-daemon serve ...` inside that throwaway home; some upgrade-compatibility scripts also point `BUDI_ANTHROPIC_UPSTREAM` / `BUDI_OPENAI_UPSTREAM` at a mock so legacy proxy-era state can be exercised without touching real upstreams.
4. Drives real CLI/HTTP commands and asserts DB rows, API responses, and CLI output.
5. Tears down the temp HOME and child processes via a `trap`.

Design rules:

- **No shared mutable state.** Every script allocates its own ports and `HOME`; runs should be safe in parallel.
- **Fail loud, fail fast.** Scripts use `set -euo pipefail` and print the daemon log on any failure.
- **Negative-path provable.** Each regression test should fail when the fix it guards is reverted (every new script should be verified this way before merging).
- **Keep the fixtures minimal.** Mock upstream responses stay inline in the script; no binary fixtures checked in.

When adding a new script, name it `test_<issue>_<short_slug>.sh` and document what bug or contract it pins in the opening comment. See `scripts/e2e/README.md` for the full convention.

## Daemon autostart

`budi init` installs a platform-native user-level service so the daemon starts automatically at login and restarts on crash:

| Platform | Mechanism | Service file |
|----------|-----------|-------------|
| macOS | launchd LaunchAgent | `~/Library/LaunchAgents/dev.getbudi.budi-daemon.plist` |
| Linux | systemd user service | `~/.config/systemd/user/budi-daemon.service` |
| Windows | Task Scheduler | `BudiDaemon` task (created via `schtasks`) |

`budi autostart status` checks service state, `budi autostart install` installs the service, `budi autostart uninstall` removes it. `budi uninstall` also removes the service. `budi doctor` reports service installation status. See ADR-0087 §8 for design rationale.

## Platforms

macOS and Linux use the Unix daemon startup path (`lsof`, `ps`, `kill`) to replace an existing listener on the same port. Windows uses PowerShell **`Get-NetTCPConnection`** and **`taskkill`** for the same behavior. Unsupported or minimal environments may skip automatic takeover - stop the old daemon manually if the new one cannot bind.

## Architecture

### Product boundaries

Three independent repos (extraction completed per [ADR-0086](docs/adr/0086-extraction-boundaries.md)). 8.x as a whole is organized as one local product with optional cloud visibility, not two peer products — see [ADR-0088](docs/adr/0088-8x-local-developer-first-product-contract.md) for the local-developer-first product contract governing 8.1 scope, classification principles, statusline contract, and 8.2 / 9.0 deferrals.

| Product | Repo | Role |
|---------|------|------|
| **budi-core** | [`siropkin/budi`](https://github.com/siropkin/budi) | Rust workspace: daemon, CLI, core business logic |
| **budi-cursor** | [`siropkin/budi-cursor`](https://github.com/siropkin/budi-cursor) | VS Code/Cursor extension. Communicates with daemon over HTTP and `budi` CLI |
| **budi-cloud** | [`siropkin/budi-cloud`](https://github.com/siropkin/budi-cloud) | Cloud dashboard + ingest API (Next.js + Supabase) |

### Crates

- **budi-core** — Business logic: analytics (SQLite queries), providers (Claude Code, Codex, Copilot CLI, Cursor) including each provider's `watch_roots()` for live tailing, pipeline (enrichment), cost calculation, config, migrations, autostart (platform-native daemon service management). Historical proxy/hook/OTEL rows remain read-only in `messages` for analytics continuity; the ingestion paths are gone.
- **budi-cli** — Thin HTTP client to the daemon. Commands: `init`, `stats`, `sessions`, `status`, `statusline`, `doctor`, `vitals`, `update`, `integrations`, `autostart`, `uninstall`, `cloud`, `pricing`, and the DB admin namespace `db` (`db migrate`, `db repair`, `db import`). `budi health` is a hidden deprecation alias for `budi vitals` that prints a one-per-day hint. The bare `budi migrate` / `budi repair` / `budi import` verbs were removed in 8.3.0 (#428) after shipping as hidden aliases during the 8.2.x window — use `budi db <verb>`.
- **budi-daemon** — axum HTTP server (port 7878). Owns SQLite exclusively. Serves the analytics API and runs the filesystem tailer that watches each `Provider::watch_roots()` directory, parses incremental JSONL appends through `Pipeline::default_pipeline()`, and writes to the canonical `messages` / tag tables. One-shot historical backfill is user-initiated via `budi db import` and runs the same pipeline.

### Data flow

```
Live data (ADR-0089):
Provider watcher (notify FS events on Provider::watch_roots() dirs)
  -> Per-file offset tracked in tail_offsets
  -> Provider::parse_file(path, content, offset) -> incremental ParsedMessage batch
  -> Pipeline: IdentityEnricher -> GitEnricher -> ToolEnricher -> FileEnricher -> CostEnricher -> TagEnricher
  -> SQLite (messages + tags + derived rollup tables)
  -> Dashboard / CLI stats / Statusline

Historical import (budi db import — same Provider trait, one-shot mode):
Sources (Claude Code JSONL, Codex sessions, Copilot CLI sessions, Cursor Usage API)
  -> Provider::discover_files() -> ParsedMessage structs (full backfill)
  -> Same Pipeline as the live tailer (single code path)
  -> SQLite (messages + tags + derived rollup tables)
```

Enricher order is critical — each depends on prior enrichers. Do not reorder. The live tailer and `budi db import` run the **same** pipeline against the **same** transcript files, so every classification feature (ticket extraction, file-level attribution, activity classification, tool outcomes) lands for both paths automatically.

```
Cloud sync (optional, disabled by default):
Local SQLite daily rollups
  -> Daemon background sync worker reads aggregates only
  -> Builds sync envelope (ADR-0083 §2): daily_rollups + session_summaries
  -> HTTPS-only POST to app.getbudi.dev/v1/ingest (Bearer budi_<key>)
  -> Watermark tracking: only sends new/updated rollups since last confirmed sync
  -> Retry with exponential backoff (1s -> 2s -> ... -> 5min cap) on 429/5xx
  -> Auth failure (401) stops syncing; schema mismatch (422) pauses until update
  -> Supabase Postgres (UPSERT, idempotent)
  -> Manager dashboard at app.getbudi.dev
Config: ~/.config/budi/cloud.toml ([cloud] section), env overrides BUDI_CLOUD_*
Never uploaded: prompts, responses, code, file paths, email, raw payloads, tag values

Manual cloud sync:
`budi cloud sync`     -> POST /cloud/sync (loopback-only) -> same sync_tick as worker
`budi cloud status`   -> GET /cloud/status -> readiness + watermarks, no network call
AppState.cloud_syncing AtomicBool guards worker and manual path from double-posting.

Onboarding helper:
`budi cloud init`                        -> write ~/.config/budi/cloud.toml from commented template
`budi cloud init --api-key KEY`          -> one-shot: write key + `enabled = true`
`budi cloud init --force [--yes]`        -> overwrite existing config (--yes skips confirm)
Status renderer distinguishes disabled (no config) / disabled (stub key) /
enabled-but-missing-api-key / enabled-but-not-fully-configured / ready.
CloudSyncStatus carries `config_exists` + `api_key_stub` so the daemon's
`GET /cloud/status` envelope drives the three-way UX without a separate
filesystem poke on every render.
```

### Database (SQLite, WAL mode, schema v1)

Core tables:
- **messages** - Single cost entity. One row per API call. All token/cost data lives here. Fields: id, session_id, role, model, provider, timestamp, input/output/cache tokens, cost_cents, cost_confidence, pricing_source (8.3+, [ADR-0091](docs/adr/0091-model-pricing-manifest-source-of-truth.md); one of `manifest:vNNN` / `backfilled:vNNN` / `embedded:vBUILD` / `legacy:pre-manifest` / `unknown` / `upstream:api` (Cursor Usage API rows) / `unpriced:no_tokens` (zero-token rows — user messages, tool results; 8.3.4+)), git_branch, repo_id, cwd, request_id
- **sessions** - Lifecycle context (start/end, duration, mode, title) without mixing cost concerns. One row per conversation. Primary key field: id
- **tags** - Flexible key-value pairs per message (repo, ticket_id, activity, user, etc.) using message_id FK to messages(id)
- **sync_state** - Tracks incremental ingestion progress per file for progressive sync. Also stores cloud sync watermarks (`__budi_cloud_sync__` keys) for idempotent cloud uploads
- **message_rollups_hourly** - Derived hourly aggregates (provider/model/repo/branch/role dimensions) for low-latency analytics reads
- **message_rollups_daily** - Derived daily aggregates for coarse-grained summaries and filter option scans

### Cost sources

| Source | Confidence | What it provides |
|--------|-----------|-----------------|
| **JSONL tailer** (Claude Code, Codex, Copilot CLI) | `estimated` | Per-message tokens parsed from the agent's local transcript as it grows. Same parser as `budi db import`; same enricher chain. |
| **Cursor Usage API** | `exact` | Per-request tokens + totalCents pulled from Cursor's API. Reconciles cost/token data the JSONL doesn't carry; scheduled pull, not a live path. Lag profile p50 ≈ 70 s, p99 ≈ 6 min (measured in #321, embedded in [ADR-0089](docs/adr/0089-reverse-proxy-first-jsonl-tailing-as-sole-live-path.md) §7). |
| **JSONL backfill** (`budi db import`) | `estimated` (Claude Code / Codex / Copilot CLI) / `exact` (Cursor) | Same providers, one-shot mode for historical backfill. Used after install or after `budi db import --force`. |
| **Legacy proxy** (pre-8.2 history only) | `proxy_estimated` | Rows written by the 8.0/8.1 proxy remain queryable. No new writes; the proxy runtime was deleted in 8.2. |

Historical OTEL data (`otel_exact` confidence) remains queryable but OTEL ingestion has been removed.

### Attribution contract

Every ingestor that writes to `messages` MUST uphold the following so that the
CLI, daemon, and dashboard tell the same story (see ADR-0082 and
[ADR-0088](docs/adr/0088-8x-local-developer-first-product-contract.md) §5):

- **`timestamp`** — RFC3339 string in UTC. Accept both `...Z` and `...+00:00`
  offsets; `session_list_with_filters` and `activity_chart` compare these as
  strings, so never write naive SQLite datetime (`YYYY-MM-DD HH:MM:SS`) or a
  local-offset string. Providers emit RFC3339 from `DateTime::<Utc>::to_rfc3339()`
  (Claude Code JSONL, Codex) or `DateTime::from_timestamp_millis(..).to_rfc3339()`
  (Cursor).
- **`session_id`** — required for every live assistant row. The JSONL tailer
  reads `sessionId` (or the per-agent equivalent) directly from each
  transcript line — Claude Code, Codex, Cursor, and Copilot all write it
  natively, so there is no header contract and no daemon-side ID synthesis.
  Empty-string `session_id` is treated as NULL by the analytics layer, and
  the insert path normalizes `""` to `NULL` so ghost `(empty)` sessions
  cannot appear. Rows with NULL/empty `session_id` are invisible to
  `budi sessions` by design — they indicate an attribution bug upstream.
- **`provider`** — canonical provider key (`claude_code`, `cursor`, `openai`,
  `copilot`). `COALESCE(provider, 'claude_code')` is the legacy fallback for
  pre-8.0 rows; new writes MUST set it explicitly.
- **`git_branch`** — written without the `refs/heads/` prefix
  (`session_list_with_filters` strips it defensively for older rows). The
  `GitEnricher` resolves the branch directly from the per-line `gitBranch`
  field that Claude Code, Codex, and Cursor already write into every
  transcript message. Resolution priority:
  1. **Per-line `gitBranch` from the transcript** — what the agent itself
     recorded for the message. The common case.
  2. **Session-level propagation in `propagate_session_context`** — if a
     transcript line lacks `gitBranch` but a sibling message in the same
     session has one, the pipeline adopts it; later messages backfill
     earlier NULL-branch rows in the same session.
  3. **`Unassigned` repo + empty branch** — last-resort fallback. Rows in
     this state fold into the `(untagged)` DB bucket and render as
     `(no branch)` in `budi stats branches` (#450).

  A detached HEAD (`gitBranch == "HEAD"`) is normalized to empty so that
  worktrees, mid-rebase sessions, and CI runs do not pollute the branches
  list with a bogus `HEAD` bucket.

- **`ticket_id`** — first-class CLI dimension. The
  `pipeline::extract_ticket_from_branch` extractor is the single source of
  truth: it (1) filters integration branches (`main`, `master`, `develop`,
  `HEAD`), (2) prefers the canonical alphanumeric pattern (e.g. `ENG-123`,
  `PAVA-2120` anywhere in the branch), then (3) falls back to a
  numeric-only id for branches like `feature/1234` or `42-quick-fix`. Runs
  inside the `GitEnricher` for both the live tailer and `budi db import`
  (one code path, [ADR-0089](docs/adr/0089-reverse-proxy-first-jsonl-tailing-as-sole-live-path.md) §1).
  Every emitted `ticket_id` tag is paired with:
    - `ticket_prefix` — alphabetic prefix (`ENG`, `PAVA`), or empty for
      numeric-only ids; and
    - `ticket_source` — explains how the id was derived: `branch` for the
      alphanumeric pattern, `branch_numeric` for the numeric fallback.
      Reserved for future `header` / `hint` sources from a smarter client
      shim. Mirrors the `activity_source` contract so every first-class
      attribution dimension carries its own provenance.

  Messages without a recognised ticket emit no `ticket_id` tag (no legacy
  `Unassigned` sentinel); they fold into the `(untagged)` DB bucket,
  keeping bucket semantics consistent across branch / ticket /
  activity views. The CLI renders the bucket with per-view copy (#450):
  `(no ticket)` for tickets, `(no branch)` for branches, `(unclassified)`
  for activities, `(no file tag)` for files, `(model pending)` for
  models (suppressed from default output behind `--include-pending`).

  Surfaces:
  - `budi stats tickets` — list ranked by cost, with a `(no ticket)`
    bucket and a `src=…` column showing the dominant `ticket_source`.
  - `budi stats ticket <ID>` — detail view with per-branch breakdown
    and a `Source` row. Legacy rows without a `ticket_source` sibling
    tag default to `branch` (the legacy pipeline producer) so older
    DBs stay readable without a reindex.
  - `budi sessions --ticket <ID>` — sessions tagged with the ticket.
  - `GET /analytics/tickets` and `/analytics/tickets/{ticket_id}` mirror
    `/analytics/branches{/branch}` so future cloud/dashboard work can adopt
    the same data contract.

- **`activity`** — first-class CLI dimension. The pipeline emits an
  `activity` tag for every assistant message whose session has a
  classified prompt category (bugfix, refactor, testing, feature, review,
  ops, question, writing, **docs**). Values come from the rule-based
  `hooks::classify_prompt_detailed` and are propagated across the session
  by `propagate_session_context`, so every assistant message in a
  classified session carries exactly one `activity` tag. Companion tags:
  `activity_source` (`rule` when derived from the rule-based classifier;
  reserved for future `header` / `hint` sources) and `activity_confidence`
  (`high` when anchored by a leading action phrase with a strong keyword
  hit, `medium` for a clear single keyword hit, `low` on weak / fallback
  matches). Precedence: a leading question-anchor phrase ("explain",
  "what is", "how do I") wins over generic `bugfix` keywords unless the
  prompt also starts with a bugfix action ("fix the error"). Coverage
  extends beyond Claude Code JSONL ingestion:
    - **Cursor JSONL ingestion** — user prompts are classified at parse
      time in `providers::cursor::parse_cursor_line`.
    - **Codex / Copilot JSONL ingestion** — the same
      `hooks::classify_prompt_detailed` runs in the pipeline once the
      per-provider parser surfaces the user turn.

  Analytics recompute the dominant `activity_source` /
  `activity_confidence` per activity from the stored tags (most frequent
  value wins, ties broken alphabetically), falling back to `rule` /
  `medium` only when an activity has no companion tags yet (legacy data).
  Surfaces:
  - `budi stats activities` — list ranked by cost, with an
    `(unclassified)` bucket (#450) for messages that never matched a
    classification rule (short prompts, slash commands, metadata-only
    messages).
  - `budi stats activity <NAME>` — detail view with per-branch
    breakdown, plus `source` and `confidence` labels.
  - `budi sessions --activity <NAME>` — sessions tagged with the
    activity, mirroring `--ticket`.
  - `GET /analytics/activities` and `/analytics/activities/{name}`
    mirror the ticket endpoints so future cloud/dashboard work can adopt
    the same data contract.

- **`file_path`** — per-file attribution. When an assistant
  message uses a file-aware tool (Claude Code's `Read` / `Write` / `Edit` /
  `MultiEdit` / `NotebookEdit` / `Grep` / `Glob`, Cursor's `edit_file` /
  `read_file` / `write_file` / `search_replace` / `delete_file` / …) the
  pipeline extracts raw candidate paths from the tool-call arguments and
  runs them through `file_attribution::attribute_files`, which:
    1. Rejects URL schemes other than `file://`.
    2. Normalizes Windows separators to forward slashes.
    3. Strips absolute paths against the message's `cwd` / resolved repo
       root. Anything that cannot be proven to sit inside the repo root
       is **dropped** — we never record outside-of-repo paths, mtimes,
       sizes, or file contents. See
       [ADR-0083](docs/adr/0083-cloud-ingest-identity-and-privacy-contract.md).
    4. Collapses `.` / `..` segments; traversals that would escape the
       repo are dropped.
    5. Caps per-message tag fan-out at
       `file_attribution::MAX_FILES_PER_MESSAGE` (16) to keep payloads
       small on pathological Grep/Glob output.

  Every accepted path is emitted as a `file_path` tag (multi-valued);
  a sibling `file_path_source` (`tool_arg` when the path came directly
  from a known argument, `cwd_relative` when it was normalized from an
  absolute path against the message cwd) and `file_path_confidence`
  (`high` / `medium`) are written once per message so provenance is
  queryable the same way as `ticket_source` / `activity_source`.

  Surfaces:
  - `budi stats files` — files ranked by cost, with a `(no file tag)`
    bucket (#450) and a `src=…` column showing the dominant source.
    Long paths are middle-ellipsis truncated (configurable via
    `--label-width N`); full paths stay available via `budi stats file <PATH>`
    and `--format json`.
  - `budi stats file <PATH>` — detail view with per-branch **and**
    per-ticket breakdowns, so you can see which tickets charged cost
    to a particular file.
  - `GET /analytics/files` and `/analytics/files/{*path}` mirror the
    ticket / activity endpoints; the path segment is validated to be
    repo-relative (no leading `/`, no `..`, no Windows separators, no
    URL scheme) before hitting SQLite.

- **Breakdown envelope** (shared). Every list endpoint (`GET /analytics/projects`,
  `/analytics/branches`, `/analytics/tickets`, `/analytics/activities`,
  `/analytics/files`, `/analytics/models`, `/analytics/tags`) returns
  a [`BreakdownPage<T>`](crates/budi-core/src/analytics/queries.rs)
  envelope rather than a bare JSON array. The envelope exposes
  `rows`, `other?` (truncation-tail aggregate), `total_cost_cents`
  (grand total across every matching row, to the cent),
  `total_rows`, `shown_rows`, and the effective `limit`. The contract
  `sum(rows.cost_cents) + other.cost_cents == total_cost_cents` is
  exercised by the reconciliation suite in `analytics/tests.rs`. The CLI
  surfaces this as a `Total` footer plus an optional `(other)` row and
  honours `--limit N` (default 30, `0` = unlimited) across every
  breakdown view.

- **`tool_outcome`** — per-message tool-call outcome. The JSONL
  extractor reads `tool_result` blocks from user messages, keeps only
  the `tool_use_id` and a bounded classification (`success`, `error`,
  `denied`), and never persists the underlying content. The pipeline
  joins these back to the originating assistant message on the next
  pass and emits one `tool_outcome` tag per distinct outcome observed.
  A session-scoped retry heuristic promotes a follow-up call to the
  same tool after an `error` into `tool_outcome=retry`, so recovery
  attempts surface without requiring provider cooperation. Sibling
  `tool_outcome_source` (`jsonl_tool_result` when direct,
  `heuristic_retry` when promoted) and `tool_outcome_confidence`
  (`high` / `medium`) mirror the `activity_source` / `file_path_source`
  contract. Messages with no tool uses carry no outcome tag. Scope: the
  JSONL extractor only walks the array-of-blocks (`UserContent::Blocks`)
  encoding Claude Code has used since inception. Plain-string user
  messages (`UserContent::Text`) never carry structured tool results
  and are deliberately not string-probed for `"type":"tool_result"`
  substrings — any future provider with a different tool-result shape
  should land a dedicated extractor keyed on the `provider` label
  rather than silently widening this one.

- **`work_outcome`** (session-scoped) — derived from local git state
  only. `budi session detail <id>` correlates the session's
  `git_branch` with commits on that branch between the session's
  start and its end + 24h grace, producing one of `committed`,
  `branch_merged`, `no_commit`, or `unknown`. The derivation runs
  `git` locally — no remote Git/PR API calls, no content capture —
  and fails open to `unknown` whenever the branch is missing, is an
  integration branch, or the repo root can't be resolved. The
  integration-branch set (`main`, `master`, `develop`, `HEAD`) is
  shared with the pipeline ticket extractor via
  `budi_core::pipeline::is_integration_branch` so the two derivations
  can't disagree about what counts as a non-feature branch; the
  literal `HEAD` sentinel is rejected here as well so a detached-HEAD
  session can't be falsely credited as `branch_merged` via the
  merge-base fallback. A one-line rationale accompanies every label
  so operators can see which rule fired. List surfaces skip the
  derivation (one `git` invocation per session list row is too
  expensive); only the detail view surfaces it.

### Statusline contract

The JSON shape emitted by `GET /analytics/statusline` and
`budi statusline --format json` is the single shared provider-scoped
status contract. It is consumed by the CLI statusline, the Cursor
extension, and the cloud dashboard. Provider is an explicit filter
rather than a family of per-surface shapes, so new agent coverage
slots into the same shape. See
[`docs/statusline-contract.md`](docs/statusline-contract.md) for the
full schema.

Key points:

- **Rolling `1d` / `7d` / `30d` windows** (`cost_1d`, `cost_7d`,
  `cost_30d`) — not calendar today/week/month. The statusline surface
  is the canonical rolling view. `budi stats --period today` still
  uses the local-calendar today (today-so-far), but `--period week`
  and `--period month` resolve to rolling 7 / 30 days ending now —
  identical to `-p 7d` / `-p 30d`.
- **Provider-scoping is strict.** When the request carries
  `provider=claude_code`, every numeric field (`cost_*`, `session_cost`,
  `branch_cost`, `project_cost`) and `active_provider` are filtered to
  that provider, so the Claude Code statusline never shows blended
  multi-provider totals.
- **Deprecated aliases** `today_cost` / `week_cost` / `month_cost` are
  still populated with the same rolling values for backward
  compatibility and will be removed in 9.0. New consumers read
  `cost_1d` / `cost_7d` / `cost_30d`.
- **Slot config aliases.** `~/.config/budi/statusline.toml` files
  written against the 8.0 vocabulary (`slots = ["today", "week",
  "month"]`) continue to render, since `today` / `week` / `month` are
  normalized to `1d` / `7d` / `30d` at load time.
- **Default install path is quiet.** `budi init` and
  `budi integrations install` install the rolling `1d` / `7d` / `30d`
  preset without prompting. The `coach` and `full` presets remain
  opt-in advanced variants documented in `README.md`.
- **`budi init` installs the statusline by default.** A fresh
  `budi init` wires the Budi statusline into `~/.claude/settings.json`
  (and installs the Cursor extension when the Cursor CLI is on PATH)
  without prompting. Pass `--no-integrations` to opt out, or run
  `budi integrations install --with claude-code-statusline` / `--with
  cursor-extension` later. The installer is idempotent: repeat runs
  merge with an existing `statusLine` command rather than clobbering
  it, and skip the Cursor-extension install when one is already
  present.
- **`budi doctor` flags a missing statusline.** When `~/.claude`
  exists but `~/.claude/settings.json` carries no Budi-backed
  `statusLine`, doctor surfaces a WARN with the exact `budi
  integrations install` command to repair it. Install paths that
  legitimately want no Claude integration (CI, containers,
  hand-rolled settings) pass `budi init --no-integrations` to
  suppress the nudge from day one.

`budi doctor` runs three attribution checks:

- **Session visibility** for the `today`, `7d`, and `30d` windows — fails
  when a window has assistant rows but zero returned sessions.
- **Branch attribution (7d, per provider)** — yellow at >10% of assistant
  rows missing `git_branch`, red at >50%. A red result points at a broken
  attribution path for that provider (no resolvable `gitBranch`, session
  propagation not rescuing the session) even if overall cost numbers look
  healthy.
- **Activity attribution (7d, per provider)** — red when a provider's
  recent assistant rows are effectively fully silent (≥99.9% missing an
  `activity` tag, float-tolerant so one legacy row doesn't save an
  otherwise-silent classifier) and the window has at least 5 rows (a
  silent classifier regression). Yellow at >90% to hint at an
  over-aggressive skip path without tripping a hard fail; a moderate
  missing-ratio is expected because one-word prompts and slash commands
  never carry an `activity` tag by design. See `activity_attribution` in
  `crates/budi-cli/src/commands/doctor.rs`.

### Key concepts

- **cost_confidence**: determines `~` prefix in dashboard for non-exact costs
- **Source of truth vs derived**: `messages` remains canonical; rollup tables are derived caches maintained incrementally via SQLite triggers during ingest/update/delete
- **Session context propagation**: git_branch/repo_id flow from user -> assistant messages within a session
- **Repository identity**: `repo_id` is `Some("host/owner/repo")` only when the cwd is inside a git repo with a remote origin. Non-repo work (scratch dirs, `~/Desktop`, brew-tap checkouts, local-only repos without upstream) persists `repo_id = NULL` and rolls up into a single `(no repository)` bucket in `budi stats projects`. An idempotent one-shot backfill on startup rewrites any pre-8.3 bare-folder-name values (`Desktop`, `ivan.seredkin`, …) to NULL; `--include-non-repo` opts back into the per-folder detail.
- **Progressive sync**: files processed newest-first so dashboard shows recent data quickly
- **Historical import**: `budi db import` = full history backfill, `budi db import --force` = clear all data and re-ingest from scratch. `budi db <verb>` is the only surface — the pre-8.2.1 bare verbs (`budi migrate` / `budi repair` / `budi import`) were removed in 8.3.
- **Legacy proxy residue (upgrade only)**: live traffic no longer flows through a proxy. The only remaining proxy-related code scans for 8.0/8.1 residue in shell profiles and agent configs, reports retained `proxy_estimated` history honestly, and lets users remove managed blocks via `budi init --cleanup` (consent-first) or `budi uninstall` (managed cleanup parity).

## Key files

- `crates/budi-core/src/analytics/mod.rs` - SQLite storage, sync pipeline, all query functions
- `crates/budi-core/src/analytics/health.rs` - Session health vitals, ProviderKind-aware tips, overall-state logic
- `crates/budi-core/src/analytics/tests.rs` - Analytics + session health unit tests
- `crates/budi-core/src/pipeline/mod.rs` — `Pipeline` struct, `Enricher` trait, `default_pipeline()` (ordered: IdentityEnricher → GitEnricher → ToolEnricher → FileEnricher → CostEnricher → TagEnricher). Hosts the cross-message tool-outcome correlation and retry heuristic that emit `tool_outcome` / `tool_outcome_source` / `tool_outcome_confidence` tags after the per-message enricher pass.
- `crates/budi-core/src/pipeline/enrichers.rs` — All enricher implementations (`IdentityEnricher`, `GitEnricher`, `ToolEnricher`, `FileEnricher`, `CostEnricher`, `TagEnricher`).
- `crates/budi-core/src/file_attribution.rs` — Repo-relative file-path extractor; enforces ADR-0083 privacy limits (no absolute paths, no outside-of-repo paths, no file contents).
- `crates/budi-core/src/work_outcome.rs` — Session-scoped `work_outcome` derivation (`committed`, `branch_merged`, `no_commit`, `unknown`) from local git state only; no remote API calls, no content capture.
- `crates/budi-core/src/cost.rs` — Cost estimation glue (aggregates `cost_cents` from `messages`). Rates come exclusively from the manifest-backed `pricing::lookup` ([ADR-0091](docs/adr/0091-model-pricing-manifest-source-of-truth.md)); there is no fallback path.
- `crates/budi-core/src/pricing/mod.rs` — Pricing loader + `lookup` API. Three-layer resolution (on-disk cache → embedded LiteLLM baseline → `unknown`), `PricingSource` tagging for immutable history, `backfill_unknown_rows` for retroactive fill-in once upstream catches up, validation guards (>95% retention floor on kept rows, $1,000/M sanity ceiling applied row-by-row per the 8.3.1 ADR-0091 §2 amendment so one bad upstream row can't DoS the whole refresh, 10 MB size cap). `partition_rows_by_sanity` + `RejectedUpstreamRow` surface the dropped rows on `GET /pricing/status`.
- `crates/budi-core/src/pricing/manifest.embedded.json` — Vendored snapshot of LiteLLM's `model_prices_and_context_window.json`, refreshed per release by `scripts/pricing/sync_baseline.sh`.
- `crates/budi-core/src/hooks.rs` — Prompt classification helpers (hook ingestion removed in 8.0; `hook_events` table gone in schema v1).
- `crates/budi-core/src/jsonl.rs` — JSONL transcript parser, `ParsedMessage` struct.
- `crates/budi-core/src/providers/claude_code.rs` — Claude Code provider (JSONL discovery + live watch).
- `crates/budi-core/src/providers/codex.rs` — Codex provider (Codex Desktop/CLI transcripts under `~/.codex/sessions/`).
- `crates/budi-core/src/providers/copilot.rs` — Copilot CLI provider (transcripts under `~/.copilot/session-state/`).
- `crates/budi-core/src/providers/cursor.rs` — Cursor provider (Usage API primary + transcript fallback; auth/session context from `state.vscdb` across macOS/Linux/Windows). Usage-API rows write `pricing_source = 'upstream:api'`; transcript-fallback rows flow through `pricing::lookup`.
- `crates/budi-core/src/migration.rs` — Schema v1, all migration paths.
- `crates/budi-core/src/cloud_sync.rs` — Cloud sync worker: envelope builder, watermark tracking, HTTPS-only HTTP client with retry/backoff, privacy-safe rollup extraction.
- `crates/budi-core/src/autostart.rs` — Platform-native daemon autostart: launchd (macOS), systemd (Linux), Task Scheduler (Windows). Install/uninstall/status.
- `crates/budi-core/src/config.rs` — `BudiConfig`, `AgentsConfig`, `StatuslineConfig`, `TagsConfig`, `CloudConfig`.
- `crates/budi-core/src/legacy_proxy.rs` — Upgrade-only detection/cleanup for managed 8.0/8.1 proxy residue in shell profiles and agent configs.
- `crates/budi-cli/build.rs` — Build script: creates empty vsix placeholder if not pre-built.
- `crates/budi-daemon/src/main.rs` — HTTP server (port 7878) + cloud sync worker + startup hooks for tailer / migration / legacy-residue notices.
- `crates/budi-daemon/src/workers/cloud_sync.rs` — Background cloud sync loop: configurable interval, backoff, auth/schema error handling.
- `crates/budi-daemon/src/workers/pricing_refresh.rs` — 24 h LiteLLM manifest refresh loop. Warm-loads the on-disk cache (running row-level sanity partitioning on restart per the 8.3.1 ADR-0091 §2 amendment), validates fetched payloads, atomic-writes, hot-swaps `pricing` state, runs `backfill_unknown_rows`. Rejected rows are structured-logged and surfaced on `GET /pricing/status`. Disabled via `BUDI_PRICING_REFRESH=0`.
- `crates/budi-daemon/src/routes/pricing.rs` — `GET /pricing/status` + `POST /pricing/refresh` (loopback-only).
- `crates/budi-daemon/src/routes/hooks.rs` — `/sync*`, `/health*`, `/admin/integrations/install` endpoints (hook ingestion removed; route file name retained for stability).
- `crates/budi-daemon/src/routes/cloud.rs` — `/cloud/sync` (loopback-only manual cloud flush) and `/cloud/status`.
- `crates/budi-daemon/src/routes/analytics.rs` — All analytics + admin endpoints.
- `crates/budi-cli/src/commands/init.rs` — `budi init` (daemon + autostart + recommended-integrations + detected-agent output) plus consent-first `--cleanup`.
- `crates/budi-cli/src/commands/doctor.rs` — `budi doctor` health checks (daemon, schema, transcript visibility, statusline wiring, attribution ratios, legacy proxy residue).
- `crates/budi-cli/src/commands/uninstall.rs` — `budi uninstall` removes autostart, Claude/Cursor integrations, and managed legacy proxy residue.
- `crates/budi-cli/src/commands/sessions.rs` — `budi sessions` list and detail view.
- `crates/budi-cli/src/commands/status.rs` — `budi status` quick overview (daemon, today's cost, first-run hints).
- `crates/budi-cli/src/commands/statusline.rs` — Statusline rendering (default quiet rolling `1d` / `7d` / `30d`, provider-scoped; `coach` / `full` opt-in variants).
- `crates/budi-cli/src/commands/cloud.rs` — `budi cloud sync` / `budi cloud status` / `budi cloud init` (text + JSON; exit code 2 on non-ok sync).
- `crates/budi-cli/src/commands/pricing.rs` — `budi pricing` / `budi pricing status` (read-only) + `budi pricing sync` (network refresh; mirrors `cloud sync`). Both accept `--format json`.
<!-- budi-cursor and budi-cloud live in their own repos: siropkin/budi-cursor, siropkin/budi-cloud -->

## Dev notes

- CLI never touches SQLite directly — all queries go through the daemon HTTP API.
- `CostEnricher` is the single source of truth for cost. It sets `cost_cents` during the pipeline, skipping when cost is already set (e.g., Cursor Usage API rows). It calls `pricing::lookup(model_id, provider)`, which resolves against a three-layer stack (on-disk cache → embedded LiteLLM baseline → `unknown`) per [ADR-0091](docs/adr/0091-model-pricing-manifest-source-of-truth.md). Unknown models land with `cost_cents = 0`, `pricing_source = 'unknown'`, and a warn, then auto-backfill to `backfilled:vNNN` once upstream catches up. History is immutable — `manifest:vNNN` and `legacy:pre-manifest` rows are never auto-recomputed, and there is no `budi pricing recompute` command.
- `budi init` creates the data dir, validates schema/binary state, starts the daemon, installs autostart, wires recommended integrations (Claude Code statusline + Cursor extension) idempotently, prints detected agents from `Provider::watch_roots()`, and exits. `budi doctor` is the canonical end-to-end verifier and prints a first-run nudge when the DB has no assistant activity yet, so day-zero users don't misread empty attribution as a setup failure. Install scripts close with the same `budi doctor` recommendation. `budi init --cleanup` is the explicit upgrade-only path for reviewing/removing managed 8.0/8.1 proxy residue.
- Tags are auto-detected (`provider`, `model`, `tool`, `tool_use_id`, `ticket_id`, `ticket_source`, `activity`, `activity_source`, `activity_confidence`, `file_path`, `file_path_source`, `file_path_confidence`, `tool_outcome`, `tool_outcome_source`, `tool_outcome_confidence`, plus conditional tags like `cost_confidence` / `speed`) with custom rules available via `~/.config/budi/tags.toml`.
- `git_branch` is a column on `messages` (not a tag) for fast queries.
- **Session health**: four vitals — context growth, cache reuse, cost acceleration, retry loops (currently disabled pending a rebuild on top of the tool-outcome signal). Each vital has green/yellow/red state. New sessions start green; vitals only degrade when there is clear evidence of a problem. Tips are provider-aware via the `ProviderKind` enum (Claude Code → `/compact`/`/clear`, Cursor → "new composer session", Other → neutral). When no session ID is provided, auto-select prefers the latest session with assistant activity, then falls back to session timestamps. The statusline `coach` preset shows health icon + session cost + tip; the cloud dashboard session detail page has a full health panel.
- **Cursor extension** ([siropkin/budi-cursor](https://github.com/siropkin/budi-cursor)) is a statusline-only VS Code extension (no sidebar, no session list, no vitals/tips panel). The status bar consumes the shared provider-scoped status contract with `?provider=cursor`, mirrors the Claude Code statusline byte-for-byte (`🟢 budi · $X 1d · $Y 7d · $Z 30d`), and click-through opens the same cloud URL the Claude Code statusline opens. Installed via the VS Code Marketplace or `budi integrations install --with cursor-extension`. Communicates with the daemon via HTTP and spawns `budi statusline --format json --provider cursor`. Writes `~/.local/share/budi/cursor-sessions.json` (v1 contract, ADR-0086 §3.4) to signal the active workspace. Checks daemon `api_version` on startup (`MIN_API_VERSION = 1`) and warns if incompatible. Also acts as a first-class onboarding entry point for users who install it before the daemon: a welcome view with a pre-filled install command, and a local-only `~/.local/share/budi/cursor-onboarding.json` counter file that `budi doctor` surfaces so install-funnel health is visible without remote telemetry.
- **Cloud dashboard** ([siropkin/budi-cloud](https://github.com/siropkin/budi-cloud)) is a Next.js 16 app deployed to `app.getbudi.dev`. Uses Supabase Auth (GitHub/Google/magic link) for web sign-in. Dashboard pages: Overview, Team, Models, Repos, Sessions, Settings. Manager role sees all org data; member sees own data.
- For the full HTTP surface (analytics, admin, sync, health, pricing, cloud), see the README "Daemon API" details block — that table is the canonical list and is kept in sync with the route files.
