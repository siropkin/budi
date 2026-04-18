# Changelog

## Unreleased (8.1.0)

### Fixed

- **`budi sessions` now shows today's proxy activity** — `insert_proxy_message` dropped the `session_id` column, so every proxied assistant message was written with `session_id = NULL` and filtered out of `session_list_with_filters`. Both the live proxy path and the defensive analytics filter now treat empty-string `session_id` as NULL so ghost sessions can't reappear (#302).

### Changed

- **Default statusline is quiet, provider-scoped, and centered on rolling `1d` / `7d` / `30d`** (R2.3, #224) — the statusline surface is the primary glance signal for the enterprise developer persona, so the default is now intentionally simple. Rolling `1d` / `7d` / `30d` windows replace calendar today/week/month on this surface (`budi stats` keeps calendar semantics). `budi statusline --format claude` — the binary Claude Code invokes — is auto-scoped to `claude_code` so it no longer mixes Cursor / Codex spend into the Claude Code status line (the 8.0 blended-totals bug this ticket was opened against). `GET /analytics/statusline` and `budi statusline --format json` now expose the shared provider-scoped status contract (see [`docs/statusline-contract.md`](docs/statusline-contract.md)) — new fields `cost_1d` / `cost_7d` / `cost_30d` / `provider_scope`, new `?provider=` filter, new `--provider` CLI flag. Deprecated aliases `today_cost` / `week_cost` / `month_cost` are populated with the same rolling values for one release of backward compatibility and removed in 9.0. `~/.config/budi/statusline.toml` files using the old slot names (`today` / `week` / `month`) keep rendering because the CLI normalizes them to the new canonical slots at load time. Default slot list is now `["1d", "7d", "30d"]`. `budi init` and `budi integrations install` no longer prompt for a statusline preset — the quiet default is the only on-boarded surface; `coach` / `full` remain opt-in advanced variants documented in `README.md`. Governed by ADR-0088 §4. Public-site sync (statusline visuals, sample text, slot names) threaded into #296.
- **Local onboarding and first-run UX** (R2.2, #228) — reshaped the post-`budi init` "Next steps" list around the real order users actually need: (1) restart-terminal prompt (previously buried in a trailing warning), (2) start coding, (3) `budi doctor` as the single end-to-end verifier, (4) `budi status` for a today-cost snapshot, (5) optional `budi import`. `budi status` now prints a friendly "no activity recorded today yet — open your agent and send a prompt" hint when the daemon is healthy but nothing has been recorded, instead of rendering an all-zero summary with no guidance. `budi doctor` now closes with a matching first-run nudge when the database has no assistant activity yet, so new users understand that "no attribution data" is expected on day zero rather than a setup failure. Install scripts (`scripts/install.sh`, `scripts/install-standalone.sh`, `scripts/install-standalone.ps1`) and the README's "First run checklist" now all point at `budi doctor` as the canonical "did setup really work?" command. No flag changes, no breaking changes to CLI output shape. Cross-surface local→cloud linking UX is deliberately left to #235 (R3) per ADR-0088 §6.

### Added

- **`budi doctor` surfaces Cursor-extension onboarding counters** (R2.4, #314) — reads the local-only `~/.local/share/budi/cursor-onboarding.json` v1 file written by the Cursor extension when it is used as an onboarding entry point (marketplace-first installs) and prints a one-line summary of welcome-view impressions, "Open Terminal" clicks, and completed `budi init` hand-offs. Silently skips when the file does not exist. Zero remote telemetry — the counters file stays local and contains only integer counts plus coarse ISO timestamps (ADR-0083 privacy limits preserved). The Cursor-side implementation lives in `siropkin/budi-cursor` v1.2.0. Public-site updates for the new extension-first acquisition funnel are threaded into #296.
- **`budi cloud sync` and `budi cloud status`** (R2.1, #225) — the pre-8.0 `budi sync` command was removed in #175 when transcript ingestion consolidated into `budi import`, leaving cloud sync running only as an async daemon worker (#101). The new `budi cloud` namespace restores a discoverable self-serve path: `budi cloud sync` pushes queued local rollups and session summaries to the cloud on demand and reports records upserted, watermark, and endpoint; `budi cloud status` reports whether sync is enabled, when it last succeeded, and how many records are queued locally. Both commands honor `--format text|json` in line with `budi stats` / `budi sessions`. Backed by new `POST /cloud/sync` (loopback-only) and `GET /cloud/status` daemon endpoints; a shared `cloud_syncing` `AtomicBool` prevents the manual command and the background worker from double-posting.
- **`budi doctor` sessions-visibility check** — reports assistant-messages vs returned-session counts for the `today`, `7d`, and `30d` windows and flags a hard error if any window has activity but zero returned sessions (#302).
- **`messages.timestamp` / `session_id` attribution contract** documented in `SOUL.md` so future providers cannot silently regress R1.0 (#302).
- **`BUDI_ANTHROPIC_UPSTREAM` / `BUDI_OPENAI_UPSTREAM` env overrides** on the proxy (mirroring the existing `BUDI_PROXY_PORT` / `BUDI_PROXY_ENABLED` pattern) so local end-to-end tests and air-gapped deployments can redirect proxy traffic without editing on-disk config.
- **Local end-to-end test harness** in `scripts/e2e/` — the first script, `test_302_sessions_visibility.sh`, boots a real `budi-daemon` + mock upstream + CLI against an isolated `$HOME` and pins the #302 fix. See `scripts/e2e/README.md` for conventions and the new "Local end-to-end tests" section in `SOUL.md`.

### Process

- **R1.6 code review pass for the 8.1 classification round** completed (#217). Audited the merged work from R1.0.1 (#302) through R1.5 (#293) for correctness, privacy, and explainability against ADR-0088 §5. No blocking defects; the round meets the 8.1 classification contract. Four non-blocking follow-ups filed for 8.2: cloud-sync ticket extractor should share the pipeline helper (#333), `proxy_events` schema missing first-class dimension columns (#334), defensive sibling-tag pairing at emission sites (#335), and R1.5 edge cases in `work_outcome` integration-branch detection and `tool_result` variant coverage (#336). Docs-drift items handed off to R1.7 (#220) in an issue comment.
- **R1.7 docs review pass for the 8.1 classification round** completed (#220). Picked up the two drift items flagged by R1.6: (1) `SOUL.md` activity-attribution doctor threshold now matches the code's `pct >= 99.9` (float-tolerant) instead of reading as a flat 100%; (2) the `pipeline/enrichers.rs` key-files entry now lists six enrichers (including R1.4 `FileEnricher`) and the `pipeline/mod.rs` entry calls out the cross-message tool-outcome correlation emitted after the per-message pass. `README.md` picked up the missing R1 surfaces: `budi stats --files` / `--file <path>`, the full R1.2–R1.5 tag vocabulary, the new `/analytics/tickets`, `/analytics/activities`, and `/analytics/files` endpoints, the `work outcome` row on the session detail view, and an explicit "disabled since 8.0" annotation on the Retry Loops health vital so the docs stop describing a vital the daemon no longer computes.

## 8.0.0 — 2026-04-16

Budi 8.0 is a ground-up rearchitecture: proxy-first live cost tracking replaces the old hook/OTEL/file-sync ingestion model, the Cursor extension and cloud dashboard are extracted into independent repos, and a new optional cloud layer gives managers team-wide AI cost visibility — all while keeping prompts, code, and responses strictly local.

### Proxy — real-time cost tracking

- **Local proxy server** on port 9878 transparently sits between AI agents and upstream providers (Anthropic, OpenAI), capturing every request in real time (#89)
- **Streaming pass-through** — SSE responses flow chunk-by-chunk with no visible lag; token metadata is extracted via tee/tap without modifying the stream (#90)
- **Proxy attribution** — each request is attributed to repo, branch, and ticket via `X-Budi-Repo`/`X-Budi-Branch`/`X-Budi-Cwd` headers or automatic git resolution (#91)
- **Cache token extraction** from proxy responses for accurate cost calculation (#192)
- **Provider normalization** — proxy events stored as `claude_code`/`codex`/`copilot_cli` instead of raw `anthropic`/`openai` for consistent analytics (#191)
- **Authorization header forwarding** for Anthropic OAuth sessions (#169)
- **Large payload resilience** — daemon avoids full JSON parse on oversized bodies to prevent crashes (#274)

### Auto-proxy-install — zero-config agent setup

- **`budi init` auto-configures proxy routing** for selected agents (#170):
  - CLI agents (Claude Code, Codex, Copilot): managed env-var block in shell profile (`~/.zshrc`, `~/.bashrc`)
  - Cursor: patches `settings.json` with proxy base URL
  - Codex Desktop: patches `~/.codex/config.toml`
- **`budi enable`/`budi disable`** toggle proxy configuration per agent
- **Shell restart warning** after enabling CLI agents (#188)
- **`budi launch <agent>`** remains available as explicit fallback; `BUDI_BYPASS=1` skips proxy for one session (#95)

### Cloud — optional team dashboard

- **Cloud ingest API** at `app.getbudi.dev` accepts pre-aggregated daily rollups and session summaries from the daemon (#100)
- **Async cloud sync worker** in the daemon with watermark tracking, exponential backoff, and idempotent UPSERT semantics (#101)
- **Cloud dashboard** at [app.getbudi.dev](https://app.getbudi.dev) — Overview, Team, Models, Repos, Sessions, Settings pages (#102)
- **Supabase Auth** (GitHub + Google + magic link) for web sign-in (ADR-0087 §4)
- **Privacy contract** — only numeric aggregates cross the wire; prompts, code, responses, file paths, and email never leave the machine (ADR-0083)
- **HTTPS-only** — daemon refuses to sync over plain HTTP
- Cloud sync is **disabled by default**; opt-in via `~/.config/budi/cloud.toml`

### Daemon autostart

- **Platform-native autostart** so the daemon survives reboots (#150):
  - macOS: launchd LaunchAgent
  - Linux: systemd user service
  - Windows: Task Scheduler
- **`budi autostart`** subcommand: `status`, `install`, `uninstall` (#187)
- `budi init` and `budi uninstall` manage the service automatically

### Multi-agent support

- **Codex Desktop/CLI transcript import** — historical backfill from `~/.codex/sessions/` (#178)
- **Copilot CLI transcript import** — historical backfill from `~/.copilot/session-state/` (#179)
- **Per-agent opt-in** — `budi init` prompts for each agent; choices stored in `agents.toml` (#85)
- **Provider filter** extended with `codex`, `copilot_cli`, `openai` (#257)
- **Model breakdown** shows provider alongside model name when duplicates exist across providers (#258)

### CLI improvements

- **Rich CLI is the primary local UX** — `budi stats`, `budi sessions`, `budi health`, `budi status` (#97)
- **`budi import`** consolidates historical transcript import (replaces removed `budi sync`) (#175)
- **Session ID** shown in `budi sessions` output with prefix matching for detail view (#174)
- **Total cost line** in multi-agent `budi stats` output (#184)
- **`budi doctor`** detects proxy env var configuration vs active shell state
- **`budi status`** quick overview of daemon, proxy, and today's cost

### Cursor extension

- Minimal bootstrap and status flow (#96)
- Extension extracted to [`siropkin/budi-cursor`](https://github.com/siropkin/budi-cursor) (#103)
- Installed via VS Code Marketplace or `budi integrations install --with cursor-extension`
- Checks daemon `api_version` on startup for compatibility

### Repo extraction

- **Cursor extension** extracted to [`siropkin/budi-cursor`](https://github.com/siropkin/budi-cursor) (#103)
- **Cloud dashboard** extracted to [`siropkin/budi-cloud`](https://github.com/siropkin/budi-cloud) (#103)
- No compile-time dependencies between repos; all communication over HTTP or JSON file contracts
- Version coordination via `api_version` (extension) and `schema_version` (cloud sync)

### Removed

- **Hook ingestion** (`budi hook`, `POST /hooks/ingest`, `hook_events` table) — replaced by the proxy (#92)
- **OTEL ingestion** (`POST /v1/logs`, `POST /v1/metrics`, `otel_events` table) — replaced by the proxy (#92)
- **MCP server** (`budi mcp-serve`) — replaced by proxy + Rich CLI (#84)
- **Starship integration** — replaced by the Rich CLI statusline (#84)
- **Local dashboard** (`/dashboard`) — replaced by cloud dashboard at `app.getbudi.dev` and Rich CLI (#103)
- **`budi sync`** command — consolidated into `budi import` (#175)
- **Deprecated integration names** no longer accepted in `--with`/`--without` CLI flags (#261)
- Database schema reset to v1 for clean 8.0 starting point (#92)

### Bug fixes

- Fix Cursor CLI discovery missing macOS app bundle path (#176)
- Fix daemon ERROR log spam for missing session health when no sessions exist (#177)
- Fix `budi launch codex` exit code when showing Codex Desktop instructions (#180)
- Fix misleading Cursor extension message in `budi init` (#181)
- Fix statusline hyperlink pointing to removed `/dashboard` (#259)
- Fix `budi import` help text to mention all 4 providers (#262)
- Fix `budi uninstall` description referencing removed hooks (#264)
- Fix subagent transcript parsing for accurate cost reporting (#205)
- Soften Cursor extension install warning (#185)
- Make Cursor watermark catch-up warning verbose-only (#186)
- Update `budi launch cursor` messaging to reflect auto-proxy-install (#183)

### Architecture decisions

- [ADR-0081](docs/adr/0081-product-contract-and-deprecation-policy.md) — surface disposition and deprecation policy
- [ADR-0082](docs/adr/0082-proxy-compatibility-matrix-and-gateway-contract.md) — proxy compatibility matrix and gateway contract
- [ADR-0083](docs/adr/0083-cloud-ingest-identity-and-privacy-contract.md) — cloud ingest, identity, and privacy contract
- [ADR-0086](docs/adr/0086-extraction-boundaries.md) — extraction boundaries for budi-cursor and budi-cloud
- [ADR-0087](docs/adr/0087-cloud-infrastructure-and-deployment.md) — cloud infrastructure, deployment, and domain strategy

### Breaking changes

All pre-8.0 releases were beta. 8.0.0 is the first stable release.

- Hook and OTEL ingestion removed with no migration path — the proxy replaces them
- `budi sync` removed — use `budi import` for historical data
- `budi mcp-serve` removed
- Starship integration removed — use `budi statusline` instead
- Local dashboard removed from daemon — use the cloud dashboard or Rich CLI
- Database schema reset to v1; existing pre-8.0 databases are dropped and recreated on upgrade
- The Cursor extension and cloud dashboard now live in separate repos
