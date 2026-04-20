# Changelog

## 8.2.1 — Unreleased

### Process

- **Tailer perf baseline harness for 8.2** (#410) — filed from the R3.5 release code review audit on `#360` finding F-4. The R3.5 audit reviewed the tailer's resource profile from code (single blocking worker, 500 ms debounce, 5 s backstop, no new HTTP listeners) but did not capture a live baseline; with `v8.2.0` shipped, the tailer is the only live ingestion path so future regressions in RSS / FD / CPU are only visible against a measured record. `scripts/e2e/test_410_tailer_baseline.sh` is the reproducible instrument: it stands up an isolated daemon against a `mktemp` `HOME` + `BUDI_HOME`, seeds empty watch roots for all four providers (so the tailer attaches a `notify` watcher per provider), runs a configurable idle soak (default 10 min at 30 s sample interval), replays a synthetic 100-event Claude Code session at 1 ms / event into one watch root while sampling RSS / CPU / FD at 100 ms granularity for a post-burst observation window, counts attached watchers by grepping the daemon log for the tailer's `watching provider=…` span, and prints a Markdown summary table suitable for pasting into the #410 baseline comment. Configurable via `SOAK_SECS`, `SAMPLE_EVERY`, `BURST_EVENTS`, `BURST_GAP_MS`, `BURST_WINDOW_SECS`, `BURST_SAMPLE_MS`; `KEEP_TMP=1` preserves the raw CSVs + daemon log for wiki archiving. Lives under `scripts/e2e/` (not `scripts/research/`, which would have required a `#321`-shaped justifying ticket per the `#407` carve-out) so it sits alongside the existing `test_328_release_smoke.sh` / `test_326_proxy_events_upgrade.sh` harnesses. Ticket closes on posted evidence, not on the harness landing — this entry pins the instrument, not the baseline.
- **Added `cargo-deny` supply-chain policy and CI check** (#409) — filed from the R3.5 release code review audit on `#360` finding F-3. The repo now ships a `deny.toml` pinning a permissive license allowlist (`MIT`, `Apache-2.0`, `Apache-2.0 WITH LLVM-exception`, `BSD-2-Clause`, `BSD-3-Clause`, `BSL-1.0`, `CC0-1.0`, `CDLA-Permissive-2.0`, `ISC`, `0BSD`, `Unicode-3.0`, `Unlicense`, `Zlib`), banning TLS backends that contradict our rustls-only posture (`openssl`, `openssl-sys`, `native-tls`), restricting crate sources to `crates.io`, denying unknown git sources, and treating wildcard version requirements on published registries as errors. CI runs `cargo deny check` on every PR via `EmbarkStudios/cargo-deny-action@v2`; the job is non-blocking for one release cycle before promotion to a required status check. Workspace members are marked `publish = false` to match reality (binaries ship via GitHub Releases, not crates.io) and to let the wildcard policy apply only to external dependencies. `CONTRIBUTING.md` documents the policy and the allowlist-update workflow. The existing inline `cargo audit` step in `.github/workflows/ci.yml` remains in place; the RustSec advisory DB is also consulted by `cargo deny check advisories`.
- **Retired the "R2.1 net-negative binary-size" framing in the 8.2 narrative** (#408) — the R3.5 release code review audit on #360 (finding F-2) showed that against `v8.1.0` on macOS arm64, `budi` grew +1.32 MB (+13.4%) and `budi-daemon` grew +0.22 MB (+1.8%) at the `v8.2.0` tag. The growth is intentional: R2.4 (#394) made `budi doctor` self-contained by opening the analytics DB directly, which pulled `rusqlite` with a bundled SQLite into the CLI binary for the first time, and proxy removal on the daemon side was offset by the `notify` family the tailer brought in. The `#322` R2.1 acceptance criterion "Diff stat shows a net-negative LOC change" was met in `git diff` terms, but the durable narrative in [ADR-0089](docs/adr/0089-reverse-proxy-first-jsonl-tailing-as-sole-live-path.md) §Consequences/Positive previously read "Code surface shrinks substantially. 8.2 R2.1 is a net-negative LOC release" and could be (and was) misread as implying a release-binary-size win. The ADR now reads "Source surface shrinks; binary size is roughly flat" with the measured per-binary deltas and the honest framing *"proxy runtime removed, replaced by a tailer of comparable size, plus a self-contained `doctor`."* The `v8.2.0` [GitHub release notes](https://github.com/siropkin/budi/releases/tag/v8.2.0) and `CHANGELOG.md` §8.2.0 never claimed a binary-size win and are unchanged. Retrospective framing correction noted on #316 and #322.
- **Docs / research discipline rule amended to permit operator-only measurement scripts** (#407) — the `8.2.1` rule "no new files under `docs/research/`, `docs/releases/`, or `scripts/research/`" (inherited from the `#316` epic body and restated on `#396`) now carries an explicit carve-out: an operator-only measurement script may live under `scripts/research/` when it is the explicit deliverable of a tracked ticket and its verdict is load-bearing for an ADR, release decision, or other durable record. Narrative output from running such a script still belongs in the wiki or a durable issue comment, not in `docs/research/`. The carve-out exists so `scripts/research/cursor_usage_api_lag.sh` (#321, [ADR-0089 §7](docs/adr/0089-reverse-proxy-first-jsonl-tailing-as-sole-live-path.md)) can stay in the repo as the durable in-tree artifact of the Cursor Usage API lag measurement without being treated as a rule exception in every subsequent audit pass. The rule wording on `#396` is updated to match; the broader `docs/research/` / `docs/releases/` prohibition is unchanged — `v8.2.1` release notes still live on the GitHub release page and research narrative still belongs in the wiki. `ADR-0089 §7` now points at this amendment so the cross-reference reflects the post-`8.2.0` state rather than the original `#316` rule 12 wording.

### Added

- **Relative `--period` / `-p` windows for `budi stats` and `budi sessions`** (#404) — in addition to the calendar windows (`today`, `week`, `month`, `all`), the CLI now accepts rolling windows of the form `Nd` / `Nw` / `Nm` where `N` is a positive integer (e.g. `budi stats -p 7d`, `budi sessions -p 2w`, `budi stats -p 3m --models`). Days and weeks subtract exactly that many local calendar days / weeks from today; months use calendar-month subtraction (clamped to the end of the target month, so `2026-03-31 - 1m = 2026-02-28`). This aligns the CLI time axis with the rolling `1d` / `7d` / `30d` windows used by the statusline surface and the cloud dashboard (ADR-0088 §4, #350). Parsing is UTF-8 safe, rejects zero (`0d` / `0w` / `0m`), rejects unknown units with an actionable error, and is case-insensitive on the unit suffix. `period_label` renders singular forms (`Last 1 day`, `Last 1 week`, `Last 1 month`) so the output never reads "Last 1 days". No wire-format changes — the daemon still consumes UTC RFC3339 `since` / `until` bounds.

### Changed

- **`budi health` renamed to `budi vitals`** (#367) — the old `budi health` verb overlapped too easily with `budi doctor` (daemon/install self-check). The session-vitals command is now `budi vitals` with identical output and the same `--session` flag. `budi health` keeps working in 8.2.x as a hidden backward-compatibility alias: the first invocation each UTC day prints a one-line stderr hint pointing users at `budi vitals`, subsequent invocations on the same day stay quiet. Slated for removal in 8.3. Help output, `after_help`, `README.md`, and `SOUL.md` all describe `budi vitals` as the canonical command.
- **`budi migrate` / `budi repair` / `budi import` moved under `budi db`** (#368) — closes the last R2.1 CLI layout outlier flagged in #225. The three DB admin verbs were the only surviving top-level bare verbs after the 8.1 `budi autostart` / `budi integrations` / `budi cloud` namespace work, and they all operate on the same analytics DB, so they now live under a single `budi db` namespace (`budi db migrate`, `budi db repair`, `budi db import`). The bare verbs still parse in 8.2.x as hidden backward-compatibility aliases: the first invocation each UTC day prints a one-line stderr hint pointing users at the new namespace, subsequent invocations on the same day stay quiet. Slated for removal in 8.3. `budi doctor` recovery hints, the `analytics schema` 503 error body surfaced by `GET /analytics/*` and `POST /sync`, the startup warning the daemon emits when the schema is stale, the empty-stats tip lines, `after_help`, `README.md`, and `SOUL.md` all describe the `budi db …` shape as the canonical form. The 503 wire contract (`needs_migration: true`, `current`, `target`) is unchanged; only the human-readable verb in the `error` field moved. No behavior change to the underlying migrate / repair / import implementations.

## 8.2.0 — 2026-04-19

8.2 is the "Invisible Budi" release: Budi is now invisible by default, reading agent transcripts directly from disk instead of intercepting network traffic.

### Added

- **`budi init --cleanup`** — A new command to explicitly remove legacy 8.1 injected configuration (shell profiles, editor settings) from your machine.

### Changed

- **JSONL tailing is the sole live path** — Budi now watches your agent's transcript files on disk instead of acting as a local proxy. This provides the exact same cost classification and privacy model, but with zero network interception and zero configuration changes to your tools. Latency is now measured in seconds rather than milliseconds. See [ADR-0089](docs/adr/0089-reverse-proxy-first-jsonl-tailing-as-sole-live-path.md) for the full rationale.
- **Removed proxy and wrapper UX** — `budi launch`, `budi enable <agent>`, and `budi disable <agent>` have been removed. The proxy binary, proxy port, and `ANTHROPIC_BASE_URL` / `OPENAI_BASE_URL` / `COPILOT_PROVIDER_BASE_URL` injections are gone.
- **Removed config mutation** — Budi no longer mutates your shell profile, Cursor `settings.json`, or `~/.codex/config.toml`.
- **`proxy_events` migration** — The obsolete `proxy_events` table is dropped on upgrade. Existing proxy-sourced `messages` rows remain read-only so your historical analytics stay intact. `budi doctor` will report this retained legacy state.

### Upgrade Checklist for 8.1 Users

1. Run `budi init --cleanup` to remove legacy proxy configuration from your shell and editors.
2. Restart your terminal.
3. Run `budi doctor` to verify the new tailer-based daemon is healthy.

## 8.1.0 — 2026-04-18

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

- **R4.2 smoke test plan for v8.1.0** drafted (#297). New [`docs/releases/8.1.0-smoke-tests.md`](docs/releases/8.1.0-smoke-tests.md) is the structured release-gate plan for v8.1.0 — it ports forward the 8.0 regression set from #280 where behavior changed, adds explicit per-test coverage for every user-visible 8.1 surface (R1.0 attribution bugs #302–#305, R1.4 file-level attribution #292, R1.5 tool / session outcomes #293, R2.1 CLI normalization + `budi cloud sync` / `status` #225, R2.2 onboarding #228, R2.3 statusline #224, R2.4 Cursor extension as onboarding entry point #314, R3.1 cloud dashboard windows + linking flow #235, R3.2 Cursor extension alignment #232), folds the four threaded comments from #297 into first-class test IDs (`ST-81-SV-01..04`, `ST-81-CX-01..04`, `ST-81-CL-01..05`), pins a 16-test minimum-viable pre-release set as the tag gate, defines the required PASS/FAIL comment shape for #297, and records #309's stale-schema disposition as a release-blocking check (503 + actionable error vs opaque 500). No code changes in this ticket — it is the release-gate plan itself. The hard release gates remain unchanged: #297 must close with a full PASS record and #296 must be merged in `siropkin/getbudi.dev` before #202 (R4.3) may tag v8.1.0. Governed by ADR-0088 §3.
- **R4.1 release readiness for v8.1.0** drafted (#230). New [`docs/releases/8.1.0.md`](docs/releases/8.1.0.md) is the single place where the tag-blocking checklist lives and where the GitHub release notes are drafted before #202 runs. It records the full roadmap closure (R1.0 through R3 review passes) with issue numbers, the validation matrix for the four repos in the ecosystem, the explicit release artifact checklist, the privacy re-check against ADR-0083 now that file-level attribution (#292) and tool / session outcomes (#293) have shipped, and the explicit deferrals into 8.2 (#316 / #294, with 8.3 absorbing the broader-agent coverage per ADR-0089) and 9.0 (#159). It also calls out #309 (opaque 500 on stale analytics schema) as the single known-open bug in the `8.1.0` milestone at R4.1 drafting time and documents the disposition rule: covered by the R4.2 smoke run (#297); if the smoke run reproduces it, it blocks v8.1.0 because it contradicts the R2.2 first-run-trust promise, otherwise it moves to 8.2. No code changes in this ticket — it is release-readiness documentation. Governed by ADR-0088 §3. The hard release gates (R4.2 #297 PASS record and #296 merged in `siropkin/getbudi.dev`) are still required before #202 tags.
- **R1.6 code review pass for the 8.1 classification round** completed (#217). Audited the merged work from R1.0.1 (#302) through R1.5 (#293) for correctness, privacy, and explainability against ADR-0088 §5. No blocking defects; the round meets the 8.1 classification contract. Four non-blocking follow-ups filed for 8.2: cloud-sync ticket extractor should share the pipeline helper (#333), `proxy_events` schema missing first-class dimension columns (#334), defensive sibling-tag pairing at emission sites (#335), and R1.5 edge cases in `work_outcome` integration-branch detection and `tool_result` variant coverage (#336). Docs-drift items handed off to R1.7 (#220) in an issue comment.
- **R1.7 docs review pass for the 8.1 classification round** completed (#220). Picked up the two drift items flagged by R1.6: (1) `SOUL.md` activity-attribution doctor threshold now matches the code's `pct >= 99.9` (float-tolerant) instead of reading as a flat 100%; (2) the `pipeline/enrichers.rs` key-files entry now lists six enrichers (including R1.4 `FileEnricher`) and the `pipeline/mod.rs` entry calls out the cross-message tool-outcome correlation emitted after the per-message pass. `README.md` picked up the missing R1 surfaces: `budi stats --files` / `--file <path>`, the full R1.2–R1.5 tag vocabulary, the new `/analytics/tickets`, `/analytics/activities`, and `/analytics/files` endpoints, the `work outcome` row on the session detail view, and an explicit "disabled since 8.0" annotation on the Retry Loops health vital so the docs stop describing a vital the daemon no longer computes.
- **R2.6 docs review pass for the 8.1 local UX round** completed (#229). Audited `README.md`, `SOUL.md`, `CONTRIBUTING.md`, `docs/design-principles.md`, `docs/statusline-contract.md`, and `scripts/e2e/README.md` against the R2 deliverables (R2.1 #225 CLI normalization, R2.2 #228 onboarding / first-run, R2.3 #224 simplified statusline, R2.4 #314 Cursor extension as onboarding entry point) and ADR-0088 §4/§6. Fixed three drift items inline: (1) `CONTRIBUTING.md` "Adding a new enricher" now documents the correct six-stage `Identity → Git → Tool → File → Cost → Tag` order (R1.4 #292 added `FileEnricher`; previous docs still showed the five-stage 8.0 order); (2) `SOUL.md` local end-to-end tests example now enumerates all five shipped regression scripts (`test_221`, `test_222`, `test_224`, `test_302`, `test_303`) instead of just #302/#303; (3) `README.md` Cursor extension section now explicitly describes the R2.4 first-run onboarding entry point (welcome view, `budi init` hand-off, local-only counters surfaced by `budi doctor`) so marketplace-first users discover the correct acquisition path from the top-level docs. No blocking code changes required — the R2 shipped surfaces match ADR-0088 §4 (quiet, rolling, provider-scoped statusline) and §6 (local-only onboarding; cross-surface linking deferred to #235). Public-site sync (`#296` in `getbudi.dev`) picks up the Cursor extension onboarding entry-point narrative; no additional public-site deltas from R2.1–R2.3 that were not already threaded.
- **R3.4 docs review pass for the 8.1 surface alignment round** completed (#236). Audited `README.md`, `SOUL.md`, `CONTRIBUTING.md`, `docs/design-principles.md`, `docs/statusline-contract.md` in this repo plus `README.md` / `SOUL.md` in `siropkin/budi-cursor` and `siropkin/budi-cloud` against the merged R3 surfaces (R3.1 cloud dashboard window contract + linking UX in #235 / `siropkin/budi-cloud#21`, R3.2 Cursor extension statusline-only simplification in #232 / `siropkin/budi-cursor#5`) and ADR-0088 §7. Fixed drift inline: (1) `README.md` "What it does" / Ecosystem / Cursor extension section / Troubleshooting / Session health sections no longer describe the Cursor extension as a "live status bar and health panel" with a "session list and vitals / tips panel" — the extension is now documented as the statusline-only v1.1.0 surface that mirrors the Claude Code statusline byte-for-byte with `?provider=cursor` scoping; (2) `SOUL.md` product-layout blurb for the Cursor extension matches the v1.1.0 / v1.2.0 shipped reality (single status bar item, byte-for-byte match with Claude Code statusline, `MIN_API_VERSION = 1`, unchanged onboarding entry point); (3) `siropkin/budi-cursor` `SOUL.md` one-line summary no longer mentions the removed "side panel" (closes the non-blocking R3.3 follow-up `siropkin/budi-cursor#7`); (4) `siropkin/budi-cloud` `SOUL.md` now documents the `1d` / `7d` / `30d` window contract, the `not_linked` / `linked_no_data` / `ok` / `stalled` freshness states, and the `LinkDaemonBanner` / `FirstSyncInProgressBanner` linking flow so the cloud repo's canonical agent doc reflects shipped R3.1 behavior. No blocking code changes required; the shared provider-scoped status contract in `docs/statusline-contract.md` and the existing provider consumers already match reality. Public-site sync threading is already complete in #296 (R3.1 and R3.2 follow-ups previously filed). No new public-site deltas surfaced by this pass.
- **R3.3 code review pass for the 8.1 surface alignment round** completed (#231). Audited the merged R3 work — R3.1 cloud dashboard alignment (#235, `siropkin/budi-cloud#21`) and R3.2 Cursor extension simplification (#232, `siropkin/budi-cursor#5`) — against ADR-0088 §7 and the shared provider-scoped status contract in `docs/statusline-contract.md`. Verified that the Cursor extension's numeric format matches `format_cost` in `crates/budi-cli/src/commands/mod.rs` byte-for-byte, `MIN_API_VERSION = 1` aligns with the daemon's `API_VERSION = 1`, the cloud dashboard adopts the `1d` / `7d` / `30d` window contract, and the sync-freshness indicator cleanly distinguishes `not_linked` / `linked_no_data` / `ok` / `stalled`. Public-site follow-ups for both R3.1 and R3.2 are already threaded into #296. All downstream validation passes cleanly: `budi-cursor` lint / format:check / vitest (56 passed) / build, and `budi-cloud` lint (1 pre-existing warning unrelated) / vitest (28 passed) / build. No blocking defects. Three non-blocking follow-ups filed: `budi-cursor` `SOUL.md` one-line summary still mentions the removed side panel (`siropkin/budi-cursor#7`); `budi-cloud` `getSyncFreshness` uses `.single()` on a `daily_rollups.synced_at` lookup that legitimately returns zero rows in the linked-but-no-data state and logs `PGRST116` noise (`siropkin/budi-cloud#22`); and a README / docs ticket to explain the rolling-vs-calendar window split between the statusline and the dashboard (#350, 8.2 scope).
- **R2.5 code review pass for the 8.1 local UX round** completed (#223). Audited the merged work from R2.1 (#225, PR #339), R2.2 (#228, PR #340), R2.3 (#224, PR #341), and the Cursor side of R2.4 (#314, budi-cursor PR #6) against ADR-0088 §4–§6: CLI normalization, first-run UX, simplified statusline, and extension-first onboarding. No blocking defects; Rust validation (`cargo fmt`, `cargo clippy --locked -D warnings`, `cargo test --workspace --locked`) passes cleanly. The R2.4 `budi doctor` counters companion (PR #342) is open and is the one remaining blocker before #223 can close. Five non-blocking follow-ups filed for 8.2: cloud-sync worker panic flag leak (#343), `/cloud/status` rebuilds full sync envelope on every call (#344), statusline legacy slot tokens silently render rolling values in custom templates (#345), `CloudSyncStatus::configured` has a redundant `effective_api_key` check (#346), and `branch_cost` aggregates across all repos with the same branch name (#347).

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

- **Rich CLI is the primary local UX** — `budi stats`, `budi sessions`, `budi health` (renamed to `budi vitals` in 8.2.1 — #367), `budi status` (#97)
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
