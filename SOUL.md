# SOUL.md

Local-first cost analytics for AI coding agents (Claude Code, Codex CLI, Cursor, Copilot CLI). Tracks tokens, costs, and usage per message via proxy interception. Historical data from Claude Code JSONL transcripts and Cursor Usage API can be imported via `budi import`. Optional cloud sync (disabled by default) pushes pre-aggregated daily rollups to a team dashboard — prompts, code, and responses never leave the machine (see [ADR-0083](docs/adr/0083-cloud-ingest-identity-and-privacy-contract.md)).

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

Shell-driven end-to-end tests live under `scripts/e2e/`. They exercise the full stack — real release binaries (`budi` + `budi-daemon`), a mock upstream over loopback, the HTTP proxy path, and the CLI — against an isolated `$HOME` so they never touch real user data.

```bash
cargo build --release                                 # once per change
bash scripts/e2e/test_302_sessions_visibility.sh      # regression guard for #302
bash scripts/e2e/test_303_branch_attribution.sh       # regression guard for #303
bash scripts/e2e/test_221_ticket_first_class.sh       # regression guard for #221 / #304 (ticket dimension)
bash scripts/e2e/test_222_activity_classification.sh  # regression guard for #222 / #305 (activity dimension)
bash scripts/e2e/test_224_statusline_provider_scope.sh # regression guard for #224 (statusline provider scoping)
```

Each script is a single self-contained bash file that:

1. Builds a throwaway `HOME` in `mktemp` and exports it for the whole run.
2. Boots a tiny Python mock upstream on loopback.
3. Starts `budi-daemon serve --port … --proxy-port …` with `BUDI_ANTHROPIC_UPSTREAM` / `BUDI_OPENAI_UPSTREAM` pointed at the mock (these env vars override the hard-coded upstreams — see `ProxyConfig::effective_anthropic_upstream` / `effective_openai_upstream`).
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

- **budi-core** - Business logic: analytics (SQLite queries), providers (Claude Code, Codex, Copilot CLI, Cursor), pipeline (enrichment), cost calculation, proxy event storage, config, migrations, autostart (platform-native daemon service management). Historical hook/OTEL data is read-only (tables kept for schema compat, ingestion removed)
- **budi-cli** - Thin HTTP client to the daemon. Commands: init, launch, stats, sessions, status, sync, import, statusline, doctor, health, update, integrations, autostart, uninstall, migrate, repair
- **budi-daemon** - axum HTTP server (port 7878). Owns SQLite exclusively. Serves analytics API. Also runs the proxy server on port 9878. The proxy is the sole live data source; transcript import is user-initiated via `budi import`

### Data flow

```
Live data:
Proxy (agent -> localhost:9878 -> upstream provider)
  -> Path-based routing (Anthropic /v1/messages, OpenAI /v1/chat/completions)
  -> Attribution: X-Budi-Repo/Branch/Cwd headers -> git resolution -> Unassigned fallback
  -> SSE: chunk-by-chunk pass-through with tee/tap token extraction
  -> Non-SSE: buffered with JSON usage parsing
  -> Cost: computed from provider pricing tables
  -> SQLite (proxy_events table + messages table for unified analytics)

Historical import (budi import):
Sources (Claude Code JSONL, Codex sessions, Copilot CLI sessions, Cursor API)
  -> Providers discover + parse -> ParsedMessage structs
  -> Pipeline: IdentityEnricher -> GitEnricher -> ToolEnricher -> CostEnricher -> TagEnricher
  -> SQLite (messages + tags + derived rollup tables)
  -> Dashboard / CLI stats / Statusline
```

Enricher order is critical - each depends on prior enrichers. Do not reorder.

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

Manual cloud sync (since 8.1, R2.1, #225):
`budi cloud sync`     -> POST /cloud/sync (loopback-only) -> same sync_tick as worker
`budi cloud status`   -> GET /cloud/status -> readiness + watermarks, no network call
AppState.cloud_syncing AtomicBool guards worker and manual path from double-posting.
```

### Database (SQLite, WAL mode, schema v1)

Nine tables, seven data entities + two supporting:
- **messages** - Single cost entity. One row per API call. All token/cost data lives here. Fields: id, session_id, role, model, provider, timestamp, input/output/cache tokens, cost_cents, cost_confidence, git_branch, repo_id, cwd, request_id
- **sessions** - Lifecycle context (start/end, duration, mode, title) without mixing cost concerns. One row per conversation. Primary key field: id
- **proxy_events** - Append-only log of proxied LLM API requests. Fields: timestamp, provider, model, input/output_tokens, duration_ms, status_code, is_streaming, repo_id, git_branch, ticket_id, cost_cents. Successful proxy events are also inserted into `messages` (cost_confidence='proxy_estimated') so existing analytics surfaces work without modification
- **tags** - Flexible key-value pairs per message (repo, ticket_id, activity, user, etc.) using message_id FK to messages(id)
- **sync_state** - Tracks incremental ingestion progress per file for progressive sync. Also stores cloud sync watermarks (`__budi_cloud_sync__` keys) for idempotent cloud uploads
- **message_rollups_hourly** - Derived hourly aggregates (provider/model/repo/branch/role dimensions) for low-latency analytics reads
- **message_rollups_daily** - Derived daily aggregates for coarse-grained summaries and filter option scans

### Cost sources

| Source | Confidence | What it provides |
|--------|-----------|-----------------|
| **Proxy** (all agents) | `proxy_estimated` | Real-time per-request tokens from response body (non-streaming) or SSE tee/tap extraction (streaming). Attribution via `X-Budi-Repo`, `X-Budi-Branch`, `X-Budi-Cwd` headers or git-resolved from cwd. Falls back to `Unassigned` repo |
| **JSONL** (Claude Code) | `estimated` | Per-message tokens (no thinking), cost calculated from pricing. Used by `budi import` for historical backfill |
| **JSONL** (Codex) | `estimated` | Per-API-call tokens from `token_count` events in `~/.codex/sessions/`. Used by `budi import` for historical backfill |
| **JSONL** (Copilot CLI) | `estimated` | Per-API-call tokens from `assistant.usage` events in `~/.copilot/session-state/`. Used by `budi import` for historical backfill |
| **Cursor Usage API** | `exact` | Per-request tokens + totalCents from Cursor's API. Used by `budi import` for historical backfill |

Historical OTEL data (`otel_exact` confidence) remains queryable but OTEL ingestion has been removed. The proxy is the sole live data source.

### Attribution contract (R1.0)

Every ingestor that writes to `messages` MUST uphold the following so that the
CLI, daemon, and dashboard tell the same story (see ADR-0082, [ADR-0088](docs/adr/0088-8x-local-developer-first-product-contract.md) §5, and the R1.0 bugs
in [#302](https://github.com/siropkin/budi/issues/302) / #303 / #304 / #305):

- **`timestamp`** — RFC3339 string in UTC. Accept both `...Z` and `...+00:00`
  offsets; `session_list_with_filters` and `activity_chart` compare these as
  strings, so never write naive SQLite datetime (`YYYY-MM-DD HH:MM:SS`) or a
  local-offset string. Providers emit RFC3339 from `DateTime::<Utc>::to_rfc3339()`
  (Claude Code JSONL, Codex) or `DateTime::from_timestamp_millis(..).to_rfc3339()`
  (Cursor, proxy).
- **`session_id`** — required for every live assistant row. Live proxy traffic
  uses the agent-provided `X-Budi-Session` header, falling back to
  `generate_proxy_session_id()`. Empty-string `session_id` is treated as NULL
  by the analytics layer, and the insert path normalizes `""` to `NULL` so
  ghost `(empty)` sessions cannot appear. Rows with NULL/empty `session_id`
  are invisible to `budi sessions` by design — they indicate an attribution
  bug upstream, not a display bug.
- **`provider`** — canonical provider key (`claude_code`, `cursor`, `openai`,
  `copilot`). `COALESCE(provider, 'claude_code')` is the legacy fallback for
  pre-8.0 rows; new writes MUST set it explicitly.
- **`git_branch`** — written without the `refs/heads/` prefix
  (`session_list_with_filters` strips it defensively for older rows). Live
  proxy ingest resolves the branch in this priority order
  (`ProxyAttribution::resolve` in `crates/budi-core/src/proxy.rs`):
  1. **`X-Budi-Branch` header** — set by an integration shim that knows the
     client's git state (e.g. a future agent wrapper).
  2. **`X-Budi-Cwd` header** → `git rev-parse --abbrev-ref HEAD` — the proxy
     shells out to git against the client-supplied cwd.
  3. **Session-level propagation in `insert_proxy_message`** (R1.0.2, #303)
     — if the incoming event has no branch, the insert path looks up the
     most recent message in the same session that does and adopts it; if
     the incoming event does resolve a branch, earlier NULL-branch rows in
     the same session are backfilled in the same transaction. This mirrors
     the batch pipeline's `propagate_session_context` on the live path so
     that once a session learns its branch, every row in that session
     reflects it.
  4. **`Unassigned` repo + empty branch** — last-resort fallback. Rows in
     this state surface as `(untagged)` in `budi stats --branches`.

  A detached HEAD (`git rev-parse --abbrev-ref HEAD` == `"HEAD"`) is
  explicitly normalized to empty so that worktrees, mid-rebase sessions, and
  CI runs do not pollute the branches list with a bogus `HEAD` bucket.

- **`ticket_id`** — promoted to a first-class CLI dimension in 8.1 (R1.0.3,
  #304) and further hardened in R1.3
  ([#221](https://github.com/siropkin/budi/issues/221)). Both the batch
  pipeline (`GitEnricher`) and the live proxy
  (`ProxyAttribution::resolve`) now share one extractor —
  `pipeline::extract_ticket_from_branch` — which (1) filters integration
  branches (`main`, `master`, `develop`, `HEAD`), (2) prefers the canonical
  alphanumeric pattern (e.g. `ENG-123`, `PAVA-2120` anywhere in the
  branch), then (3) falls back to a numeric-only id for branches like
  `feature/1234` or `42-quick-fix`. Every emitted `ticket_id` tag is paired
  with:
    - `ticket_prefix` — alphabetic prefix (`ENG`, `PAVA`), or empty for
      numeric-only ids; and
    - `ticket_source` — explains how the id was derived: `branch` for the
      alphanumeric pattern, `branch_numeric` for the numeric fallback.
      Reserved for future `header` / `hint` sources from a smarter client
      shim. Mirrors the `activity_source` contract so every first-class
      attribution dimension carries its own provenance.

  Messages without a recognised ticket emit no `ticket_id` tag (no legacy
  `Unassigned` sentinel); they surface as `(untagged)` in the tickets
  list, keeping bucket semantics consistent across branch / ticket /
  activity views.

  Surfaces:
  - `budi stats --tickets` — list ranked by cost, with `(untagged)`
    bucket and a `src=…` column showing the dominant `ticket_source`.
  - `budi stats --ticket <ID>` — detail view with per-branch breakdown
    and a `Source` row. Legacy rows without a `ticket_source` sibling
    tag default to `branch` (the only pre-R1.3 pipeline producer) so
    older DBs stay readable without a reindex.
  - `budi sessions --ticket <ID>` — sessions tagged with the ticket.
  - `GET /analytics/tickets` and `/analytics/tickets/{ticket_id}` mirror
    `/analytics/branches{/branch}` so future cloud/dashboard work can adopt
    the same data contract.

- **`activity`** — promoted to a first-class CLI dimension in 8.1 (R1.0.4,
  [#305](https://github.com/siropkin/budi/issues/305)); strengthened in
  R1.2 ([#222](https://github.com/siropkin/budi/issues/222)). The pipeline
  emits an `activity` tag for every assistant message whose session has a
  classified prompt category (bugfix, refactor, testing, feature, review,
  ops, question, writing, **docs**). Values come from the rule-based
  `hooks::classify_prompt_detailed` and are propagated across the session
  by `propagate_session_context`, so every assistant message in a
  classified session carries exactly one `activity` tag. R1.2 also emits
  two companion tags — `activity_source` (`rule` when derived from the
  rule-based classifier; reserved for future `header` / `hint` sources)
  and `activity_confidence` (`high` when anchored by a leading
  action phrase with a strong keyword hit, `medium` for a clear single
  keyword hit, `low` when the match is weak or based on fallback
  heuristics). Precedence: a leading question-anchor phrase ("explain",
  "what is", "how do I") wins over generic `bugfix` keywords unless the
  prompt also starts with a bugfix action ("fix the error"). Coverage
  extends beyond Claude Code JSONL ingestion to:
    - **Cursor JSONL ingestion** — user prompts are classified at parse
      time in `providers::cursor::parse_cursor_line`.
    - **Proxy live path** — the daemon route calls
      `budi_core::proxy::classify_request_body` on the request body
      before forwarding, extracts the last user turn in-memory, and
      records only the derived `(activity, source, confidence)` triple
      as tags. Per [ADR-0083](docs/adr/0083-privacy-constraints.md) no
      prompt text is persisted on the proxy path.
  Analytics recompute the dominant `activity_source` /
  `activity_confidence` per activity from the stored tags (most frequent
  value wins, ties broken alphabetically), falling back to R1.0 defaults
  (`rule` / `medium`) only when an activity has no companion tags yet
  (pre-R1.2 data). Surfaces:
  - `budi stats --activities` — list ranked by cost, with `(untagged)`
    bucket for messages that never matched a classification rule (short
    prompts, slash commands, metadata-only messages).
  - `budi stats --activity <NAME>` — detail view with per-branch
    breakdown, plus `source` and `confidence` labels.
  - `budi sessions --activity <NAME>` — sessions tagged with the
    activity, mirroring `--ticket`.
  - `GET /analytics/activities` and `/analytics/activities/{name}`
    mirror the ticket endpoints so future cloud/dashboard work can adopt
    the same data contract.

- **`file_path`** — per-file attribution added in R1.4
  ([#292](https://github.com/siropkin/budi/issues/292)). When an assistant
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
       [ADR-0083](docs/adr/0083-privacy-constraints.md).
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
  - `budi stats --files` — files ranked by cost, with `(untagged)`
    bucket and a `src=…` column showing the dominant source. Long
    paths are truncated in the CLI output; full paths stay available
    via `--file <PATH>` and `--format json`.
  - `budi stats --file <PATH>` — detail view with per-branch **and**
    per-ticket breakdowns, so you can see which tickets charged cost
    to a particular file.
  - `GET /analytics/files` and `/analytics/files/{*path}` mirror the
    ticket / activity endpoints; the path segment is validated to be
    repo-relative (no leading `/`, no `..`, no Windows separators, no
    URL scheme) before hitting SQLite.

- **`tool_outcome`** — per-message tool-call outcome added in R1.5
  ([#293](https://github.com/siropkin/budi/issues/293)). The JSONL
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
  contract. Messages with no tool uses carry no outcome tag. The proxy
  ingest path does not emit outcomes in 8.1 — tool names and IDs
  aren't captured there yet — so outcomes are import-only for now.

- **`work_outcome`** (session-scoped) — derived in R1.5 from local
  git state only. `budi session detail <id>` correlates the session's
  `git_branch` with commits on that branch between the session's
  start and its end + 24h grace, producing one of `committed`,
  `branch_merged`, `no_commit`, or `unknown`. The derivation runs
  `git` locally — no remote Git/PR API calls, no content capture —
  and fails open to `unknown` whenever the branch is missing, is an
  integration branch (`main`, `master`, `develop`), or the repo root
  can't be resolved. A one-line rationale accompanies every label so
  operators can see which rule fired. List surfaces skip the
  derivation (one `git` invocation per session list row is too
  expensive); only the detail view surfaces it.

### Statusline contract (R2.3, #224)

The JSON shape emitted by `GET /analytics/statusline` and
`budi statusline --format json` is the single shared provider-scoped
status contract. It is consumed by the CLI statusline, the Cursor
extension ([#232](https://github.com/siropkin/budi/issues/232)), and the
cloud dashboard ([#235](https://github.com/siropkin/budi/issues/235)).
Provider is an explicit filter rather than a family of per-surface
shapes — new agents added in 8.2 under
[#294](https://github.com/siropkin/budi/issues/294) slot into the same
shape. See [`docs/statusline-contract.md`](docs/statusline-contract.md)
for the full schema.

Key points:

- **Rolling `1d` / `7d` / `30d` windows** (`cost_1d`, `cost_7d`,
  `cost_30d`) — not calendar today/week/month. The statusline surface
  is the only place Budi uses rolling windows; `budi stats` keeps
  calendar semantics.
- **Provider-scoping is strict.** When the request carries
  `provider=claude_code`, every numeric field (`cost_*`, `session_cost`,
  `branch_cost`, `project_cost`) and `active_provider` are filtered to
  that provider. The Claude Code statusline uses this by default so it
  never shows blended multi-provider totals (the 8.0 bug #224 was
  opened against).
- **Deprecated aliases** `today_cost` / `week_cost` / `month_cost` are
  kept populated with the same rolling values for one release of
  backward compatibility and are removed in 9.0. New consumers read
  `cost_1d` / `cost_7d` / `cost_30d`.
- **Slot config aliases.** `~/.config/budi/statusline.toml` files
  written against the 8.0 vocabulary (`slots = ["today", "week",
  "month"]`) continue to render, since `today` / `week` / `month` are
  normalized to `1d` / `7d` / `30d` at load time.
- **Default install path is quiet.** `budi init` and
  `budi integrations install` no longer prompt for a statusline preset;
  the default is the rolling `1d` / `7d` / `30d` cost view. The
  `coach` and `full` presets remain as opt-in advanced variants
  documented in `README.md`.

`budi doctor` runs three attribution checks:

- **Session visibility** for the `today`, `7d`, and `30d` windows (R1.0.1,
  #302) — fails when a window has assistant rows but zero returned sessions.
- **Branch attribution (7d, per provider)** (R1.0.2, #303) — yellow at >10%
  of assistant rows missing `git_branch`, red at >50%. A red result points
  at a broken attribution path for that provider (no headers, no resolvable
  cwd, session propagation not rescuing the session) even if overall cost
  numbers look healthy.
- **Activity attribution (7d, per provider)** (R1.0.4, #305) — red when
  a provider's recent assistant rows are effectively fully silent
  (≥99.9% missing an `activity` tag, float-tolerant so a single legacy
  row without an activity doesn't save an otherwise-silent classifier)
  and it has at least 5 rows in the window (a silent classifier
  regression). Yellow at >90% to hint at an over-aggressive skip path
  without tripping a hard fail; a moderate missing-ratio is expected
  because one-word prompts and slash commands never carry an `activity`
  tag by design. See `activity_attribution` in
  `crates/budi-cli/src/commands/doctor.rs`.

### Key concepts

- **cost_confidence**: determines `~` prefix in dashboard for non-exact costs
- **Source of truth vs derived**: `messages` remains canonical; rollup tables are derived caches maintained incrementally via SQLite triggers during ingest/update/delete
- **Session context propagation**: git_branch/repo_id flow from user -> assistant messages within a session
- **Progressive sync**: files processed newest-first so dashboard shows recent data quickly
- **Historical import**: `budi import` = full history backfill, `budi import --force` = clear all data and re-ingest from scratch
- **Proxy mode**: Daemon runs a second HTTP server on port 9878 that acts as a transparent proxy between AI agents and upstream providers (Anthropic, OpenAI). `budi init` auto-installs proxy routing for selected agents: shell-profile env block for Claude Code/Codex/Copilot, Cursor settings patch (`openai.baseUrl`), and Codex Desktop config patch (`openai_base_url` in `~/.codex/config.toml`). `budi enable <agent>` / `budi disable <agent>` toggle this configuration. `budi launch <agent>` remains an explicit fallback launcher, and `BUDI_BYPASS=1 budi launch <agent>` skips proxy injection for one run. Gemini CLI is deferred (Tier 3, different API format). Path-based routing: `/v1/messages` → Anthropic, `/v1/chat/completions` → OpenAI. SSE streaming responses are passed through chunk-by-chunk with no buffering; a tee/tap on the byte stream extracts token metadata (input/output tokens) from SSE events without modifying the data. Non-streaming responses are buffered and parsed for usage data. Duration is measured from request start to stream end (not to first headers). Mid-stream failures and client disconnects are handled gracefully — partial metadata is recorded via Drop. No read timeout on streaming; non-streaming uses 300s. Config: `[proxy]` section in `config.toml`, `BUDI_PROXY_PORT` / `BUDI_PROXY_ENABLED` env vars, `--proxy-port` / `--no-proxy` CLI flags. See [ADR-0082](docs/adr/0082-proxy-compatibility-matrix-and-gateway-contract.md) for the full contract.

## Key files

- `crates/budi-core/src/analytics/mod.rs` - SQLite storage, sync pipeline, all query functions
- `crates/budi-core/src/analytics/health.rs` - Session health vitals, ProviderKind-aware tips, overall-state logic
- `crates/budi-core/src/analytics/tests.rs` - Analytics + session health unit tests
- `crates/budi-core/src/pipeline/mod.rs` - Pipeline struct, Enricher trait, default_pipeline() (ordered: IdentityEnricher → GitEnricher → ToolEnricher → FileEnricher → CostEnricher → TagEnricher); also hosts the cross-message tool-outcome correlation and retry heuristic that emit `tool_outcome` / `tool_outcome_source` / `tool_outcome_confidence` tags after the per-message enricher pass
- `crates/budi-core/src/pipeline/enrichers.rs` - All 6 enricher implementations (`IdentityEnricher`, `GitEnricher`, `ToolEnricher`, `FileEnricher`, `CostEnricher`, `TagEnricher`; `HookEnricher` removed in 8.0, `FileEnricher` added in R1.4 #292)
- `crates/budi-core/src/file_attribution.rs` - R1.4 (#292) repo-relative file-path extractor, enforces ADR-0083 privacy limits (no absolute paths, no outside-of-repo paths, no file contents)
- `crates/budi-core/src/work_outcome.rs` - R1.5 (#293) session-scoped `work_outcome` derivation (`committed`, `branch_merged`, `no_commit`, `unknown`) from local git state only — no remote API calls, no content capture
- `crates/budi-core/src/cost.rs` - Cost estimation, ModelPricing, per-provider pricing tables
- `crates/budi-core/src/hooks.rs` - Prompt classification and migration helpers (hook ingestion removed in 8.0; `hook_events` table no longer exists in schema v1)
- `crates/budi-core/src/jsonl.rs` - JSONL transcript parser, ParsedMessage struct
- `crates/budi-core/src/providers/claude_code.rs` - Claude Code provider (JSONL discovery, pricing)
- `crates/budi-core/src/providers/codex.rs` - Codex provider (Codex Desktop/CLI transcript import from `~/.codex/sessions/`, OpenAI model pricing)
- `crates/budi-core/src/providers/copilot.rs` - Copilot CLI provider (transcript import from `~/.copilot/session-state/`, delegates pricing to Claude/OpenAI based on model)
- `crates/budi-core/src/providers/cursor.rs` - Cursor provider (Usage API primary, transcript fallback; auth/session context from state.vscdb across macOS/Linux/Windows layouts)
- `crates/budi-core/src/migration.rs` - Schema v1, all migration paths
- `crates/budi-core/src/proxy.rs` - ProxyEvent types with attribution (repo, branch, ticket, cost), proxy_events and messages table storage, ProxyAttribution resolution from headers/git
- `crates/budi-core/src/cloud_sync.rs` - Cloud sync worker: envelope builder, watermark tracking, HTTPS-only HTTP client with retry/backoff, privacy-safe rollup extraction
- `crates/budi-core/src/autostart.rs` - Platform-native daemon autostart: launchd (macOS), systemd (Linux), Task Scheduler (Windows). Install/uninstall/status.
- `crates/budi-core/src/config.rs` - BudiConfig, ProxyConfig, AgentsConfig, StatuslineConfig, TagsConfig, CloudConfig
- `crates/budi-cli/build.rs` - Build script: creates empty vsix placeholder if not pre-built
- `crates/budi-daemon/src/main.rs` - HTTP server (port 7878) + proxy server (port 9878) + cloud sync worker, ~40 routes
- `crates/budi-daemon/src/workers/cloud_sync.rs` - Background cloud sync loop: configurable interval, backoff, auth/schema error handling
- `crates/budi-daemon/src/routes/hooks.rs` - /sync, /sync/all, /sync/reset, /sync/status, /health, /health/integrations, /health/check-update, /admin/integrations/install endpoints (hook ingestion removed)
- `crates/budi-daemon/src/routes/cloud.rs` - /cloud/sync (loopback-only manual cloud flush) and /cloud/status (cloud readiness + watermarks); added in R2.1 (#225)
- `crates/budi-cli/src/commands/cloud.rs` - `budi cloud sync` / `budi cloud status` (R2.1 #225): text + JSON output, exit code 2 on non-ok sync
- `crates/budi-daemon/src/routes/analytics.rs` - All analytics + admin endpoints (summary, messages, projects, cost, models, activity, branches, tags, providers, statusline, cache-efficiency, session-cost-curve, cost-confidence, subagent-cost, sessions, session-health, session-audit, admin/providers, admin/schema, admin/migrate, admin/repair)
- `crates/budi-daemon/src/routes/proxy.rs` - Proxy handlers for Anthropic Messages and OpenAI Chat Completions
- `crates/budi-cli/src/commands/proxy_install.rs` - Auto-proxy installer and verifier: shell profile block + Cursor/Codex config patching + `budi enable/disable`
- `crates/budi-cli/src/commands/launch.rs` - `budi launch <agent>` explicit launcher (fallback path, supports `BUDI_BYPASS=1`)
- `crates/budi-cli/src/commands/sessions.rs` - `budi sessions` list and detail view (Rich CLI)
- `crates/budi-cli/src/commands/status.rs` - `budi status` quick overview (daemon, proxy, today's cost). When the daemon is healthy but no messages are recorded for today, the command prints a first-run hint pointing the user at their agents and at `budi doctor` (R2.2, #228)
- `crates/budi-cli/src/commands/statusline.rs` - Statusline rendering (default: quiet rolling `1d` / `7d` / `30d`, provider-scoped per ADR-0088 §4 / [docs/statusline-contract.md](docs/statusline-contract.md); `coach` / `full` presets remain as opt-in advanced variants) + installation
<!-- budi-cursor and budi-cloud live in their own repos: siropkin/budi-cursor, siropkin/budi-cloud -->

## Dev notes

- CLI never touches SQLite directly - all queries go through the daemon HTTP API
- CostEnricher is the single source of truth for cost - sets cost_cents during pipeline. Skips if cost already set (API data)
- `budi init` prompts for per-agent enablement (Claude Code, Codex CLI, Cursor, Copilot CLI), persists choices to `~/.config/budi/agents.toml`, and auto-configures proxy routing for enabled agents (shell profile + Cursor/Codex settings). `budi enable/disable <agent>` updates this config later. Legacy installs (no `agents.toml`) treat all available agents as enabled for backward compatibility. After configuring CLI agents (Claude, Codex, Copilot), both `budi init` and `budi enable` warn that a shell restart is required for proxy env vars to take effect and suggest `budi launch <agent>` for immediate routing. `budi doctor` detects when proxy env vars are configured in the shell profile but not set in the current process. R2.2 (#228) reshaped the `budi init` "Next steps" output so the restart-terminal prompt is step 1 (previously buried in a trailing warning), `budi doctor` is framed as the canonical end-to-end verifier, and `budi status` is framed as a today-only snapshot. `budi status` adds a friendly "no activity recorded today yet — open your agent and send a prompt" hint when the daemon is healthy but today has zero messages; `budi doctor` prints a matching first-run nudge when the DB has no assistant activity yet, so day-zero users don't misread empty attribution as a setup failure. Install scripts (`scripts/install.sh`, `scripts/install-standalone.sh`, `scripts/install-standalone.ps1`) close with the same `budi doctor` recommendation.
- `budi init` configures integrations (statusline, extension) for enabled agents
- Tags are auto-detected (`provider`, `model`, `tool`, `tool_use_id`, `ticket_id`, `ticket_source`, `activity`, `activity_source`, `activity_confidence`, `file_path`, `file_path_source`, `file_path_confidence`, `tool_outcome`, `tool_outcome_source`, `tool_outcome_confidence`, and conditional tags like `cost_confidence` / `speed`) + custom rules via `~/.config/budi/tags.toml`
- git_branch is a column on messages (not a tag) for fast queries
- **Session health**: Four vitals computed per session - context growth (context-size growth), cache reuse (cache hit rate), cost acceleration (per-reply cost growth), retry loops (currently disabled — hook ingestion removed in 8.0; `hook_events` table no longer exists in schema v1). Each vital has green/yellow/red state. New sessions start green - the default is always positive; vitals only degrade to yellow/red when there is clear evidence of a problem. Tips are provider-aware via `ProviderKind` enum (Claude Code -> `/compact`/`/clear`, Cursor -> "new composer session", Other -> neutral). When no session ID is provided, health auto-select prefers the latest session with assistant activity, then falls back to session timestamps. Statusline "coach" mode shows health icon + session cost + tip. Dashboard session detail page has a health panel with vitals grid and tips section.
- **Cursor extension** ([siropkin/budi-cursor](https://github.com/siropkin/budi-cursor)): VS Code extension that shows session health in the status bar (aggregated health circles) and a side panel (session details, vitals, tips, session list). Installed via VS Code Marketplace or `budi integrations install --with cursor-extension`. Communicates with daemon via HTTP and spawns `budi statusline --format json`. Writes `~/.local/share/budi/cursor-sessions.json` (v1 contract, ADR-0086 §3.4) to signal the active workspace. Checks daemon `api_version` on startup and warns if incompatible. As of v1.2.0 (R2.4, #314) the extension also acts as a first-class onboarding entry point for users who install it from the marketplace before installing the daemon: a dedicated `firstRun` state, an in-editor welcome view with a pre-filled install command, and a local-only `~/.local/share/budi/cursor-onboarding.json` v1 counter file (`welcome_view_impressions`, `open_terminal_clicks`, `handoffs_completed`) that `budi doctor` surfaces so we can see install-funnel health without any remote telemetry. Cross-surface local→cloud linking UX is owned by #235 (R3) per ADR-0088 §6.
- **Cloud dashboard** ([siropkin/budi-cloud](https://github.com/siropkin/budi-cloud)) is a Next.js 16 app deployed to app.getbudi.dev. Uses Supabase Auth (GitHub/Google/magic link) for web sign-in. Dashboard pages: Overview, Team, Models, Repos, Sessions, Settings. Manager role sees all org data; member sees own data.
- Analytics endpoints: `/analytics/summary`, `/analytics/filter-options`, `/analytics/messages`, `/analytics/messages/{message_uuid}/detail`, `/analytics/projects`, `/analytics/cost`, `/analytics/models`, `/analytics/activity` (activity chart timeline), `/analytics/activities`, `/analytics/activities/{name}` (activity buckets — #305), `/analytics/branches`, `/analytics/branches/{branch}`, `/analytics/tickets`, `/analytics/tickets/{ticket_id}`, `/analytics/files`, `/analytics/files/{*path}`, `/analytics/tags`, `/analytics/providers`, `/analytics/statusline`, `/analytics/cache-efficiency`, `/analytics/session-cost-curve`, `/analytics/cost-confidence`, `/analytics/subagent-cost`, `/analytics/sessions`, `/analytics/sessions/{id}`, `/analytics/sessions/{id}/messages`, `/analytics/sessions/{id}/curve`, `/analytics/sessions/{id}/tags`, `/analytics/session-health`, `/analytics/session-audit` (session attribution stats for debugging ingestion)
- Admin endpoints (loopback-only): `/admin/providers` (registered providers), `/admin/schema` (schema version), `/admin/migrate` (run migration), `/admin/repair` (repair schema drift + run migration), `/admin/integrations/install` (integration installer orchestration)
- Sync mutation endpoints (loopback-only): `/sync` (30-day), `/sync/all` (full history), `/sync/reset` (wipe sync state + full re-sync)
- Sync status endpoint: `/sync/status` (syncing flag + last_synced)
- Health endpoints: `/health` (ok + version + api_version), `/health/integrations` (statusline/extension status + DB stats + paths), `/health/check-update` (GitHub releases)
