# Changelog

## 8.0.0 — 2026-04-XX

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
