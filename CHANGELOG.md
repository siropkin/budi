# Changelog

## 8.4.8 — 2026-05-12

8.4.8 is the second same-day hotfix on top of v8.4.6 / v8.4.7. The v8.4.7 fix combined the Xodus `projectName` extraction with Nitrite per-turn extraction, but the populated-entity probe still skipped session-dirs whose `.xd` carried only the `projectName` property (no `Xd*Session` marker) and whose Nitrite store used the older `copilot-edit-sessions-nitrite.db` filename — without the `chat-` prefix `NITRITE_DB_FILES` was looking for. Result: one of the three resolvable-on-disk JetBrains sessions (Verkada-Backend) stayed at `repo_id = NULL` even after the v8.4.7 dual-store fix. `api_version` stays at `3`.

### Fixed

- **JetBrains Copilot: recognize `copilot-edit-sessions-nitrite.db` (no `chat-` prefix)** (#766) — older plugin builds wrote the edit-session Nitrite store under the shorter filename. The post-release smoke test on v8.4.7 caught this when session `32REEy.../ic/chat-edit-sessions/` (Verkada-Backend) emitted zero rows even though its `.xd` carried `projectName=Verkada-Backend`. Added the legacy filename to `NITRITE_DB_FILES` so the populated-entity probe accepts it.

### Cross-repo lockstep

- No cross-repo changes required.

## 8.4.7 — 2026-05-12

8.4.7 is a same-day hotfix for the v8.4.6 JetBrains parser. The 8.4.6 implementation treated the Xodus `.xd` log and the Nitrite `.db` store as mutually exclusive — the parser ran `extract_xodus_project_name` only when the populated-entity probe returned `.xd`, and `extract_nitrite_turn_ids` only when it returned a Nitrite file. Real dual-store session-dirs (the common shape post-migration) put `projectName` in `.xd` and `Nt*Turn` documents in `.nitrite.db`; the 8.4.6 code picked one and dropped the other, so every `surface=jetbrains` row landed with `repo_id = NULL` even when the .xd carried a clean `Verkada-Web`-style name. The post-release smoke test on a real DB caught this within minutes — 0 of 23 sessions populated `repo_id`. The parser now reads `00000000000.xd` and every `*.nitrite.db` in the session-dir independently and merges the results: per-turn UUIDs from Nitrite + `repo_id` / `git_branch` / `session_title` from Xodus land on the same `ParsedMessage`. `api_version` stays at `3`.

### Fixed

- **JetBrains Copilot: combine Xodus projectName and Nitrite per-turn extraction on dual-store session-dirs** (#766, #764) — `parse_session_dir` previously ran `populated_store_in` and then used its single return value to decide between the two extraction paths. The result was that every dual-store agent-session (Xodus carrying the populated-entity marker, Nitrite carrying the per-turn documents) emitted either a one-row placeholder with repo enrichment *or* a per-turn batch without it, never both. Now the parser reads each store unconditionally — `.xd` for `projectName`, each `*.nitrite.db` for `Nt(Agent|Edit)?Turn` UUIDs — and merges the results onto every emitted row. New regression test `dual_store_session_combines_xodus_repo_with_nitrite_turns` covers the wire shape.

### Cross-repo lockstep

- No cross-repo changes required.

## 8.4.6 — 2026-05-11

8.4.6 is the patch that closes the JetBrains-as-first-class-host story for real. v8.4.3 said JetBrains was a first-class surface; v8.4.5 fixed the parser bail-out that left every Nitrite-only session emitting zero rows. The v8.4.5 post-release smoke test then surfaced the remaining four cuts — and this release lands all four. The headline fix is **per-turn rows on JetBrains**: the Nitrite parser previously emitted one assistant-role placeholder per session, so fresh prompts inside an existing session never materialized as new rows; the parser now byte-walks `Nt(Agent|Edit)?Turn` document boundaries and emits one row per turn UUID. Sibling fixes light the rest of the dashboard up: the Billing API reconciler now dollarizes zero-token JetBrains placeholders evenly across the bucket's rows (pre-#765 it short-circuited on zero existing-sum and left `cost_30d` for `surface=jetbrains` permanently at \$0); the dashboard's Repo column finally renders real project names for JetBrains sessions thanks to a Xodus-log `projectName` byte-scan; and on every upgrade across a wire-shape change boundary, the daemon now resets its cloud-sync watermark automatically so historical rows re-upload under the new shape — no more `POST /cloud/reset` as undocumented institutional knowledge. `api_version` stays at `3`.

### Fixed

- **JetBrains Copilot: emit one row per Nitrite turn** (#764, #771) — `parse_session_dir` previously emitted a single `assistant`-role placeholder keyed on `(session_id, path)`, so subsequent ingests of the same session collided with the existing UUID and `INSERT OR IGNORE` dropped every fresh prompt. The new `extract_nitrite_turn_ids` byte-walker scans the on-disk `.nitrite.db` for `Nt(Agent|Edit)?Turn` class markers, pairs each with the first `t\x00\x04uuid t\x00\x24<36-byte-uuid>` field within an 8 KB window forward, and dedupes (Nitrite's MVStore writes class metadata + B-tree leaf entries for the same document so the same turn surfaces multiple times). One `ParsedMessage` per distinct turn UUID keyed via a new `deterministic_uuid_from_nitrite(turn_id, path)` helper. Sessions whose Nitrite store carries no recoverable turn UUIDs (e.g. an `NtAgentSession` with no `NtAgentTurn` yet) fall back to the pre-#764 one-row-per-session placeholder so #757's existence-marker path still emits *something*. Phase 1 only — per-turn `createdAt` / `modelName` / `stringContent` extraction (full MVStore + Java-serialization decoder) deferred to a future ADR amendment.
- **JetBrains Copilot: dollarize zero-token placeholders via Billing API** (#765, #770) — `apply_buckets` in `crate::sync::copilot_chat_billing` short-circuited on `SUM(existing_cost) = 0`. Combined with the pre-#764 one-row-per-session shape (always zero tokens, always zero cost), this meant the Billing API dollar truth never reached JetBrains rows and the statusline read `\$0.00` for `surface=jetbrains` even after a successful reconciliation tick. The worker now reads `COUNT(*)` alongside the sum: when sum is zero but row_count > 0 and the billing dollar amount > 0, distribute evenly across the placeholder rows and tag them `cost_confidence='estimated'` + `pricing_source='billing_api:copilot_chat'`. Empty buckets and zero-amount buckets still skip cleanly.
- **JetBrains Copilot: extract projectName from Xodus log so the dashboard Repo column populates** (#766, #769) — every `surface=jetbrains` session rendered `Repo: (unknown)` because `parse_session_dir` never read the IntelliJ project name from the `.xd` log's `XdChatSession.projectName` property. Phase 1 lands the byte-scan: read the property ID from the schema header (`projectName\x00<id>`), then walk forward through every `\x82\x00<id>\x82<value>\x00` value record until one passes the file-name false-positive filter. On hit, probe `~/_projects/<name>`, `~/projects/<name>`, `~/<name>` for a matching git checkout; populate `repo_id` via `repo_id::resolve_repo_id` and `git_branch` from `.git/HEAD`. On miss, still set `session_title` to the raw `projectName` so the dashboard renders something useful. Phase 2 (Nitrite `NtAgentWorkingSetItem.stringContent` extraction) is deferred and pairs with #764's MVStore decoder.
- **Cloud sync: re-upload history under new wire shape on binary upgrade** (#767, #768) — `__budi_cloud_sync__` / `__budi_cloud_sync_sessions__` tracked timestamps but not the shape of the rows they covered, so when `surface` joined the wire (8.4.3) every session uploaded before the upgrade stayed frozen on the cloud under the pre-surface shape. The v8.4.5 smoke test surfaced this as "22 of 23 historical JetBrains sessions missing from the `?surface=jetbrains` dashboard view until a manual `POST /cloud/reset`." Two new columns (`sessions.wire_shape_version`, `message_rollups_daily.wire_shape_version`, default `1`) plus `WIRE_SHAPE_VERSION_{SESSIONS,ROLLUPS}` constants in the daemon binary (`2` for the surface shape). On boot, `reset_stale_shape_watermarks` compares each table's max local version against the binary's expected value; drift drops the matching watermark and bulk-bumps every row's version to the binary's expected value so the next boot is a no-op. Logged at INFO so on-call can correlate the dashboard reflow with the upgrade event.

### Cross-repo lockstep

- No cross-repo changes required. Every fix is local-side; the cloud's accepted `schema_version` set, the budi-jetbrains widget contract, and the dashboard's wire shape are all unchanged. The #767 boot reset triggers one extra re-upload tick after the upgrade (the daemon log line `wire-shape upgrade detected; dropped cloud watermark(s) to force re-upload` calls out exactly what happened); cloud-side dedup makes the re-upload safe even when records overlap with what the cloud already has.

## 8.4.5 — 2026-05-11

8.4.5 is a follow-up patch on top of 8.4.4 that finishes the v8.4.3 JetBrains-as-first-class-host story and tightens two diagnostic paths the v8.4.4 smoke test exposed. The headline fix is **the JetBrains Copilot Chat parser**: post-migration sessions persist to `copilot-chat-nitrite.db` and skip the Xodus `.xd` log entirely, so the parser bailed on `.xd not found` and the `surface=jetbrains` rollup stayed at \$0.00 for every fresh JetBrains user. The parser now accepts either store as the existence marker. Two diagnostic-quality fixes ride along: cloud-sync 422s now surface the server's actual body verbatim (the v8.4.4 smoke test wasted an hour decoding a "schema mismatch (422): Server returned 422" daemon log that was actually a per-field validation failure from the cloud), and JetBrains Copilot watch roots are now correctly routed to `surface=jetbrains` in `/health/sources` instead of falling into `surface=unknown`. One narrow correctness tighten-up: the `messages` INSERT path was leaving `cost_cents_effective` NULL on every `role=user` row (ADR-0094 §1 says it's never legitimately NULL); the schema and the bind now enforce the invariant. `api_version` stays at `3` — every fix is wire-shape-compatible with 8.4.4 daemons and clients.

### Fixed

- **JetBrains Copilot Chat: parse Nitrite-only sessions** (#757, #762) — post-migration sessions write only `copilot-chat-nitrite.db` (and `copilot-agent-sessions-nitrite.db` / `copilot-chat-edit-sessions-nitrite.db` for the agent and edit variants) and skip the Xodus log. The existence probe now accepts either store: `00000000000.xd` first (legacy sessions parse the same way they used to), then any of the `*.nitrite.db` files. The Nitrite scan looks for `Nt*Session` / `Nt*Turn` class-name suffixes in the MVStore catalog and emits one assistant-role row per populated session, matching the existing Xodus behavior. `NtSelectedModel` is deliberately *not* treated as a populated marker — empty chat tabs persist that record on open, and counting them would synthesize fake assistant turns. Token attribution still flows through the GitHub Billing API reconciliation per ADR-0093 §5; this fix closes the regression where Nitrite-only sessions emitted zero rows at all. ADR-0093 amended with the dual-store probe contract and the marker set.
- **Cloud sync: surface real 422 body, classify schema mismatches** (#756, #760) — the 422 arm in `send_sync_envelope` mapped every cloud rejection to `SyncResult::SchemaMismatch("Server returned 422")` with the body discarded; the daemon log read `schema mismatch (422): Server returned 422` and the CLI told the user to "update budi" even when budi was the latest tag and the cloud was the lagging side (the exact failure mode flagged in #749's body). The agent now disables `ureq`'s `http_status_as_error` so the response body reaches us on non-2xx (mirroring the post-#751 team-pricing pattern), and `SyncResult::SchemaMismatch(String)` is promoted to `SchemaMismatch(SchemaMismatch { body, kind })` with three kinds: `ClientTooOld` (the only path "update budi" is correct), `CloudTooOld` (call out the cloud as the lagging side, not budi), and `NotSchemaRelated` (per-field validation errors like `cost_cents must be a finite, non-negative number` — surface the body verbatim and skip the version advice).
- **JetBrains Copilot Chat: surface routing for `~/.config/github-copilot/` paths** (#758, #761) — `/health/sources` placed every JetBrains-side Copilot watch root (`~/.config/github-copilot/<ide-slug>/{chat-sessions,chat-agent-sessions,chat-edit-sessions,bg-agent-sessions}`) under `surface=unknown`. With #757 fixed and rows flowing, they'd land tagged `surface=unknown` and stay invisible to the budi-jetbrains 0.1.2 widget (which queries `?surface=jetbrains` per #748's contract). `infer_copilot_chat_surface` now treats any path containing a `github-copilot` segment as `surface=jetbrains`. Matches the segment instead of an IDE-slug allowlist so new JetBrains short codes (`pc`, `go`, `rr`, …) don't keep regressing.
- **Schema: `cost_cents_effective` is `REAL NOT NULL DEFAULT 0`** (#755, #759) — the tailer's INSERT bound `Option<f64>` directly to `cost_cents_effective`, so every `role=user` row (no LLM spend) wrote a NULL into the column even after the v8.4.4 dual-cost migration. ADR-0094 §1 declares the column is never legitimately NULL; the recompute worker healed the NULLs every hour but the column drift was real and visible to anyone running `SELECT SUM(cost_cents_effective IS NULL) FROM messages`. Two changes: the schema declares `NOT NULL DEFAULT 0` on fresh installs (any explicit NULL bind is now rejected at the SQLite layer), and the ingest bind mirrors the existing `_ingested` path (`cost_cents.unwrap_or(0.0)`). The #750 defense-in-depth `IS NULL` heal in `recompute_messages` is retained for legacy DBs that upgraded past the broken rename before this fix.

### Cross-repo lockstep

- No cross-repo changes required. Every fix is local-side; the cloud's accepted `schema_version` set and the budi-jetbrains widget contract are both unchanged.

## 8.4.4 — 2026-05-11

8.4.4 is a hotfix patch on top of 8.4.3 — the team-pricing + JetBrains release shipped five orthogonal regressions that the post-tag smoke test surfaced within minutes of v8.4.3 landing on `app.getbudi.dev`. Three of the five paralyzed the end-to-end team-pricing flow that 8.4.3 was supposed to ship (the recompute CLI 400'd, the hourly worker crashed on every 304 after the first install, and the migration left history with NULL `cost_cents_effective` that the recompute then panicked on); one paralyzed *every* cloud-connected user's daily/session sync (`schema_version: 2` was bumped on the daemon without widening the cloud's accepted set, so the budi-cloud ingest rejected every v8.4.3 envelope with HTTP 422); and one left every JetBrains user's status-bar widget showing the global cross-IDE rollup instead of their own host's spend (the `?surface=` filter never made it to `/analytics/statusline`). `api_version` stays at `3` — every fix is wire-shape-compatible with 8.4.3 daemons and clients.

If you're on 8.4.3, upgrade — the local-only paths still work but anything that touches the cloud (sync, team pricing, JetBrains status bar) is broken.

### Fixed

- **Cloud sync: schema_version v1 + v2 both accepted** (siropkin/budi-cloud#253, #749) — #741 bumped the daemon's `schema_version` 1 → 2 to signal that `surface` is now part of the wire shape, but the cloud's strict-equals validator was never widened. Every v8.4.3 daemon's sync got back HTTP 422 `Unsupported schema_version: 2. Expected 1.` with a misleading "update budi" hint. Cloud now accepts both versions; the daemon needs no change.
- **Team pricing: migration backfills NULL `cost_cents_effective`** (#746, #750) — the original #730 rename preserved legacy NULL rows into the new column, then `recompute_messages` crashed reading `r.get::<f64>(8)?` and the worker's no-pricing SQL path silently no-op'd (`!=` is NULL-propagating). For real users on the local mirror this meant the hourly worker stayed a no-op against any DB with pre-pricing-manifest history (i.e. anyone who's been on budi for more than a couple of weeks). Reconcile now backfills `_effective := _ingested` on every run (idempotent — heals existing DBs without a re-migrate), the per-row recompute reads `Option<f64>` and treats NULL as "always rewrite", and the no-pricing fast path's `WHERE` clause covers NULL rows.
- **Team pricing: worker treats `Ok(304)` as Unchanged** (#747, #751) — `ureq`'s `call()` in the pinned version returns `Ok(response)` for a 304 (it's not a 4xx/5xx), so after the first successful install the worker fell through to `read_json()` on the empty body and surfaced `parse team-pricing response: EOF while parsing a value at line 1 column 0` on every hourly tick. The route now short-circuits on `response.status() == 304` before touching the body. The existing `Err(StatusCode(304))` arm stays as defense-in-depth.
- **CLI: `budi pricing recompute` sends `force=true|false`, not `force=0|1`** (#745, #752) — the daemon's `RecomputeQuery` uses serde's strict bool deserializer (only `"true"` / `"false"`), but #743 shipped numeric `0`/`1` from the CLI. Every `budi pricing recompute` invocation got back 400 with a serde error — the only documented manual escape hatch between hourly worker ticks was broken. Two new daemon-side tests pin the wire shape so the regression can't recur.
- **`/analytics/statusline` honors `?surface=`** (#748, #753) — the original surface rollout (#702) retrofitted `/analytics/messages`, `/analytics/sessions`, and the breakdown routes but missed `/analytics/statusline`. Every `?surface=…` value silently returned the global rollup, so the JetBrains widget shipped in siropkin/budi-jetbrains 0.1.2 showed total cross-IDE spend instead of `surface=jetbrains` spend. `StatuslineParams` now takes a `surface` field (comma-separated, same shape as `provider`); the filter wires through every numeric field plus `active_provider`, including the daily/hourly rollup fast path so surface-scoped queries don't fall back to a messages-table scan.

### Cross-repo lockstep

- **`siropkin/budi-cloud`** — siropkin/budi-cloud#253 widens `SUPPORTED_SCHEMA_VERSIONS` to `{1, 2}` on the v1 ingest route. Deployed in lockstep with this tag so v8.4.3 daemons stop emitting 422s the moment cloud goes green; v8.4.4 daemons remain forward-compatible with the same shape.
- **`siropkin/budi-jetbrains`** — no plugin change required. The 0.1.2 status-bar widget already sends `?surface=jetbrains` against `/analytics/statusline`; the daemon side just had to start honoring it.

## 8.4.3 — 2026-05-11

8.4.3 closes two stories that 8.4.2 only set the table for: **JetBrains is a first-class host** (two new provider parsers — JetBrains AI Assistant and Copilot for JetBrains — finally materialize rows for the `surface=jetbrains` placeholder shipped in #712), and **negotiated team pricing now flows end-to-end** (ADR-0094 ships in full: the local daemon polls `GET /v1/pricing/active` on a 1h cadence, hot-swaps an in-memory price list, and recomputes `messages.cost_cents_effective` from the org's sale prices — `budi stats` and the cloud dashboard now show the same dollar amount by construction whenever an org has uploaded a list). The data plumbing needed to make both stories land also went in: `messages` / `message_rollups_hourly` / `message_rollups_daily` gained the `cost_cents_ingested` + `cost_cents_effective` column pair (mirroring the cloud-side dual-cost schema from budi-cloud #231), the `cloud_sync` daily-rollup + session-summary wire structs now carry `surface` so the dimension actually crosses the wire, and the daemon exposes a new `GET /health/sources` endpoint that the budi-jetbrains plugin uses to discover which paths the tailer is currently watching for its host. Two new ADRs land: ADR-0093 pins the JetBrains Copilot Chat on-disk storage shape (companion to ADR-0092 for the VS Code host), and ADR-0094 records the full custom-team-pricing + effective-cost-recalculation contract that drives the local mirror. `api_version` stays at `3` — none of this changes wire shape for existing endpoints; both the new `/health/sources` endpoint and the dual-cost columns are additive.

### Added

- **Provider: JetBrains AI Assistant** (#736, #738) — new `jetbrains_ai_assistant` provider for JetBrains' own Anthropic-backed assistant (plugin `com.intellij.ml.llm`), distinct from Copilot for JetBrains. Anthropic-style JSONL parsing keyed off `message_stop` for finalized `usage` tokens (including cache creation + cache read). Discovery enumerates per-product/year `<Product><Year>.<x>/log/llm-chat-history/` directories under the JetBrains config root. Rows land with `surface=jetbrains`.
- **Provider: Copilot for JetBrains chat-storage parser** (#722, #739) — wires the JetBrains host into the existing `copilot_chat` provider so `surface=jetbrains` rows from Copilot for JetBrains land in the DB and budi-jetbrains' status-bar plugin stops rendering `$0.00` against active sessions. New `providers/copilot_chat/jetbrains.rs` discovers session dirs under `~/.config/github-copilot/<ide-slug>/sessions/`.
- **Team pricing: local mirror worker** (#731, #742) — new `pricing/team` module + `team_pricing` worker. Daemon polls `GET /v1/pricing/active` every 1h (configurable via `BUDI_TEAM_PRICING_REFRESH_SECS`, gated by the same `BUDI_PRICING_REFRESH=0` opt-out as the LiteLLM refresher), hot-swaps the in-memory `Option<TeamPricing>` slot via `RwLock`, persists a local cache at `~/.local/share/budi/team-pricing.json`, and rewrites every `messages.cost_cents_effective` when `list_version` bumps. `304` → no-op; `404` → clear cache + reset `_effective := _ingested`; `401` → log warn + stop polling; `5xx`/network → retry next tick. Per-row resolve uses trailing-`*` glob model matching with region match against the row or org default, with `_effective` falling back to `_ingested` per row when any token type is unpriced (ADR-0094 §7 coalesce rule). One audit row per install/clear pass into the new `recalculation_runs_local` table.
- **Team pricing: `budi pricing status` surfaces team-pricing layer** (#732, #743) — `GET /pricing/status` now attaches a `team_pricing` object (org, list version, effective dates, defaults, latest local recompute audit row, 30-day savings). Key is always present (`active: false` for no-cloud-config and no-active-list paths) so JSON consumers can probe a single field. `budi pricing status` (text) renders a new `Team pricing (cloud)` section after the existing `Pricing manifest` block.
- **Team pricing: `budi pricing recompute [--force]`** (#732, #743) — new CLI subcommand backed by `POST /pricing/recompute`. Triggers an immediate re-poll + recompute pass. Without `--force` it short-circuits when `list_version` is unchanged; with `--force` it always runs the recompute against the currently-installed list. Useful for support cases.
- **Schema: dual cost columns on `messages`** (#730, #737) — `cost_cents_ingested` + `cost_cents_effective` column pair on `messages`, `message_rollups_hourly`, and `message_rollups_daily`, mirroring the cloud-side pattern from budi-cloud #231. `cost_cents_ingested` is what Budi computed at ingest via `pricing::lookup` (ADR-0091) and is never overwritten after insert (ADR-0091 §5 history-is-honest contract); `cost_cents_effective` is what every surface displays and gets rewritten by the team-pricing worker. Migration is additive — existing v1 DBs pick up the new columns and backfill `_effective := _ingested` everywhere on next migrate, so users with no cloud config see zero change.
- **Daemon `GET /health/sources`** (#735, #740) — new HTTP endpoint returning the on-disk paths the tailer is currently watching. Optional `?surface=<id>` filter; unfiltered form returns every surface grouped under `{ "surfaces": [ { "surface", "paths" }, ... ] }` (sorted + deduped). Contract matches what the budi-jetbrains plugin already speaks (siropkin/budi-jetbrains#36) — host extensions use this to show users "I am tracking N paths for your host" without each extension having to re-derive the discovery logic.
- **`cloud_sync`: `surface` on wire structs** (#723, #741) — `pub surface: String` field on `DailyRollupRecord` / `SessionSummaryRecord`, so the surface dimension landed in #701 / #712 actually crosses the wire to budi-cloud instead of being dropped between the local DB and the POST. `fetch_daily_rollups` / `fetch_session_summaries` now project `surface` from `message_rollups_daily` and `sessions`; new `map_rollup_row` helper mirrors the existing session-row mapping for symmetry.

### Architecture decisions

- **ADR-0093: JetBrains Copilot Chat on-disk storage** (#716, #721) — pins the storage shape for the JetBrains host as the companion to ADR-0092 for VS Code. Captures a redacted fixture in-tree, anchors the next parser ticket with an `unimplemented!()` stub, and locks in `xd.lck`-redaction tests so the lockfile-name secret doesn't leak into the fixture.
- **ADR-0094: Custom team pricing & effective-cost recalculation** (#725, #734) — records the full contract for team-pricing in budi-cloud and the local mirror. The shape, in one bullet: split every cost column into `cost_cents_ingested` (LiteLLM-priced at ingest, immutable per ADR-0091 §5) and `cost_cents_effective` (what every surface displays, recomputable from the org's price list). Cloud is the authoring surface; local mirrors via pull on a 1h cadence. Both sides run the same resolve algorithm, so cost matches by construction. Implementations: cloud (budi-cloud #231–#234 + #251), local (#730 schema, #731 worker, #732 CLI), polish (budi-cloud #251 dashboard delta widget).

### Cross-repo lockstep

- **`siropkin/budi-cloud`** — ships the cloud half of ADR-0094 in #231–#234 (dual cost columns, Settings → Pricing UI, recalculation engine, `GET /v1/pricing/active`) plus #251 (audit history tab + per-row cost tooltips on the dashboard list-vs-effective delta + savings widget). No daemon-vs-cloud wire-shape change beyond the new pricing endpoint and the `surface` field on the daily-rollup / session-summary POST bodies.
- **`siropkin/budi-jetbrains`** — host extension consumes the new `GET /health/sources` endpoint (siropkin/budi-jetbrains#36) and the `surface=jetbrains` axis. Both Copilot for JetBrains and JetBrains AI Assistant parsers now feed it real rows, so the status-bar plugin renders actual dollar amounts against active sessions instead of `$0.00`.
- **`siropkin/budi-cursor`** — no changes required; the existing `?surface=cursor` filter from 8.4.2 keeps working unchanged.

### Non-blocking, carried forward

- **Cloud dashboard list-vs-effective delta widget** — shipped polish in budi-cloud #251; nothing further required local-side.
- **Surface-aware pricing** — pricing remains keyed on `(provider, model, region)`. Surface is a display/filter axis only.
- **Continue, Cline, Roo Code, Aider, Windsurf, Gemini CLI providers** — still 9.0.0 (#295, #161). 8.4.3 only touches `copilot_chat` (JetBrains host), the new `jetbrains_ai_assistant` provider, and the shared pricing + cloud_sync + daemon HTTP layers.

## 8.4.2 — 2026-05-08

8.4.2 is a follow-up hardening patch on top of 8.4.1's Copilot Chat parser rewrite. After 8.4.1 shipped the v4 mutation-log reducer (#668) that finally produced live rows from `github.copilot-chat` ≥0.47.0, the daily traffic exposed a wave of secondary issues every one of which leaked attribution, dollars, or filters away from the truth: the parser hard-coded `role=assistant` so every user prompt was missing from the message stream; the Copilot Chat globalStorage discovery glob walked entire embedding caches as if they were chat sessions; `modelId` came from the `agent.id` static fallback even when `result.metadata.resolvedModel` carried the actual ID; tool calls and edited file paths sat unread in `result.metadata.toolCallRounds`; emptyWindow chats had no `cwd` because the workspace.json hint never reached the parser; and the LiteLLM pricing manifest's hyphenated/dated model keys did not match the dotted/non-dated IDs the providers actually emit, so freshly captured rows priced at $0 until the next manifest refresh. Three orthogonal correctness bugs were ridden along: `budi stats --provider X` silently dropped the filter on the breakdown subcommands; the `message` slot avg in the statusline divided session cost by **every** assistant row instead of by the user-prompt count; and `session_msg_cost` was emitted in cents while every other `*_cost` field is dollars, so any consumer that round-tripped both fields rendered a 100× bug. Two non-trivial security/robustness fixes also land: the daemon's HTTP API now validates the `Host` header (DNS-rebinding from a malicious webpage could read all analytics and trigger loopback-only admin endpoints), and `read_tail` no longer allocates a `Vec` sized to `file_len` (a pathological multi-GB transcript file could OOM the daemon). Finally, 8.4.2 lays the data-layer groundwork for budi-jetbrains by adding a `surface` axis (`vscode` / `cursor` / `jetbrains` / `terminal` / `unknown`) to messages, sessions, and rollups so host extensions can ask the daemon for "only my host's data" without forking the provider key — the daemon's `/health` `api_version` rolls forward to `3` in lockstep (one tick for the dollars-vs-cents statusline contract change in #707, one tick for the surface axis in #712).

### Fixed

- **Copilot Chat: capture user-role rows from `requests[].message`** (#686, #703) — the v4 mutation-log reducer hard-coded `role=assistant`, so every user prompt vanished from `messages` even though its tokens were already in `promptTokens`. The reducer now also emits a `role=user` row keyed by `requestId` per ADR-0092 §2.4. The user-prompt count is what `message` slot avg and `budi sessions` cost-per-prompt analytics use as their denominator (see #691) — without it both metrics divided by zero or by the wrong count.
- **Copilot Chat: prefer `result.metadata.resolvedModel` over `agent.id` static-table for model attribution** (#685, #700) — the 8.4.1 parser fell back to `agent.id` for `modelId="auto"` (ADR-0092 §2.4.1) but kept using the static fallback even when `result.metadata.resolvedModel` was present and the `LiteLLM` manifest knew it. Now the parser tries (in order): manifest-known `result.metadata.resolvedModel` → `agent.id` resolver → literal `auto`. ADR-0092 §2.4.1 amended; rows that previously priced via the agent-id fallback now price against the actual resolved model.
- **Copilot Chat: extract tool calls + edited file paths from `result.metadata.toolCallRounds`** (#687, #704) — the metadata is what powers per-session tool / edited-files attribution in `budi sessions --format json`. Previously the parser dropped it; the per-session detail and curve endpoints now carry the tool-call summary and the edited-file path list, with one entry per round.
- **Copilot Chat: emptyWindow `cwd` hint from `result.metadata.renderedUserMessage` editorContext** (#688, #705) — sessions opened in an emptyWindow (no folder, no workspace) had `cwd=NULL` because the workspaceStorage `workspace.json` hint (#681) is empty for that case. The parser now reads `editorContext.documentUri` from the rendered user message; for a single-file emptyWindow session the parent directory is the working `cwd` and feeds the same repo_id/git_branch resolver. Falls back to `NULL` only when the rendered message has no editor context (genuinely empty session).
- **Copilot Chat: emptyWindow / workspaceStorage `cwd` enrichment** (#681, #690) — additionally, the workspaceStorage `<hash>/workspace.json` is now read alongside the chatSessions JSONL it sits next to, so `cwd`, `repo_id`, and `git_branch` populate from the host's recorded workspace folder rather than staying `NULL` when the chat session itself does not name a workspace. Combined with the editorContext fallback (#688) and the surface-aware backfill (#701), `cwd` coverage on real Copilot Chat sessions is now complete.
- **Copilot Chat: globalStorage glob narrowed to session-dir allowlist** (#684, #699) — the discovery glob walked everything under `globalStorage/`, picking up embedding caches, language-model state, and CLI state blobs as if they were chat sessions. The watcher now anchors recursion at a `chatSessions/` / `chatEditingSessions/` allowlist so the tailer wakes only on actual session files.
- **Pricing lookup: Anthropic-form ↔ LiteLLM-form model id normalization** (#680, #689) — providers emit dotted, non-dated IDs (e.g. `claude-sonnet-4-5`) but the LiteLLM manifest keys them dashed and dated (`claude-sonnet-4-5-20250929`). The pricing layer now normalizes both sides into a canonical form and tries the dotted/dashed/dated permutations during lookup; rows that previously priced at \$0 with `pricing_source="unknown"` because of this skew now price against the canonical manifest entry.
- **`budi stats --provider` propagates to every breakdown subcommand** (#682, #694) — `budi stats --provider X models|repos|cwds|days|surfaces|...` silently dropped the filter on the per-breakdown CLI path. The breakdown subcommands now thread `--provider` through into the underlying `/analytics` request, matching the parent `budi stats` filter behavior.
- **Statusline `message` slot avg uses user-prompt count denominator** (#691, #706) — `message` slot avg was `session_cost / count(every assistant row)`, which inflated denominators on tool-call-heavy sessions where a single user prompt produces many assistant rows. Now it divides by the user-prompt count (which #686 finally captures correctly), so the per-prompt cost displayed in the statusline matches what users intuitively expect.
- **Statusline `session_msg_cost` is in dollars** (#692, #707) — `session_msg_cost` was emitted in cents while every other `*_cost` field is dollars; any consumer that did per-field arithmetic against both rendered a 100× bug. Now consistent with the rest of the schema. Statusline templates that hard-coded the cents fix should drop their `/100` workaround.
- **Daemon: HTTP API has Host-header allowlist middleware (defeats DNS rebinding)** (#695, #709) — without Host validation a malicious webpage could DNS-rebind a user-controlled hostname to `127.0.0.1` and read all analytics or trigger the loopback-only admin endpoints. The daemon now rejects requests whose `Host` header isn't on a small allowlist (`localhost:<port>`, `127.0.0.1:<port>`, `[::1]:<port>`); LAN/loopback usage from the local CLI / extension is unaffected.
- **Tailer: `read_tail` capped to a sane upper bound** (#696, #710) — `read_tail` allocated a `Vec` sized to the on-disk `file_len`, which on a pathological multi-GB transcript file would OOM the daemon. Reads are now capped per call; tailing falls back to incremental reads for files that exceed the cap.
- **`atomic_write_json` preserves mode bits and symlinks** (#697, #711) — the rename-into-place path replaced symlinks with regular files and dropped restrictive mode bits (e.g. `chmod 600 ~/.claude/settings.json` becoming `644` after `budi init`). Now both are preserved across atomic writes.

### Added

- **`sessions` / `messages` / rollups gain a `surface` axis** (#701, #712) — new column `surface TEXT NOT NULL DEFAULT 'unknown'` on `messages` and `sessions`, and added to the `message_rollups_hourly` / `_daily` PRIMARY KEY (`(bucket_*, role, provider, model, repo_id, git_branch, surface)`) with all triggers updated. Per-provider parser-local inference: `claude_code` / `copilot_cli` / `codex` → `terminal`, `cursor` → `cursor`, `copilot_chat` → path-based (Cursor/Code/VSCodium/`.vscode-server*` → `vscode|cursor`; `JetBrains/...` → `jetbrains` placeholder). Migration backfills existing rows with the same rules; sessions inherit the dominant surface of their messages with provider-rule fallback for stub-only sessions. `provider` answers *which agent*; `surface` answers *which host*. Forking the provider key was rejected because it fragments the Billing API reconciliation in ADR-0092 §3 and the manifest/alias work in ADR-0091 — one provider, one bill; surface is a separate axis.
- **`/analytics` + `budi sessions` / `budi stats`: surface filter and `/analytics/surfaces` breakdown** (#702, #713) — `?surface=` query param on `/analytics/messages`, `/analytics/sessions`, and the per-session detail/curve endpoints; `--surface` flag on `budi sessions` and `budi stats` (incl. every breakdown subcommand per #682); new `/analytics/surfaces` breakdown endpoint and matching `budi stats surfaces` subcommand for "spend by host environment". Sibling extensions (budi-cursor #40, budi-jetbrains discovery) consume the filter to scope their UI to host-relevant rows.
- **Daemon `/health` advertises surface support: `api_version: 3` and `surfaces: [...]`** (#701) — the daemon's advertised `api_version` rolls forward from `1` (8.4.1 release prep value) to `3` across 8.4.2: tick 1 with #707 for the dollars-vs-cents statusline contract change, tick 2 with #712 for the surface axis, both per ADR-0092 §2.6 versioning rule. The `/health` payload now also exposes the canonical `surfaces` value space (`["vscode","cursor","jetbrains","terminal","unknown"]`) so host extensions don't have to hardcode it. The Copilot Chat parser-side `MIN_API_VERSION` constant is unchanged at `4` — the v4 mutation-log shape from 8.4.1 still applies.
- **`budi doctor`: pre-boot transcript history hint with `budi db import` cross-reference** (#693, #708) — pre-existing agent transcripts were seeded at EOF on first boot, so historical sessions never materialized and there was no observable signal that they were recoverable. `budi doctor` now emits an INFO row pointing at `budi db import` whenever it detects pre-boot transcript files for any provider; the hint disappears once an import has been run for that provider.

### Architecture decisions

- **ADR-0092 §2.2 amended for the discovery-glob session-dir allowlist** ([#684](https://github.com/siropkin/budi/issues/684)) — the `globalStorage/{GitHub,github}.copilot{,-chat}/**` recursive globs are now anchored at a `{chatSessions,chat-sessions,sessions}` directory-name allowlist instead of "any `*.json` / `*.jsonl`". Discovery-layer change; `MIN_API_VERSION` does not bump.
- **ADR-0092 §2.4 + §2.4.1 amended for the resolvedModel-first model-id priority** ([#685](https://github.com/siropkin/budi/issues/685)) — the previous "never use `result.metadata.resolvedModel`" rule was too strong (it conflated GPU-fleet codes with all uses of the field). The parser now walks `resolvedModel` (when shape-clean and manifest-known) → `modelId` → §2.4.1 `agent.id` table. The table itself is unchanged from 8.4.1; only the precedence flips. Model-extraction priority change, not a §2.3 record-shape change — `MIN_API_VERSION` does not bump.
- **No new ADR for the `surface` axis** — the per-provider inference rules for `surface` are recorded in the source-of-truth `crates/budi-core/src/surface.rs` module (canonical constants + `default_for_provider()` + `infer_copilot_chat_surface()`) and in the migration backfill in `crates/budi-core/src/migration.rs`. The data-contract version that consumers gate on is the daemon's advertised `/health` `api_version` (now `3`); ADR-0092 §2.6 already covers that mechanism.

### Cross-repo lockstep

- **`siropkin/budi-cursor` 1.5.x** — host extension consumes the surface axis: filters its analytics requests to `?surface=cursor`, drops the host-extension-side workaround that filtered on `provider IN (cursor, copilot_chat)` heuristically, and bumps its compiled `MIN_API_VERSION` to `3` (the value the daemon now advertises on `/health`) so a 1.5.x extension running against an 8.4.1 daemon prints the existing API-version warning instead of rendering empty results. No daemon-vs-extension wire-shape change beyond the new `?surface=` query param and the `surfaces` array on `/health`.
- **`siropkin/budi-jetbrains`** — discovery ticket lands here; the `jetbrains` value in the surface axis is the placeholder it consumes once a real JetBrains parser exists. No 8.4.2 cross-repo work required (the placeholder is sufficient for the host extension to gate its UI on `surface=jetbrains` while waiting for the parser).
- **`siropkin/getbudi.dev`** — no copy change. The supported-agent table already lists Copilot Chat as `Supported`; 8.4.2 only changes how rows are attributed and filtered, not which agents are covered.

### Non-blocking, carried forward

- **JetBrains Copilot Chat parser** — the `surface=jetbrains` axis lands but the parser itself is the next discovery ticket in the budi-jetbrains repo. Until it lands the value never materializes from a real session; only the placeholder fixture exercises the rule.
- **Sub-IDE breakdown for JetBrains** (IDEA vs GoLand vs ...) — one bucket now; subdivide later if usage demands.
- **Surface-aware pricing** — pricing remains keyed on `(provider, model)`. Surface is purely a display/filter axis.
- **Cloud Overview cost / token totals diverge from local CLI** (#10) — presentation-layer aggregation difference, not data loss.
- **RC-4 Part B** (#504) — Cursor Usage API auth root-cause; Part A shipped with `v8.3.1`.
- **Continue, Cline, Roo Code, Aider, Windsurf, Gemini CLI providers** — still 9.0.0 (#295, #161). 8.4.2 only touches `copilot_chat`, `cursor`, `claude_code`, `codex`, `copilot_cli`, and the shared analytics + statusline + tailer + integrations layers.

## 8.4.1 — 2026-05-07

8.4.1 is a focused patch for a Copilot Chat live-tailer regression discovered immediately after 8.4.0 shipped (#647 post-mortem, parent epic #667). The 8.4.0 `copilot_chat` parser parsed each JSONL line independently and looked for `completionTokens` at the top level. That worked for synthetic fixtures and for the older inline-tokens shape, but the `github.copilot-chat` extension ≥0.47.0 (VS Code 1.109+, January 2026) writes session files as a JSON Pointer mutation log (`kind:0` snapshot + `kind:1` set-at-pointer + `kind:2` array-splice), so token counts arrive on later patches like `{"kind":1,"k":["requests",8,"completionTokens"],"v":39}` — never at the top of the line. The 8.4.0 live tailer therefore emitted **zero rows** from active Copilot Chat sessions on extension ≥0.47.0; only historical sessions whose `kind:0` snapshot already inlined the tokens produced rows. Three secondary issues compounded the miss: `budi doctor` had no signal for "tailer is consuming bytes but emitting zero rows," the R2.2 smoke gate seeded rows directly into the DB and never exercised the parser, and `modelId="auto"` priced out at $0 because there is no `auto` entry in the LiteLLM manifest. 8.4.1 fixes the parser, closes the observability + smoke-gate gaps that let the regression ship, and resolves `auto` to a concrete model so individually-licensed users see non-zero list-price-equivalent dollars before the §3 Billing API truth-up tick.

### Fixed

- **Copilot Chat mutation-log reducer** (R1.1, #668) — `copilot_chat` parser is now a per-session reducer that replays `kind:0` snapshots, `kind:1` JSON-Pointer set mutations, and `kind:2` array-splice mutations onto a per-session state, then runs the four-then-five token-key dispatch from ADR-0092 §2.3 against the **materialized** request rather than the raw line. Rows emit the moment the dispatch returns a result, keyed by `requestId` so cross-tick re-replay does not double-emit. `MIN_API_VERSION` bumped to `4` in lockstep with the §2.3 amendment per the §2.6 versioning rule. Live `github.copilot-chat` ≥0.47.0 sessions now produce per-request rows synchronously with the response stream — non-zero `$1d` is visible in one poll cycle of the first prompt.
- **`modelId="auto"` resolves to a concrete model** (R1.4, #671) — when the user picks `auto` in the Copilot Chat model selector, GitHub picks the actual model server-side and persists the literal string `"auto"` as the request's `modelId`. The LiteLLM manifest has no `auto` entry, so 8.4.0 priced these rows at \$0 with `pricing_source = "unknown"`. The parser now resolves `"auto"` via `agent.id` immediately after the §2.4 prefix-strip (`github.copilot.editsAgent` / `codingAgent` → `claude-sonnet-4-5`; `workspaceAgent` / `terminalAgent` / `default` / `chat-default` → `gpt-4.1`). Unrecognised `agent.id` preserves the literal `"auto"` so the row still emits and the §3 Billing API reconciliation supplies the dollar truth on the next tick. ADR-0092 §2.4.1 documents the table; the resolver lives at `crates/budi-core/src/providers/copilot_chat.rs::resolve_auto_model_id`.

### Added

- **`budi doctor` zero-rows-from-tailer signal** (R1.3, #670) — `budi doctor` now has a `tailer rows / Copilot Chat` check that flips to AMBER (`warn`) when a provider's tailer has consumed bytes for >N minutes without emitting any new rows. Reads `tail_offsets` and `messages` from the analytics DB directly, so it does not depend on the daemon being up — exactly what is needed for a parser-pipeline assertion. The detail message hints at `ADR-0092 §2.6 / MIN_API_VERSION` so the next time a Copilot Chat schema flip happens, the AMBER signal points at the canonical fix-it ADR. PASS on a clean install (rows landed); AMBER is byte-equivalent to running the v3 parser against a v4 mutation-log fixture, so this is the gate that would have caught 8.4.0 before the tag.
- **Real-extension regression fixture for the `copilot_chat` parser** (R1.2, #669) — `crates/budi-core/src/providers/copilot_chat/fixtures/vscode_chat_0_47_0.jsonl` is a sanitized capture of an actual `github.copilot-chat` 0.47.0 session file (prompt text, response markdown, code citations, file paths, and local-machine metadata stripped; envelope keys, `requestId`, timestamps, `agent.*`, `modelId`, `responseId`, `modelState`, and `completionTokens` / `promptTokens` patches preserved). A sibling `vscode_chat_0_47_0.expected.json` lists the per-request `(requestId, output_tokens, input_tokens, model)` tuples the reducer must materialize. A truncated companion `vscode_chat_0_47_0_streaming.jsonl` slices the fixture mid-stream — `kind:2` stub written, `kind:1` `completionTokens` patch not yet — and pins the no-emit-until-completion-token contract from R1.1. The fixture is the canonical shape going forward per ADR-0092 §2.3; when extension N+1 changes the format again, the next bump captures a new fixture and the previous one is kept as a regression for the older format.
- **Real-parser smoke-gate steps 20–22** (R1.5, #672) — `scripts/e2e/test_655_release_smoke.sh` now exercises the parser pipeline rather than seeding rows directly into the DB. Step 20 drops the R1.2 fixture under `workspaceStorage/<hash>/chatSessions/<uuid>.jsonl` and asserts the expected row count and `SUM(output_tokens)` materialize via parser → tailer → DB (FAILs against the 8.4.0 broken parser; PASSes against the post-#R1.1 reducer). Step 21 appends a `kind:2` stub and asserts the row count is unchanged, then appends the `kind:1` `completionTokens` patch and asserts exactly one new row materializes — pins the no-emit-until-completion contract against the live tailer. Step 22 runs `budi doctor --format json` after step 20 and asserts `tailer rows / Copilot Chat` is `pass`, then simulates the broken-parser state (clears `messages` rows while leaving `tail_offsets` intact) and asserts the same check flips to AMBER with the parser-regression hint. The release smoke plan at `docs/release/v8.4.1-smoke-test.md` documents the new automated steps and adds one manual step ("send one prompt in VS Code Copilot Chat with the model selector on `auto`; assert `budi sessions` shows the session within one poll cycle with non-zero `cost_cents`") to the v8.4.0 plan.

### Architecture decisions

- **ADR-0092 §2.3 amended for the v4 mutation-log shape** — the four-then-five token-key shapes now apply to **materialized** request records (after `kind:0` + `kind:1` / `kind:2` replay), not raw lines. New §2.3 paragraph documents the mutation-log reducer (R1.1 #668), tail-offset semantics under full-replay, and the no-double-emit invariant. New §2.4.1 documents the `auto` router resolver table (R1.4 #671). The R1.2 fixture (#669) is now the canonical on-disk reference; future format flips capture a new fixture and keep the old one as a regression for the previous format. `MIN_API_VERSION` bumped to `4` per the §2.6 versioning rule — daemon-vs-extension drift surfaces as a `budi doctor` warning rather than silent zeros.

### Cross-repo lockstep

- **`siropkin/budi-cursor` 1.4.x** — no extension release required for 8.4.1. The wire shape between the host extension and the daemon is unchanged; the parser fix is daemon-side only. The host extension's compiled `MIN_API_VERSION` continues to compare against the daemon's `/health` numeric `api_version` (8.4.0 R1.6 #653 mechanism); a developer running 1.4.x against an 8.4.0 daemon sees the existing API-version warning rather than rendering silent zeros.
- **`siropkin/getbudi.dev`** — no copy change. The supported-agent table on the public site already lists Copilot Chat as `Supported` (8.4.0 R1.4 #651 / #653), and 8.4.1 does not change support level or the dollar surfaces — only what rows materialize from active sessions.

### Non-blocking, carried forward

- **Cloud Overview cost / token totals diverge from local CLI** (#10) — presentation-layer aggregation difference, not data loss.
- **RC-4 Part B** (#504) — Cursor Usage API auth root-cause; Part A shipped with `v8.3.1`.
- **ADR-0090 supersede** — pending one release cycle of live validation on the now-working `cursorDiskKV` bubbles path before the Usage API §1 surface can be retired.
- **Continue, Cline, Roo Code, Aider, Windsurf, Gemini CLI providers** — still 9.0.0 (#295, #161). 8.4.1 only touches `copilot_chat`. The R1.4 `auto` resolver pattern (ADR-0092 §2.4.1) is the canonical shape future per-provider tables will reuse.

## 8.4.0 — 2026-05-07

8.4.0 is the **VS Code-side coverage** release: the budi extension now hosts inside VS Code as well as Cursor, with **GitHub Copilot Chat** as the first non-Cursor provider. The architectural pieces this round forces into the codebase — a host-scoped statusline surface that aggregates over multiple providers in a single editor window, a third-party-API-as-reconciliation pattern (GitHub Billing API), and a `MIN_API_VERSION` pattern for provider-side data contracts — are the same pieces every later 9.0.0 provider will need (#647).

### Added

- **`copilot_chat` provider plugin — local JSON/JSONL tailing** (R1.4, #651) — new provider tails per-message tokens out of VS Code's `workspaceStorage` and `globalStorage` for the GitHub Copilot Chat extension on macOS / Linux / Windows. Tokens × pricing manifest is the primary live cost path; ADR-0092 §2 pins the on-disk format with a `MIN_API_VERSION = 1` guard so a future Copilot Chat schema flip surfaces in `budi doctor` rather than producing silent zeros. Org-managed users (where the GitHub Billing API is unavailable) rely entirely on this local-tail × pricing path.
- **GitHub Billing API reconciliation worker for `copilot_chat`** (R1.5, #652) — `sync_direct` worker pulls per-user dollar truth-up for individually-licensed users and reconciles against the local-tail rows. Org-managed users see an empty endpoint and the worker no-ops cleanly. Runs on the same configurable interval as the cloud-sync worker; auth/schema errors are structured-logged and surfaced on the existing pricing/sync status endpoints. ADR-0092 §3 governs the contract.
- **Multi-provider statusline endpoint — `?provider=a,b,c`** (R1.3, #650) — `GET /analytics/statusline` and `budi statusline --format json` now accept a comma-list `?provider=` value and aggregate every numeric field (`cost_1d` / `cost_7d` / `cost_30d`, `session_cost`, `branch_cost`, `project_cost`) over the listed providers. The response carries a new `contributing_providers` array for tooltip rendering and click-through routing; `provider_scope` is omitted under multi-provider so the byte shape of the single-provider response is unchanged. Single-provider `?provider=cursor` is byte-identical to the 8.1 contract — no consumer is forced to change. The endpoint is host-scoped per the ADR-0088 §7 amendment (#648); cloud dashboard tiles stay provider-scoped.
- **`budi doctor` surfaces installed VS Code AI extensions and tailer health** (R1.6, #653) — `budi doctor` detects installed Copilot Chat / Continue / Cline / Roo Code / Aider / Windsurf extensions in VS Code, reports per-provider tailer health, and flags ADR-0092 §2.6 `MIN_API_VERSION` mismatches (Copilot Chat schema flip → visible warning, not silent zeros). Org-managed Copilot users and unconfigured-PAT cases are surfaced as informational signals (ADR-0092 §3.3 / §3.4) so the install-funnel state is visible without remote telemetry.

### Changed

- **Supported agents — Copilot Chat is now first-class.** README's supported-agent table now lists Copilot Chat alongside Claude Code, Codex CLI, Cursor, and Copilot CLI. The VS Code host is documented as a peer of the Cursor host for the budi extension.
- **`docs/statusline-contract.md` updated for the host-scoped surface** — the contract now documents both **provider-scoped** (`?provider=<single>`) and **host-scoped** (`?provider=<a>,<b>,<c>`) behavior, the `contributing_providers` field, and the unknown-provider tolerance rule (unknown names contribute `0.0` and survive in `contributing_providers`). Provider-scoped consumers (Claude Code statusline, cloud dashboard tiles) are unaffected.

### Architecture decisions

- **ADR-0088 §7 amended — host-scoped vs. provider-scoped surfaces** (R1.1, #648). The host extension surface (the VS Code / Cursor status bar) is allowed to aggregate across providers detected in that editor host; provider-scoped surfaces (Claude Code statusline, cloud dashboard per-provider tiles) remain provider-only. Cloud dashboard rollups stay provider-scoped per the existing §7 — only the in-editor surface aggregates. The amendment lands ahead of the multi-provider endpoint so the endpoint is not an ADR violation at merge time.
- **ADR-0092 — `copilot_chat` data contract** (R1.2, #649). Pins the on-disk format Copilot Chat writes under `workspaceStorage` / `globalStorage`, the GitHub Billing API contract for individually-licensed users, the `MIN_API_VERSION` bump rule for visible drift detection, and the org-managed-user fallback path (local-tail × pricing).

### Cross-repo lockstep

- **`siropkin/budi-cursor` 1.4.x** ships VS Code host detection, the multi-provider request shape (`?provider=cursor,copilot_chat` when both are detected), tooltip copy update for the host-scoped surface, and a `MIN_API_VERSION` bump in `budiClient.ts` matched to this contract. Older daemons continue to surface the existing API-version warning rather than rendering silent zeros. The bundled vsix in this repo was retired in 8.0 (#96), so there is nothing to refresh on the budi side.
- **`siropkin/getbudi.dev`** copy and screenshots refresh for VS Code support is threaded into the public-site sync ticket; never let the public-site copy drift from shipped behavior.

### Non-blocking, carried forward

- **Cloud Overview cost / token totals diverge from local CLI** (#10) — presentation-layer aggregation difference, not data loss.
- **RC-4 Part B** (#504) — Cursor Usage API auth root-cause; Part A shipped with `v8.3.1`.
- **ADR-0090 supersede** — pending one release cycle of live validation on the now-working `cursorDiskKV` bubbles path before the Usage API §1 surface can be retired.
- **Continue, Cline, Roo Code, Aider, Windsurf, Gemini CLI providers** — deferred to the 9.0.0 breadth round (#295, #161). 8.4.0 ships the architectural pieces every one of those providers will reuse: host-scoped surface, multi-provider endpoint, third-party-API-as-reconciliation pattern.

## 8.3.19 — 2026-05-05

8.3.19 is a quality-of-life release that improves statusline ergonomics
outside the Claude Code stdin envelope, lights up per-session model
attribution on the cloud sync envelope, and finishes the 8.3.18 doc /
slot-removal cleanup.

### Added

- **`budi statusline --slots <list>` flag and auto-resolve current session (#639 / PR #643)** —
  `--slots` overrides `~/.config/budi/statusline.toml` for a single
  invocation (comma-separated, with `today/week/month` normalized to
  `1d/7d/30d`). When `session` or `message` slots are required but
  stdin didn't carry a `session_id` (manual TTY invocation, custom
  drivers, non–Claude-Code agents), the renderer falls back to the
  daemon's `/analytics/sessions/resolve?token=current` endpoint scoped
  to cwd, so the coach and full presets stop silently dropping back to
  rolling-window slots outside the Claude Code stdin envelope. Errors
  on the resolve path are swallowed so the prompt-hot path keeps
  rendering.
- **Per-session `primary_model` on the cloud-sync envelope (#638 / PR #642)** —
  `SessionSummaryRecord` now carries an optional `primary_model` field,
  defined as the model that consumed the largest share of
  `input + output` tokens for the session, with ties broken by
  latest-used. Field is omitted (not empty-string) when the session has
  zero scored messages so the cloud column stays NULL rather than
  guessing. ADR-0083 §2 updated to match.

### Changed

- **Docs / config audit pass for 8.3.18 statusline changes (#640 / PR #644)** —
  `SOUL.md`, `CHANGELOG.md`, `crates/budi-core/src/config.rs`, and
  `docs/statusline-contract.md` updated to reflect the current slot
  vocabulary (`1d`, `7d`, `30d`, `session`, `message`, `branch`,
  `project`, `provider`), the `health` slot removal, and the
  legacy-`coach`/`full` preset migration. Removed a stale reference to
  the retired `STATUSLINE_PRESETS` constant and a `{health}` placeholder
  from the `StatuslineConfig::format` docstring.

### Fixed

- **Stale `health` slot / preset references missed in 8.3.18 (#641 / PR #645)** —
  follow-up audit on the 8.3.18 statusline changes turned up three
  real stragglers: `crates/budi-cli/src/commands/init.rs:227` still
  hinted at the removed `--statusline-preset coach` flag,
  `scripts/e2e/test_600_init_seeds_statusline_toml.sh` pinned
  `preset = "coach"` / `preset = "full"` markers and a confirmation
  grep that no longer matched the current template, and `SOUL.md:518`
  claimed the `coach` preset still rendered "health icon + session
  cost + tip". All three updated; the e2e test now passes (was failing
  on master). Daemon `health_state` / `health_tip` fields are kept on
  purpose — they are part of the documented 8.x statusline contract
  and consumed by the Cursor extension.

### Non-blocking, carried forward

- **Cloud Overview cost / token totals diverge from local CLI** (#10) — presentation-layer aggregation difference, not data loss.
- **RC-4 Part B** (#504) — Cursor Usage API auth root-cause; Part A shipped with `v8.3.1`.
- **ADR-0090 supersede** — pending one release cycle of live validation on the now-working `cursorDiskKV` bubbles path before the Usage API §1 surface can be retired.

## 8.3.18 — 2026-05-05

8.3.18 adds first-class `session` and `message` statusline slots, cleans
up legacy rendering machinery, and fixes two consistency bugs.
Headline: **statusline slots are now fully composable** — `session` and
`message` work like any other slot (`1d`, `7d`, `30d`), and the old
`preset` / `render_coach` codepath with its hard-coded 📊 emoji is gone.

### Added

- **`session` and `message` as first-class statusline slots (#631 / PR #635)** —
  users can now place `session` and `message` in their `slots` array
  like any rolling-window slot. `session` shows current-session cost;
  `message` shows last-message cost. Both read from the daemon response
  and convert cents → dollars through the standard slot pipeline.

### Changed

- **Remove `preset` / `render_coach` machinery and 📊 emoji (#632 / PR #636)** —
  the statusline had two rendering paths: normal slots and a special
  `render_coach()` codepath triggered by `preset = "coach"`. This
  created dead code, config confusion, and a hard-coded emoji.
  All presets now expand to regular slot arrays at config load time;
  `render_coach` and the 📊 prefix are removed.
- **Remove vestigial `health` statusline slot; coach preset → session + message** —
  the `health` slot was a leftover from the old `render_coach`
  codepath that rendered the same `session_cost` dollar amount as the
  `session` slot, so the legacy `coach` preset showed duplicate values
  (`$1.22 session · $1.22 health`). The `health` slot is removed from
  the slot vocabulary entirely; legacy `preset = "coach"` /
  `preset = "full"` values in older `statusline.toml` files now expand
  to `["session", "message"]` / `["session", "message", "1d"]` for
  migration. Patched in after the initial 8.3.18 release prep.

### Fixed

- **`budi sessions latest --format json` `health_state` consistency (#629 / PR #633)** —
  the list view (`budi sessions --format json`) emitted `health_state`
  (a string) while the detail view (`budi sessions latest --format json`)
  only emitted `health` (an object). The detail view now includes both
  fields for consistency.
- **Flaky `e2e_refresh_from_v8_3_14` test under parallel execution (#630 / PR #634)** —
  the test temporarily overrode the `HOME` environment variable, which
  raced with other tests sharing the process-wide env. Fix: isolated
  the HOME override to eliminate the race condition.

### Non-blocking, carried forward

- **Cloud Overview cost / token totals diverge from local CLI** (#10) — presentation-layer aggregation difference, not data loss.
- **RC-4 Part B** (#504) — Cursor Usage API auth root-cause; Part A shipped with `v8.3.1`.
- **ADR-0090 supersede** — pending one release cycle of live validation on the now-working `cursorDiskKV` bubbles path before the Usage API §1 surface can be retired.

## 8.3.17 — 2026-05-05

8.3.17 is a quality-of-life release that fixes five user-facing bugs and
adds two test-coverage backstops for fixes that landed in 8.3.16.
Headline: **`budi stats activities` no longer silently drops attribution**
when assistant messages arrive in a later tailer batch than the user
messages that classify them. The rest closes out JSON output gaps
(`sessions --format json` truncation envelope, `id_short`), a
`budi status` / `budi stats` cost divergence window, inconsistent
`--provider` validation across commands, and a `budi doctor` display
bug that masked daemon outages behind a green PASS.

### Fixed

- **`budi stats activities` no longer drops attribution (#616 / PR #622)** —
  the live tailer processes user and assistant messages in separate
  batches (500 ms debounce). `propagate_session_context` only propagated
  `prompt_category` from user → assistant within a single
  `Pipeline::process()` call, so assistant messages arriving in a later
  batch never inherited the classification and never received activity
  tags. Fix: deferred propagation across batch boundaries.
- **`budi status` Today cost no longer lags `budi stats` (#619 / PR #621)** —
  `status` read a cached aggregate while `stats` queried live SQL,
  creating a visible divergence window of seconds after each ingest.
  New `/analytics/status_snapshot` daemon endpoint queries summary,
  cost, and providers from a single DB connection, guaranteeing a
  consistent point-in-time snapshot.
- **`budi statusline --provider <unknown>` now validates (#615 / PR #620)** —
  `statusline` silently returned zero cost for unknown providers while
  `stats` rejected them with a helpful list. Centralized provider
  parsing so both commands validate against the canonical provider set.
- **`budi sessions --format json` truncation envelope (#617 / PR #623)** —
  JSON output now wraps sessions in an envelope with `returned_count`,
  `truncated`, `limit`, and `window` fields so consumers can tell
  whether they hit the cap.
- **`budi doctor` auto-recovery display (#612 / PR #627)** —
  when `doctor` auto-starts a dead daemon, text output now shows
  supervisor state (e.g. `supervisor: launchd LaunchAgent: installed
  (not running)`) alongside the gap duration, instead of masking the
  outage behind a green PASS.

### Added

- **`id_short` in session JSON output (#618 / PR #624)** —
  `budi sessions --format json` and `budi sessions <id> --format json`
  now emit both `id` (full UUID) and `id_short` (8-char prefix).

### Tests

- **macOS launchctl kickstart regression guards (#611 / PR #625)** —
  extracted `launchctl_kickstart_target()` helper from
  `try_launchctl_kickstart()` for testability; added unit tests pinning
  the expected kickstart target format.
- **Upgrade-path integration refresh e2e (#613 / PR #626)** —
  simulates upgrading from a v8.3.14 `integrations.toml` (missing the
  `/budi` skill component) and verifies that `refresh_enabled_integrations`
  creates `statusline.toml`, `SKILL.md` with canonical bytes, and
  persists the expanded component set.

### Non-blocking, carried forward

- **Cloud Overview cost / token totals diverge from local CLI** (#10) — presentation-layer aggregation difference, not data loss.
- **RC-4 Part B** (#504) — Cursor Usage API auth root-cause; Part A shipped with `v8.3.1`.
- **ADR-0090 supersede** — pending one release cycle of live validation on the now-working `cursorDiskKV` bubbles path before the Usage API §1 surface can be retired.

## 8.3.16 — 2026-05-01

8.3.16 is a same-day patch release on top of 8.3.15 that closes three
"the update path is fragile" gaps surfaced during the 8.3.15 release-day
smoke test. Headline: **8.3.15 features didn't actually reach upgrading
users** because the in-process integration refresh during `budi update`
ran the PRE-install CLI's logic. This release fixes that, plus the two
sibling lifecycle bugs the smoke test uncovered.

### Fixed

- **macOS daemon survives `budi update` (#611 / PR #614)** —
  `restart_daemon_for_version_upgrade` had a Linux systemd-user branch
  (from #582 in v8.3.14) but no macOS equivalent. Pre-fix, the macOS
  branch fell into the raw-spawn fallback and the daemon ran outside
  launchd; once that CLI-child later exited cleanly (terminal close,
  second `budi update`, OS signal), launchd's
  `KeepAlive.SuccessfulExit=false` left the LaunchAgent orphaned at
  `state = not running` until next login — silently producing
  multi-hour ingestion gaps. Added `try_launchctl_kickstart` that runs
  `launchctl kickstart -k gui/$UID/dev.getbudi.budi-daemon` when the
  LaunchAgent plist is registered, mirroring the Linux systemd path.
  Confirmed via repro on a developer box: ~22 h gap between the
  v8.3.13 → v8.3.14 update and the next CLI invocation that revived
  the daemon.
- **`budi update` actually delivers new integrations to upgrading users
  (#613 / PR #614)** — pre-fix, the post-install integration refresh
  ran in-process against the OLD (pre-update) CLI binary. Any new
  `IntegrationComponent` or seeded file added in a release silently
  skipped upgraders: 8.3.15's `/budi` skill (#603) and seeded
  `statusline.toml` (#600) didn't reach existing installs at all.
  Update now re-execs the freshly-installed CLI via the new
  `budi integrations refresh` subcommand; the in-process call remains
  as a defensive fallback if the re-exec can't be launched.
  `refresh_enabled_integrations` also unions the user's stored
  `integrations.toml` with `default_recommended_components()` so
  components added in newer releases install idempotently for
  upgraders without a manual `budi integrations install --with …`.
- **`budi doctor` no longer hides daemon outages behind a green PASS
  (#612 / PR #614)** — pre-fix, when `doctor` had to auto-start a dead
  daemon to run its diagnostic, it reported the result as `PASS daemon
  health: started successfully`, making `All checks passed.` print
  immediately after rescuing the daemon from a multi-hour outage. Now
  reports WARN (`auto-recovered: was NOT running on first probe; doctor
  started it`) and surfaces the approximate gap duration from the
  daemon log's mtime (e.g. `~22h ago`), pointing at #611 as the likely
  root cause on macOS.

### Non-blocking, carried forward

- **Cloud Overview cost / token totals diverge from local CLI** (#10) — same as 8.3.15; presentation-layer aggregation difference, not data loss.
- **RC-4 Part B** (#504) — Cursor Usage API auth root-cause; Part A shipped with `v8.3.1`.
- **ADR-0090 supersede** — pending one release cycle of live validation on the now-working `cursorDiskKV` bubbles path before the Usage API §1 surface can be retired.
- **Detached daemon log capture** — first post-`budi update` daemon's startup lines don't land in `~/Library/Logs/budi-daemon.log` until the next launchctl kickstart. Should be largely closed by #611 in 8.3.16; observability-only carry-forward from v8.3.6 / v8.3.7 if the no-autostart raw-spawn branch still hits it.

## 8.3.15 — 2026-05-01

8.3.15 is a discoverability + Claude Code integration polish release
that closes out the 8.3.15 milestone. The headline is **`/budi`**, an
auto-installed Claude Code skill that surfaces session vitals for the
*current* cwd — the answer to "buddy is back, but now it's budi" — and
its server-side `budi sessions current` resolver. The rest is the
8.3.14-aftermath sweep: the silent statusline install path now
self-documents (seeded `statusline.toml` + a one-line hint in `init`
output), the README catches up to the 8.3.14 surface drift, and the
`session_health` insufficient-data tip tells users *how many*
assistant messages they actually have. No wire / data-shape changes;
no ADR amendments.

### Added

- **`/budi` Claude Code skill + `budi sessions current` resolver
  (#603 / PR #609)** — Anthropic shipped `/buddy` as April Fools 2026
  and pulled it on April 9. We're shipping the real product
  opportunity in the gap: a tiny Claude Code skill at
  `~/.claude/skills/budi/SKILL.md` that returns Prompt Growth, Cache
  Reuse, Retry Loops, and Cost Acceleration for the *current* session
  — auto-installed alongside the Claude Code statusline. New
  server-side token `budi sessions current` resolves the active
  Claude Code session for the CLI's cwd (encodes `/` → `-`, looks
  under `~/.claude/projects/<encoded-cwd>/`, returns the live
  `session_id`); sibling to the existing `latest` token. Help surface
  advertises both.
- **Seeded `~/.config/budi/statusline.toml` on first install (#600 /
  PR #605)** — pre-fix, the file the README told users to edit
  didn't exist after `budi init`, leaving customization a
  discoverability dead end. The init / `integrations install` path
  now writes the file idempotently with the active `cost` preset and
  commented examples for `coach` / `full` / custom slots, so users
  can `cat` / tab-complete to it and discover the presets without
  hunting the README.
- **Statusline customization hint in `budi init` output (#604 / PR
  #608)** — companion to the seeding fix: the previously silent
  statusline install now prints a single dim line on first install
  pointing at `~/.config/budi/statusline.toml` and the `coach`
  preset. Suppressed on repeat `budi init`, on `--no-integrations`,
  and when an existing budi marker indicates the user is already
  onboarded.

### Changed

- **`session_health` insufficient-data tip surfaces assistant-message
  count (#602 / PR #607)** — when vitals haven't ramped yet because
  the rolling window is too small, the tip now tells users *how
  many* assistant messages they have (and how many they need)
  instead of the opaque "INSUFFICIENT DATA" verdict alone. Removes
  the failure mode where a user mid-session reads "insufficient
  data" as "the feature is broken and not updating."

### Documentation

- **README pass for 8.3.14-era surface drift (#601 / PR #606)** —
  swept stale references to the verbs and flags 8.3.14 removed
  (`budi vitals` / `budi health` / `db migrate` / `db repair` /
  `init --cleanup` / `pricing status --refresh` / per-flag
  `stats --by-*`) across `README.md`, `SOUL.md`, and `docs/`, and
  re-checked the troubleshooting and full CLI reference sections
  against the actual binary's `--help` output.

### Closed without code change

- **Friendly redirects for removed verbs (#599)** — closed
  completed without a shipped redirect dispatcher. Today, `budi
  vitals` / `budi health` / `db migrate` / `db repair` hard-fail at
  the clap layer (the existing parse-rejection from 8.3.14, with
  unit-test guard `cli_no_longer_exposes_vitals_or_health_top_level`
  in `crates/budi-cli/src/main.rs`); the explicit "this verb moved
  to X" hint message did not land in 8.3.15 and is implicitly
  carried forward as polish if the misleading clap suggestion
  resurfaces in user reports.

### Non-blocking, carried forward

- **Cloud Overview cost / token totals diverge from local CLI** (#10) — same as 8.3.14; presentation-layer aggregation difference, not data loss.
- **RC-4 Part B** (#504) — Cursor Usage API auth root-cause; Part A shipped with `v8.3.1`.
- **ADR-0090 supersede** — pending one release cycle of live validation on the now-working `cursorDiskKV` bubbles path before the Usage API §1 surface can be retired.
- **Detached daemon log capture** — first post-`budi update` daemon's startup lines don't land in `~/Library/Logs/budi-daemon.log` until the next launchctl kickstart. Observability-only; carried from v8.3.6 / v8.3.7.

## 8.3.14 — 2026-04-30

8.3.14 is a CLI ergonomics + Linux-daemon stability release that
closes out the entire 8.3.14 milestone (9/9 issues). One real bug fix
(Linux daemon dying with the CLI on `budi update`) plus an
across-the-board cleanup of the `budi` command surface — pruning dead
flags, splitting overloaded subcommands, and standardizing
`--format json` on system-state commands so the surface is finally
self-consistent before 8.4. No wire / data-shape changes; no ADR
amendments.

### Fixed

- **Linux daemon now survives `budi update` (#582 / PR #590)**.
  Pre-fix, on Linux the new daemon was spawned as a child of the CLI
  with no `setsid()`, so closing the terminal killed it; macOS hid the
  bug because launchd respawned under its own parent. Three changes:
  `setsid()` on Unix daemon spawn so the child becomes a session
  leader; systemctl-aware restart on Linux that routes through
  `systemctl --user restart budi-daemon` when a `budi autostart
  install` systemd unit is registered (falls back to raw kill/spawn
  otherwise); and a one-line nudge to `budi autostart install` at the
  end of `budi update` on Linux when no autostart is registered.
  Pinned by a new `detach_from_session_starts_new_process_group`
  regression test that fails when the `pre_exec` hook is removed.
  #581 (separate `budi autostart restart` command) was closed in
  favor of folding the fix straight into `budi update`.

### Changed (CLI surface cleanup)

- **`budi cloud sync --full` replaces the two-step reset+sync flow
  (#583 / PR #591)** — single command drops watermarks and resyncs in
  one call so the "cloud lost everything, push it all back" path is
  one verb instead of two.
- **`budi stats` view flags converted to subcommands (#589 / PR
  #592)** — the 11 mutually-exclusive `--by-*` flags
  (`--by-day`, `--by-tool`, …) are now subcommands
  (`budi stats day`, `budi stats tool`, …). Old flags removed; help
  output is finally legible.
- **`budi pricing status` and `budi pricing sync` are now distinct
  commands (#584 / PR #593)** — the overloaded
  `pricing status --refresh` is gone; `status` is read-only,
  `sync` is the explicit network call.
- **`--format json` is now consistent across system-state commands
  (#588 / PR #594)** — `budi status`, `budi doctor`,
  `budi autostart status`, `budi cloud reset` all accept
  `--format json` and emit the same envelope shape, so scripting
  against the CLI no longer requires four parsers.
- **`budi vitals` folded into `budi sessions <id>` (#585 / PR
  #595)** — top-level `vitals` is removed; per-session vitals live
  on the session subcommand they were always describing.
- **`budi init --cleanup` flag removed (#587 / PR #596)** — the
  flag existed to wipe legacy 8.0/8.1 proxy residue from
  ~/.budi; nine months in, that residue is statistically gone from
  user machines, and the consent-first cleanup flow that gated it is
  also removed.
- **`budi db` namespace simplified (#586 / PR #597)** — `db
  migrate` is dropped (every entry point already runs migrations on
  open, so the explicit verb did nothing distinct); `db repair` is
  renamed `db check` and gains a `--fix` flag, so the read-only
  diagnostic and the destructive repair are one command with one
  switch instead of two verbs that did almost the same thing.

### Non-blocking, carried forward

- **Cloud Overview cost / token totals diverge from local CLI** (#10) — same as 8.3.13; presentation-layer aggregation difference, not data loss.
- **RC-4 Part B** (#504) — Cursor Usage API auth root-cause; Part A shipped with `v8.3.1`.
- **ADR-0090 supersede** — pending one release cycle of live validation on the now-working `cursorDiskKV` bubbles path before the Usage API §1 surface can be retired.
- **Detached daemon log capture** — first post-`budi update` daemon's startup lines don't land in `~/Library/Logs/budi-daemon.log` until the next launchctl kickstart. Observability-only; carried from v8.3.6 / v8.3.7.

## 8.3.13 — 2026-04-29

8.3.13 is a same-day follow-up on `v8.3.12` that closes out two
parallel bugs in the `sessions` heal pass that #569 / PR #570
introduced. Both surfaced during v8.3.12 release validation against
the cloud Sessions page after a fresh `budi cloud reset && budi cloud
sync` re-uploaded everything: claude_code and codex sessions reached
the cloud, but every row rendered as `Repo = (unknown) / Branch = -`
and every active session showed `Duration = <1m`. No new ingest /
pricing behavior, no ADR amendments, no wire / data-shape changes —
the wire and the cloud already accept everything we now send; the heal
just wasn't filling enough columns and was freezing one of them too
early.

### Fixed

- **`sessions.repo_id` / `sessions.git_branch` now populated for claude_code & codex** (#577 / PR #579). Pre-fix the heal pass added in PR #570 only filled `started_at` / `ended_at`. Every claude_code (992/992) and codex (67/67) session row stayed with `repo_id IS NULL AND git_branch IS NULL` even though the underlying `messages` rows carried both for ~99% of rows (149,324 messages with repo_id, 151,083 with git_branch on the maintainer DB). `cloud_sync::fetch_session_summaries` SELECTs `s.repo_id, s.git_branch` straight off `sessions`, so cloud `/dashboard/sessions` rendered every row as `(unknown) / -`. Post-fix the heal pass in `analytics::sync` (per-batch) and `migration::backfill_session_timestamps_from_messages` (post-tick + boot) backfills both columns from the matching session's most-recent message — latest-known wins for branch so a mid-session branch switch is reflected; `COALESCE(NULLIF(…, ''), …)` preserves any authoritative session-row value already set. Pinned by 2 new unit tests covering claude_code + codex hydration with a mid-session branch switch and the preserve-already-populated invariant.

- **`sessions.ended_at` now advances as new messages arrive for in-flight sessions** (#578 / PR #579). Pre-fix `ended_at = COALESCE(ended_at, MAX(messages.timestamp))` froze the column at the first ingest tick's MAX. Active sessions still streaming new messages stayed at the timestamp of message #3 (~1.85 s after start) forever, and the WHERE-guard `(started_at IS NULL OR ended_at IS NULL)` made every subsequent heal-pass run skip the row. Real evidence from the maintainer DB the day this was filed: session `ba1e53ac-…` had `ended_at = 02:37:08` while `messages` for the session spanned 02:37:07 → 03:11:44 (270 rows over 34 minutes). Cloud Sessions rendered every recent claude_code row as `Duration = <1m` while older sessions (whose first heal-pass tick happened to land late in their lifetime) showed plausible durations. Post-fix the heal always recomputes `MAX(messages.timestamp)` for any session whose stored `ended_at` lags. `started_at` keeps `COALESCE` since it's immutable. The WHERE clause is tightened so the heal stays idempotent on a stable DB — repo / branch holes are only counted when the matching messages actually carry repo / branch. Pinned by an explicit regression-guard unit test that pre-populates a session row with `started_at = X, ended_at = X+1s`, ingests messages spanning 30 minutes, and asserts `ended_at` advances to the new MAX (pre-fix asserted it stayed frozen at `X+1s`).

### Non-blocking, carried forward

- **Cloud Overview cost / token totals diverge from local CLI** — cloud Overview "All time" reads $2,322 / 118.9 M tokens / 40.2 K messages while local `budi stats -p all` reads $11,267 / 1.07 B in+out / 169.6 K messages. Sessions counts roughly match (cloud 2.4 K vs local 2,356). Likely cloud aggregates differ from local `cost_cents` (e.g. excludes cache costs from the totals) and cloud `Messages` is summed off `session_summaries.message_count` which is `COUNT(role='assistant')`, not all-roles. Tracked in #10 ; not blocking 8.3.13 since it's a presentation-layer concern, not data loss.
- **RC-4 Part B** (#504) — Cursor Usage API auth root-cause; Part A shipped with `v8.3.1`.
- **ADR-0090 supersede** — pending one release cycle of live validation on the now-working `cursorDiskKV` bubbles path before the Usage API §1 surface can be retired.
- **Detached daemon log capture** — first post-`budi update` daemon's startup lines don't land in `~/Library/Logs/budi-daemon.log` until the next launchctl kickstart. Observability-only; carried from v8.3.6 / v8.3.7.

## 8.3.12 — 2026-04-29

8.3.12 is a same-day follow-up to `v8.3.11`. Verifying the
`session_summaries` fix on a real dogfood DB after `budi cloud reset`
returned `Server returned 413` — the watermark-less re-upload built a
single envelope of 1931 rollups + 2350 sessions and blew past the
cloud's body-size cap. Cutting the v8.3.11 tag immediately after
merging the prep PR also surfaced a long-suspected race in the release
workflow: `wait-on-check-action` exited within seconds because the
`ci-success` umbrella check hadn't been registered yet on the fresh
tag SHA, so every release prep that pushed a tag right after merge was
playing roulette with a workflow_dispatch retry. Both are
infrastructure fixes — no new ingest / pricing behavior, no ADR
amendments, no wire / data-shape changes.

### Fixed

- **`budi cloud reset` + `budi cloud sync` no longer 413 on multi-month DBs** (#572 / PR #574). Pre-fix `cloud_sync::build_envelope` collected everything `fetch_daily_rollups` and `fetch_session_summaries` returned for the watermark in one shot and `push_envelope` POSTed it as one body. Per-tick incremental sync stayed under the limit by accident — each tick covers ≤ 1 day of new data — but `budi cloud reset` deliberately drops the three sentinel rows in `sync_state` (`__budi_cloud_sync__`, `__budi_cloud_sync___value`, `__budi_cloud_sync_sessions__`) to force a no-watermark rebuild, and the rebuilt envelope on a real dogfood DB (~8 rollups/day × ~245 days, ~10 sessions/day → ~4000 records, ~8 MB body) immediately exceeded the cloud's body-size cap. Real evidence from the maintainer machine the day this was filed: `budi cloud reset --yes` succeeded, `budi cloud sync` failed with `Cloud sync hit a transient error: Server returned 413` (`attempted 1931 rollups, 2350 sessions`). Workaround used to verify v8.3.11 fix #569 was a manual `INSERT OR REPLACE INTO sync_state` to seed an intermediate watermark — not a hatch any user will figure out, and explicitly the recovery path that v8.3.10 advertised. Post-fix `cloud_sync.rs` chunks the envelope client-side at `MAX_RECORDS_PER_ENVELOPE = 500`, with rollup chunks day-aligned (a single `bucket_day` never spans two chunks) so the local "watermark = latest day fully synced" contract from ADR-0083 §5 stays honest on partial-chunk failure. The chunk loop POSTs each batch separately; on partial-chunk failure already-confirmed watermark progress is preserved and the next tick / CLI retry resumes from there. Cloud-side dedup (ADR-0083 §6) keeps the re-upload safe even when records overlap with rows the cloud already has. Per-chunk progress now surfaces through `SyncTickReport` and the daemon's `/cloud/sync` JSON, so `budi cloud sync` renders "(N records pushed across M chunks)" instead of one long silence. Pinned by 6 new unit tests covering: small payload → single chunk, empty input → one empty chunk, large rollup set → day-aligned multi-chunk split, oversized single day stays intact (one `bucket_day` is never split), sessions chunk independently of rollups, dogfood-sized payload (~1920 rollups + 2350 sessions) splits as expected; plus a new daemon route test for the partial-success message including chunk progress in the transient-error path.

- **Release workflow no longer races on the `ci-success` umbrella check for fresh tags** (#573 / PR #575). Pre-fix `release.yml`'s `Verify CI passed` job used `lewagon/wait-on-check-action@v1.3.4` to wait for `check-name: ci-success` on the tag's SHA. The action queries check-runs for the SHA, filters by check name, and exits within seconds with `"The requested check was never run against this ref"` if no match — confusing "not yet registered" with "won't happen". On every fresh tag push made immediately after merging the prep PR, `ci-success` (which depends on `rust-checks` / `windows-build` / `macos-build` / `supply-chain` and only registers once those start completing) hadn't been registered yet, and the verify step exited 1 within ~2 seconds. Real evidence: pushing the v8.3.11 tag at 17:45 PDT this evening hit exactly that path, and the workaround was `gh workflow run release.yml -f tag=v8.3.11` after `main`'s `ci-success` went green by hand — which is why v8.3.11 binaries didn't appear on the release page until 00:53 UTC. Post-fix the verify step uses `fountainhead/action-wait-for-check@v1.2.0` (pinned to commit SHA `5a908a2` to match the existing convention in this workflow), which polls until a completed check named `ci-success` actually appears on the SHA before returning its conclusion. The new action only reports the result via an output (`conclusion`), so a follow-up step explicitly fails the job when the conclusion is anything other than `success`. v8.3.12's own tag is the in-anger smoke test for this fix.

### Non-blocking, carried forward

- **RC-4 Part B** (#504) — Cursor Usage API auth root-cause; Part A shipped with `v8.3.1`.
- **ADR-0090 supersede** — pending one release cycle of live validation on the now-working `cursorDiskKV` bubbles path before the Usage API §1 surface can be retired.
- **Detached daemon log capture** — first post-`budi update` daemon's startup lines don't land in `~/Library/Logs/budi-daemon.log` until the next launchctl kickstart. Observability-only; carried from v8.3.6 / v8.3.7.

## 8.3.11 — 2026-04-29

8.3.11 is a single-bug correctness patch for the cloud session-summary
sync path. After the org-switch / watermark recovery work in `v8.3.10`,
a separate dogfood divergence surfaced: cost / tokens / messages totals
kept landing on the cloud (Overview was correct, "Synced 2m ago" was
green), but `/dashboard/sessions` silently returned zero rows for any
window that excluded the last cursor-only sliver of activity. Root
cause was on the *local* side, not the cloud — claude_code and codex
sessions never had `started_at` populated, so the cloud-sync predicate
filtered them out forever. No new ingest / pricing behavior, no ADR
amendments, no wire changes.

### Fixed

- **`session_summaries` no longer goes silently empty for claude_code / codex sessions** (#569 / PR #570). Pre-fix the message-ingest path in `crates/budi-core/src/analytics/mod.rs` inserted stub `sessions` rows with only `(id, provider)` when it discovered a new `session_id` while ingesting messages — `started_at` and `ended_at` stayed `NULL`. The only production code path that populated `started_at` was `crates/budi-core/src/providers/cursor.rs`'s `cursor_session_contexts` backfill, so claude_code and codex sessions were stranded with `started_at IS NULL` permanently. `cloud_sync::fetch_session_summaries` filters with `WHERE s.started_at > ?1 OR s.ended_at > ?1 OR (s.ended_at IS NULL AND s.started_at IS NOT NULL)` — every clause requires one of the timestamps to be NOT NULL — so those rows were silently skipped on every sync, forever. The dashboard hid this because `message_rollups_daily` (which feeds `daily_rollups`) is populated by SQLite triggers directly from `messages` and doesn't depend on `sessions.started_at`, so cost / token / message totals kept flowing on Overview while Sessions emptied. Real evidence from a live DB the day this was filed: `983` claude_code + `67` codex session rows with `COUNT(started_at) = 0`, including 73 active claude_code sessions whose messages spanned the four days after the last cursor row that fed cloud sync. Cloud-side smoke alarm shipped in `budi-cloud` PR #86. Post-fix three idempotent layers heal both new and existing rows: per-batch ingest now COALESCE-fills `started_at`/`ended_at` from `MIN(timestamp)` / `MAX(timestamp)` of the matching `messages` immediately after the `INSERT OR IGNORE` (so freshly-discovered sessions are visible to cloud sync atomically); a standalone `migration::backfill_session_timestamps_from_messages` repair pass runs after every successful sync tick in `analytics::sync` (heals legacy stranded rows the moment the daemon ticks even if no new messages for them arrive); the same repair runs once on boot from `migration::reconcile_schema` so user DBs that already accumulated thousands of NULL-timestamp rows get fixed without waiting for fresh ingest on every stranded session. `COALESCE` keeps cursor's authoritative composer-header timestamps intact when both passes run. Pinned by 4 new unit tests covering: fresh ingest populates timestamps for non-cursor providers, ingest-time heal of pre-existing stranded rows when fresh messages arrive, the standalone repair pass heals stranded rows for both `claude_code` and `codex` and is idempotent on re-run, and `COALESCE` preserves a session row whose timestamps were already populated by cursor's repair.

### Non-blocking, carried forward

- **RC-4 Part B** (#504) — Cursor Usage API auth root-cause; Part A shipped with `v8.3.1`.
- **ADR-0090 supersede** — pending one release cycle of live validation on the now-working `cursorDiskKV` bubbles path before the Usage API §1 surface can be retired.
- **Detached daemon log capture** — first post-`budi update` daemon's startup lines don't land in `~/Library/Logs/budi-daemon.log` until the next launchctl kickstart. Observability-only; carried from v8.3.6 / v8.3.7.

## 8.3.10 — 2026-04-28

8.3.10 closes out the "after an org switch on the cloud, my daemon is
in a weird state" arc that started with #559 / #560 in `v8.3.9`. Those
two patches fixed the *future-tick* case (rotated key now propagates,
re-link UX is sane); this release adds the *past-history* recovery
hatch — once the cloud has lost the rows the daemon already pushed,
the local watermark needs to be reset so the next sync re-uploads
everything. Independently reproduced by Stacy on a fresh `v8.3.9`
install the day this was filed. No new ingest / pricing behavior, no
ADR amendments.

### Fixed

- **`budi cloud reset` re-uploads every local rollup + session summary after the cloud loses them** (#564 / PR #567, with #566 closed as a duplicate). Pre-fix the daemon's cloud sync was watermark-incremental and the watermark was org-blind: after an org switch (or device_id rotation, or cloud-side data wipe) the cloud no longer had the rows the watermark *implied* it had, but the next `sync_tick` still queried for `bucket_day > local_watermark` and only sent today's bucket. Real evidence from the dogfood session that surfaced this: 1,856 local rollups spanning 2025-08-27 → today, cloud `/v1/ingest/status` reporting `total_rollup_records: 26`, cloud Ivan-row at `$55.01` for `days=7` while local `-p 7d` showed `$1,913.82`. Post-fix `budi cloud reset` drops the three sentinel rows in `sync_state` (`__budi_cloud_sync__`, `__budi_cloud_sync___value`, `__budi_cloud_sync_sessions__`) so the next `budi cloud sync` falls into the no-watermark path of `fetch_daily_rollups` / `fetch_session_summaries` and re-uploads everything; cloud-side dedup (ADR-0083 §6) keeps the re-upload safe even when records overlap with rows the cloud already has. Routes through the daemon's new `POST /cloud/reset` (loopback-protected, takes the same `cloud_syncing` busy flag as `/cloud/sync` so a manual reset can never race a concurrent envelope build that already read the about-to-be-deleted watermark). The CLI prompts in a TTY and names the linked org so the user can sanity-check the target before re-uploading; non-TTY callers (CI, scripts) need `--yes` so a stray invocation can never silently re-upload. Pinned by 5 new unit tests covering the watermark-drop helper (idempotent + scoped — ingestion offsets / `__budi_sync_completed__` survive, regression guard against accidentally re-importing every transcript), the clap-level subcommand parse, and the prompt-copy fallbacks for missing / empty `org_id`.

### Docs

- **README rework** (#565). Tighter structure with prominent ecosystem links to `siropkin/budi-cloud` and `siropkin/budi-cursor`. No CLI / wire / data-shape changes — pure docs.

### Non-blocking, carried forward

- **RC-4 Part B** (#504) — Cursor Usage API auth root-cause; Part A shipped with `v8.3.1`.
- **ADR-0090 supersede** — pending one release cycle of live validation on the now-working `cursorDiskKV` bubbles path before the Usage API §1 surface can be retired.
- **Detached daemon log capture** — first post-`budi update` daemon's startup lines don't land in `~/Library/Logs/budi-daemon.log` until the next launchctl kickstart. Observability-only; carried from v8.3.6 / v8.3.7.

## 8.3.9 — 2026-04-27

8.3.9 is a cloud-rotation-correctness patch release on top of `v8.3.8`.
Two paired tickets shipped together: the user-facing `budi cloud init`
UX for re-linking after an API key rotation, and the daemon-side bug
that kept the rotation from actually working without a process
restart. Both surfaced from a single dogfood session immediately after
the cross-org switch flow shipped in `budi-cloud` PR #73 — the cloud
side worked, but the local CLI told users they "did something wrong"
and the daemon kept POSTing the stale Bearer token until restart. No
pivot, no new pricing / ingest behavior, no ADR amendments.

### Fixed

- **`budi cloud sync` now picks up a rotated `api_key` on the next tick instead of 401-ing indefinitely until daemon restart** (#560 / PR #562). Pre-fix the cloud sync worker captured `CloudConfig` at daemon startup and reused that clone for every tick — `let cfg = config.clone()` in `crates/budi-daemon/src/workers/cloud_sync.rs` always rebuilt the `Authorization: Bearer …` header from the value cached at boot. The auth-failed recovery branch *did* re-load `cloud.toml` via `load_cloud_config()`, but only to flip its own `auth_failed` flag — the next `sync_tick` still ran against the captured-at-startup config, so a key rotation produced 401s every 5 minutes indefinitely. Real evidence from a dogfood session today: daemon up at 22:33:57 PDT on 2026-04-23, last successful sync at 10:10:41 PDT, `cloud.toml` rewritten by `cloud init --force` at 10:15:42, first 401 at 10:15:41, 401 every interval thereafter for hours. Post-fix the worker re-reads `cloud.toml` at the top of every loop iteration so a rewritten `api_key` / `endpoint` / `org_id` / `device_id` propagates without a daemon restart; the on-disk read is a small TOML parse — cheap at the default 5-minute interval. The previous "Cloud config refreshed, resuming sync" log (which fired every retry whether or not anything changed) gets replaced with a credential-diff'd `"Cloud credentials changed on disk; resuming sync"` line that only emits when the `api_key` or `endpoint` actually changed on disk, gated by a new `credentials_changed(prev_api_key, prev_endpoint, fresh)` helper covered by 5 unit tests (rotation, endpoint swap, no-op retry, cold start, key removed). `initial_config` is now documented as "first-tick interval only" so the captured-at-startup confusion doesn't recur.

- **`budi cloud init` re-link path no longer reads as "you did something wrong"** (#559 / PR #561). Pre-fix re-running `budi cloud init --api-key NEW_KEY` against an existing `cloud.toml` (org switch via the cross-org flow shipped in `budi-cloud` PR #73, manager-driven key rotation, lost-device re-provision) bailed with the bare *"already exists. Pass --force to overwrite (existing settings will be replaced)"* error — a path that's now expected enough to deserve real ergonomics. Post-fix the CLI prompts in a TTY and surfaces a rotation-aware error in non-TTY: when `--api-key KEY` is supplied interactively against an existing `cloud.toml`, the new `confirm_relink` prompt names the org currently linked (`"~/.config/budi/cloud.toml already points to org \"org_xEvtA\". Replace with the key you just supplied? [y/N]"`) so the user can sanity-check what they're about to overwrite before confirming. CI / scripted callers (no TTY) hit a new `rotation_aware_already_exists_error` that names the existing org and explicitly mentions `--force` as the right escape hatch for the org-switch / key-rotation case, instead of the bare "Pass --force to overwrite" wording that pre-#559 users read as a blame line. `--force` keeps working unchanged as the non-interactive escape hatch for CI / scripted callers, with the existing `--yes` requirement to silence the overwrite confirmation when a real (non-stub) key is being replaced. Pinned by 4 new unit tests on `describe_existing_link` / `rotation_aware_already_exists_error` plus a new e2e regression `scripts/e2e/test_559_cloud_init_relink_ux.sh` that asserts the rotation-aware error wording, points at `--force`, and confirms `--force --yes` still rotates cleanly.

### Non-blocking, carried forward

- **RC-4 Part B** (#504) — Cursor Usage API auth root-cause; Part A shipped with `v8.3.1`.
- **ADR-0090 supersede** — pending one release cycle of live validation on the now-working `cursorDiskKV` bubbles path before the Usage API §1 surface can be retired.
- **Detached daemon log capture** — first post-`budi update` daemon's startup lines don't land in `~/Library/Logs/budi-daemon.log` until the next launchctl kickstart. Observability-only; carried from v8.3.6 / v8.3.7.

## 8.3.8 — 2026-04-23

8.3.8 is a same-day follow-up on `v8.3.7` that lands the real fix for
#553. The v8.3.7 post-tag live-smoke on the maintainer machine showed
that the `cursorDiskKV` bubble reader lined up against the schema the
[CodeBurn reference](https://github.com/getagentseal/codeburn/blob/main/src/providers/cursor.ts)
documented, which turns out to diverge from what Cursor actually
writes on disk: only 7 of 1,565 token-bearing bubble rows parsed, so
`budi stats` still read like the pre-fix symptom the release was
supposed to eliminate. No pivot, no new pricing primitive, no ADR
supersede.

### Fixed

- **Cursor bubble reader now matches the real `state.vscdb` schema** (#553 follow-up / PR #557). Pre-fix v8.3.7's `read_cursor_bubbles` dropped bubble rows on two guards that read against the wrong JSON paths: (a) it filtered on `json_extract(value, '$.conversationId') != ''`, but Cursor never writes `$.conversationId` into the value — the conversation id is embedded in the row KEY, shaped `bubbleId:<36-char conv-uuid>:<36-char bubble-uuid>` (every key observed on the maintainer DB is exactly 82 chars); (b) it required `$.createdAt` to be present, but 131 of 1,565 token-bearing bubbles carry no `createdAt` and every `type=1` user-role bubble is missing it too. Post-fix the SQL parses `conversation_id` from `substr(key, 10, 36)` and `bubble_id` from `substr(key, 47, 36)` directly, gated on `length(key) = 82` so malformed keys are rejected before reaching the Rust decoder. New `load_bubble_timestamp_fallbacks` reads `ItemTable.composer.composerHeaders` once per call and builds a `conversation_id -> last_updated_at_ms` map so bubbles without `$.createdAt` fall back to the composer-level timestamp (day-bucket attribution stays correct, sub-minute ordering within a conversation may not match Cursor's UI). Bubbles with neither an explicit `createdAt` nor a composer-header match are dropped rather than invented at `Utc::now()` so they don't pollute today's totals. Dedup uuid shape changed from `cursor:bubble:<conv>:<created_ms>:<input>:<output>` to `cursor:bubble:<conv>:<bubble>` — uniqueness now comes from the row key itself, so two assistant bubbles with identical token counts in the same conversation can't collide. `budi db import --force` is the expected recovery path after `budi update` so v8.3.7-era bubbles re-ingest under the new uuid shape.

### Non-blocking, carried forward

- **RC-4 Part B** (#504) — Cursor Usage API auth root-cause; Part A shipped with `v8.3.1`.
- **ADR-0090 supersede** — pending one release cycle of live validation on the now-working `cursorDiskKV` bubbles path before the Usage API §1 surface can be retired.
- **Detached daemon log capture** — first post-`budi update` daemon's startup lines don't land in `~/Library/Logs/budi-daemon.log` until the next launchctl kickstart. Observability-only; carried from v8.3.6 / v8.3.7.

## 8.3.7 — 2026-04-23

8.3.7 is a second dogfood-driven patch release on top of `v8.3.6`. The
v8.3.6 post-tag dogfood surfaced one high-signal correctness bug
(Cursor pricing was an order of magnitude low because the only path
that priced rows was the Usage API, which returns overage-only
events) and one cloud-UX gap (the Devices page had nothing
human-friendly to render). Both ship here. No pivot, no new pricing
primitive, no ADR supersede.

### Fixed

- **Cursor subscription-included traffic is now priced locally instead of reading as $0** (#553). Pre-fix `budi stats -p today` showed Claude Code at $254 and Cursor at $16 after a day of comparable keyboard time across both tools — Cursor's JSONL shipped neither tokens nor model, and its dashboard Usage API at `/api/dashboard/get-filtered-usage-events` (documented in ADR-0090 §1) returns only billable overage events, so every subscription-included request read as $0. New local path reads `state.vscdb::cursorDiskKV` rows keyed `bubbleId:*` directly: Cursor persists real `tokenCount.inputTokens` / `tokenCount.outputTokens` / `modelInfo.modelName` / `createdAt` / `conversationId` per message, and the `json_extract`-powered SELECT in `crates/budi-core/src/providers/cursor.rs` surfaces each bubble as a `ParsedMessage` that flows through the existing pipeline. `CostEnricher` prices them via the standard manifest, so `pricing_source` lands as `embedded:v*` / `manifest:v*` — no Cursor-specific pricing code. Auto-mode bubbles (`modelInfo.modelName` empty or the literal `"default"`) rewrite to `claude-sonnet-4-5` via the new `CURSOR_AUTO_MODEL_FALLBACK` constant, matching Cursor's public "Auto ≈ Sonnet rates" statement and the [CodeBurn](https://github.com/getagentseal/codeburn/blob/main/src/providers/cursor.ts) reference implementation's convention. Deterministic row id `cursor:bubble:<conversationId>:<createdAt>:<inputTokens>:<outputTokens>` dedups bubbles against Usage API events describing the same activity; `INSERT OR IGNORE` keeps first-seen. Schema defense: missing `cursorDiskKV` table → one-time `cursor_bubble_schema_unrecognized` warn + `Ok(vec![])` so the sync tick still proceeds to the Usage API fallback. Dual-path coexistence during the validation window: the bubbles path advances a NEW watermark key `cursor-bubbles`, the existing Usage API path keeps its `cursor-api-usage` watermark; both run in the same `sync_direct` tick and advance independently. Semantic note now documented in the ADR-0090 amendment: the resulting Cursor number is list-price consumption, not a literal Cursor bill — same framing every other Budi provider surfaces, so cross-provider stats stay comparable. ADR-0090 gets a dated amendment pointing at the bubbles-first path; no supersede until one release cycle of live validation.

### Added

- **Cloud ingest envelope now includes a human-friendly `label` field** (#552). Pre-fix the budi-cloud dashboard's Devices page (siropkin/budi-cloud#58) had nothing readable to render per row and fell back to a truncated `dev_<id>` — the ingest envelope never carried anything device-level beyond the opaque id. Post-fix `SyncEnvelope` gains a `label: String` field populated on every ingest tick via `CloudConfig::effective_label`, sourced from `~/.config/budi/cloud.toml [cloud] label`. Precedence: key missing → local OS hostname via `pipeline::enrichers::get_hostname` (now `pub(crate)`; same resolver already used for message-level identity tags, so hostname renames propagate consistently across callers); key present including `label = ""` → value sent verbatim. Empty string is the documented opt-out — raw hostnames can be PII (ADR-0083), and the opt-out has to be edit-one-line simple. `budi cloud init` template gains a commented-out `label = "ivan-mbp"` hint so users discover the option without grepping the source. Paired cloud-side ticket siropkin/budi-cloud#60 persists the label on auto-register + subsequent ingests and renders it on `/dashboard/devices`.

### Non-blocking, carried forward

- **RC-4 Part B** (#504) — Cursor Usage API auth root-cause; Part A shipped with `v8.3.1`.
- **ADR-0090 supersede** — pending one release cycle of live validation on the `cursorDiskKV` bubbles path before the Usage API §1 surface can be retired; ADR-0090's 2026-04-23 amendment documents the dual-path policy in the meantime.
- **Detached daemon log capture** — daemons respawned directly by `budi update` run as a detached child whose stdout isn't captured by launchd's `StandardOutPath`, so the first post-update daemon's startup lines don't land in `~/Library/Logs/budi-daemon.log` until the next launchctl kickstart. Observability-only; carried from v8.3.6.

## 8.3.6 — 2026-04-23

8.3.6 is a first-user-feedback-driven patch release on top of
`v8.3.5`. Two items from the maintainer's own v8.3.5 install-smoke
got filed as bugs and shipped within the same session: the statusline
was subtracting context info from Claude Code's default, and `budi
init` didn't cover the historical-import step most fresh users
expected to happen automatically. No pivot, no new pricing / ingest
behavior, no ADR amendments.

### Fixed

- **`budi statusline --format claude` renders Claude-Code-default-equivalent context before the budi cost line** (#546). Pre-fix the Claude-format output was just `budi · $X 1d · $Y 7d · $Z 30d`; installing it into `statusLine.command = "budi statusline"` replaced Claude Code's built-in statusline (model / working directory / git branch) with only budi's cost display. First-user report: "user had some claude code additional info in statusline ... we removed that and replace with budi". Post-fix the line prepends `<model> · <short-cwd> · <branch>` extracted from the Claude Code stdin envelope + local git state, so installing budi ADDS cost info without losing Claude Code's context. New helpers in `crates/budi-cli/src/commands/statusline.rs`: `extract_model_name` reads `model.display_name` / falls back to `model.id`; `short_display_path` normalizes `$HOME` → `~` and keeps the last two path segments to avoid blowing out the prompt width; `render_context_prefix` drops each field individually when missing and returns `None` when all three are absent so the offline / daemon-down branch still renders legibly. The `apply_statusline` merge-append path (user already had a non-budi `statusLine.command`) is unchanged and now has a pinned regression test. Starship / Custom / JSON formats are unaffected.

### Added

- **`budi init` auto-imports historical transcripts** (#548). Pre-fix after `budi init` a fresh user ran `budi stats -p 30d` and saw `$0.00` because the tailer only ingests new activity — historical Claude Code / Cursor / Codex / Copilot transcripts on disk stayed invisible until a separate `budi db import` call. First-user report: "how to run history import with budi? i thought we do it with budi init" + "i would say auto import, too many initial command then". Post-fix `cmd_init` calls `cmd_import(false, false)` between `install_default_integrations` and the final banner. The existing sync path is idempotent (per-path offset table skips already-seen messages by id) so repeat inits are fast no-ops reporting "0 new messages" per provider. Skipped under `--no-daemon` since the import routes through `/sync/*` on the daemon we didn't start. Import failure is warn-only — init exit code stays 0 with a pointer to retry via `budi db import`.

### Non-blocking, carried forward

- **RC-4 Part B** (#504) — Cursor Usage API auth root-cause; Part A shipped with `v8.3.1`.
- **Detached daemon log capture** — daemons respawned directly by `budi update` run as a detached child whose stdout isn't captured by launchd's `StandardOutPath`, so the first post-update daemon's startup lines (including the #540 `cloud uploader` line) don't land in `~/Library/Logs/budi-daemon.log` until the next launchctl kickstart. Observed during the v8.3.5 post-tag smoke, documented in the v8.3.5 closing evidence comment; affects observability only (the daemon IS running correctly).

## 8.3.5 — 2026-04-23

8.3.5 is a cloud-quality-of-life patch release on top of `v8.3.4`. Two
user-requested tickets land: daemon startup now logs whether the
cloud uploader is running and why not, and `budi cloud init --api-key`
auto-seeds both identity fields via a new `GET /v1/whoami` round-trip
so a fresh user never has to hand-copy `org_id` out of the dashboard.
No pivot, no new pricing / ingest behavior, no ADR amendments, no
proxy reintroduction.

### Added

- **`budi cloud init --api-key KEY` auto-seeds `device_id` and `org_id`** (#541). Pre-fix the command wrote the api_key and flipped `enabled = true` but left `device_id` / `org_id` as commented placeholders, forcing the user through a six-step manual ritual (open dashboard → Settings → hand-copy `org_xxx` → paste into `cloud.toml` → uncomment → re-run) before `budi cloud sync` would do anything. Post-fix the CLI calls the new `GET /v1/whoami` endpoint on `app.getbudi.dev` (shipped as `siropkin/budi-cloud#56`) to resolve the `org_id` for the just-pasted key, generates a fresh UUID v4 for `device_id`, and writes both fields as real TOML assignments. `budi cloud init` prints a "Seeded cloud identity" block with each field's provenance (`generated` / `from /v1/whoami` / `from flag`) so the user can eyeball the result. Error taxonomy — `Unauthorized` reverts the template to `enabled = false` + stub key and tells the user, `EndpointAbsent` (404/405) and transient network / 5xx errors fall through to the pre-#541 commented-placeholder shape with a `!` warning so a self-hosted cloud without `/v1/whoami` or a cloud outage doesn't block `budi cloud init` from writing a config file. Two new escape-hatch flags `--device-id <ID>` and `--org-id <ID>` bypass the UUID v4 generation / whoami call respectively — useful for multi-machine setups and offline / self-hosted installs.
- **Daemon boot emits exactly one `cloud uploader …` INFO line regardless of enabled state** (#540). Pre-fix the daemon silently skipped the uploader spawn when `cloud.enabled = false`, when `api_key` was still the placeholder, or when `device_id`/`org_id` were missing — a reader tailing `daemon.log` saw only ingest / pricing lines and had to cross-check `cloud.toml` to confirm. Post-fix the cloud-startup block always emits `cloud uploader configured endpoint=… device_id=… org_id=… interval_s=…` when ready, or `cloud uploader disabled reason="<tag>"` otherwise. The `reason` taxonomy mirrors the precedence `budi cloud status` uses (`cloud.enabled=false` → `missing api_key` → `api_key is placeholder` → `missing device_id` → `missing org_id`), sourced from a new `CloudConfig::disabled_reason()` on `budi-core`. `device_id` / `org_id` are abbreviated to the first 8 chars plus `…` via a new `log_id_prefix` helper (char-boundary-safe for non-ASCII ids); `api_key` is never logged.

### Non-blocking, carried forward

- **RC-4 Part B** (#504) — Cursor Usage API auth root-cause. Part A shipped with `v8.3.1`; Part B needs maintainer credential-level probing that can't be driven from CI.

## 8.3.4 — 2026-04-23

8.3.4 is an internal-correctness + maintainer-quality-of-life patch
release on top of `v8.3.3`. Two fixes surfaced during the v8.3.3
post-tag smoke: one DB-label correctness issue for anyone auditing
`messages.pricing_source` directly, and one CI-infrastructure
regression that was silently disabling supply-chain coverage. No
pivot, no new features, no ADR amendments, no proxy reintroduction,
no new runtime network destinations.

### Fixed

- **`pricing_source = "unpriced:no_tokens"` for zero-token rows instead of `"legacy:pre-manifest"`** (#533). Pre-fix every user-role message and every row ingested with zero tokens in every lane was landing with `pricing_source = "legacy:pre-manifest"` — the DB `DEFAULT` that [ADR-0091 §4](docs/adr/0091-model-pricing-manifest-source-of-truth.md) reserves for rows that existed BEFORE the 8.3.0 migration. `CostEnricher::enrich` in `crates/budi-core/src/pipeline/enrichers.rs` only set `msg.pricing_source` for `role == "assistant"`; non-assistant rows kept `None` and fell through to the COALESCE at `crates/budi-core/src/analytics/mod.rs:488`. On a live 8.3.3 DB this mislabeled 81,946 Claude Code and 1,516 Cursor rows, including rows timestamped today — anyone auditing `SELECT pricing_source, COUNT(*) FROM messages` saw a confusing mix of real pre-migration rows and post-migration zero-token rows. New column literal `COLUMN_VALUE_UNPRICED_NO_TOKENS = "unpriced:no_tokens"` in `crates/budi-core/src/pricing/mod.rs`, alongside the existing `"unknown"` / `"upstream:api"` sentinels. `PricingSource::parse_column` returns `None` for the new literal (same contract as the others). `CostEnricher::enrich` now tags any row with `pricing_source == None && cost_cents == None && all_token_lanes == 0` with the new sentinel. No backfill — [ADR-0091 §5 Rule C](docs/adr/0091-model-pricing-manifest-source-of-truth.md) reserves existing `"legacy:pre-manifest"` rows from automated rewrites; the fix only applies to rows ingested by 8.3.4+. Zero impact on displayed cost numbers (these rows correctly have `NULL` cost — nothing to price).

### Changed (CI / maintainer)

- **`supply-chain` job replaces the `EmbarkStudios/cargo-deny-action` wrapper with a direct `cargo install cargo-deny --locked` + `cargo deny --all-features check`** (#536). Pre-fix the action failed on every PR with `failed to get 'FETCH_HEAD' metadata: failed to parse ISO-8601 timestamp 'fatal: cannot change to '/home/runner/.cargo/advisory-dbs/advisory-db-<hash>': No such file or directory'` — the action's internal gix-based advisory-db cache couldn't bootstrap its directory layout on fresh runners, and v2.0.17 is the latest tag so there was no upgrade path. Direct install bypasses the wrapper entirely; cargo-deny manages its own advisory-db in `~/.cargo/advisory-db` (same path cargo-audit uses in the `rust-checks` job, which has been working reliably). RUSTSEC coverage was already intact via cargo-audit in `rust-checks`, but license / ban / duplicate checking was silently off. Job stays `continue-on-error: true` for this release cycle per the existing policy comment; a follow-up can drop that flag in 8.3.5 once the fix has been observed stable.

### Non-blocking, carried forward

- **RC-4 Part B** (#504) — Cursor Usage API auth root-cause. Part A shipped with `v8.3.1`; Part B needs maintainer credential-level probing that can't be driven from CI.
- **`siropkin/budi-cloud#37`** — fresh-eyes UX audit of `app.getbudi.dev`.

## 8.3.3 — 2026-04-23

8.3.3 is a single-ticket patch release. The post-`v8.3.2` update
smoke surfaced one post-install drift bug in `budi update`; this
release lands that fix. No pivot, no new features, no ADR
amendments, no proxy reintroduction, no new runtime network
destinations.

### Fixed

- **`budi update` no longer leaves the daemon pinned to the pre-install version until the next `budi init`** (#529). Pre-fix `cmd_update` called `ensure_daemon_running_with_binary` at the end of its run; that helper compares `/health.version` against `env!("CARGO_PKG_VERSION")` of the CURRENTLY-RUNNING CLI process — which during `budi update` is still the PRE-install binary. The pre-install daemon matched the pre-install CLI, no restart fired, and `/health` kept reporting the old version until the next `budi init` / `budi doctor` happened to respawn it. The post-install smoke-check `verify_installed_version` correctly flagged the drift as `Expected v8.3.2, but detected version is: budi 8.3.1`, but the user had to manually nudge the daemon to catch up. New `restart_daemon_for_version_upgrade(expected)` in `crates/budi-cli/src/daemon.rs` takes the just-installed version as an explicit argument, compares `/health` against THAT (not against the running CLI), and on mismatch kills every `budi-daemon` process, waits for the port to release, and hands off to `ensure_daemon_running_with_binary` to spawn the new binary. `daemon_version_matches` now delegates to a shared `daemon_version_equals(expected)` helper so both entry points share the same health-probe logic. Bootstrap-paradox caveat: the fix only activates on updates FROM a binary that contains it, so `v8.3.2 → v8.3.3` will still hit the pre-fix drift once; every subsequent `budi update` then lands with `/health` reporting the new version immediately.

### Non-blocking, carried forward

- **RC-4 Part B** (#504) — Cursor Usage API auth root-cause. Part A shipped with `v8.3.1` (structured `cursor_auth_skipped` warn per reason). Part B needs maintainer credential-level probing that can't be driven from CI; re-homes to the next milestone.
- **`siropkin/budi-cloud#37`** — fresh-eyes UX audit of `app.getbudi.dev`. 11 findings across setup-flow / account-management / data-shape clarity. Closes independently of the core repo.

## 8.3.2 — 2026-04-23

8.3.2 is the second post-tag hardening train after `v8.3.1`. Every
ticket in this milestone came out of the `v8.3.1` post-tag smoke
on 2026-04-22: a fresh-user audit of the cloud connect flow, a
cold run of `budi update` against a just-published tap bump, and a
closer-look at the summary cost reconciliation. No new features,
no ADR amendments, no proxy reintroduction, no new runtime network
destinations.

### Fixed

- **`budi cloud init` + `budi init` complete the documented fresh-user connect flow** (#521). Pre-fix the template comment in `~/.config/budi/cloud.toml` promised "\`budi init\` seeds `device_id` / `org_id` automatically on a real enable" but the seeding logic was never wired up in `crates/budi-cli/src/commands/init.rs`. Users who followed the three-step `budi cloud init --api-key <k>` → `budi init` → `budi cloud status` flow landed on `state: enabled but not fully configured` with no indication of what to do next. New `seed_cloud_device_id_if_needed` in `budi-core::config` auto-generates a UUID v4 and writes it into the template's commented `# device_id = "your-device-id"` slot when cloud is opted-in and `api_key` is non-stub; idempotent (returns `AlreadySet` on repeat runs). `budi init` calls the seeder and emits a single `✓ Cloud device_id seeded (<uuid-prefix>…)` line plus a follow-up nudge for `org_id` when it's still missing. The template comment is rewritten to match — no longer promises seeding that doesn't happen. Daemon `/cloud/sync` + `/cloud/status` responses now surface a per-field missing-fields message (`api_key` → paste from Settings; `device_id` → run `budi init`; `org_id` → copy from Organization panel) via a new `missing_fields_message` helper instead of the pre-fix generic line that listed every possible gap.
- **`budi update` upgrades in one invocation right after a tap bump** (#517). Pre-fix `run_brew_upgrade` called `brew upgrade budi` directly and relied on Homebrew's async auto-update to refresh the tap index. During the `v8.3.1` post-tag smoke the first `budi update --yes` invocation hit a stale tap and reported "Warning: siropkin/budi/budi 8.3.0 already installed"; a second invocation a minute later succeeded because brew's background auto-update had refreshed the tap between runs. Prepend `brew update --quiet` before `brew upgrade budi`. `--quiet` keeps the output readable; failures are non-fatal (falls through to `brew upgrade` which either succeeds against a cached index or surfaces its own error). No behavior change for the standalone-install path.
- **`TODAY-7` no longer surfaces as a ticket in `budi stats --tickets`** (#518). Branch `v8/502-fix-flaky-today-7d-30d-reconcile-test` (the D-4 fix branch from the 8.3.1 train itself) produced a `TODAY-7` row because `TODAY` was absent from the #499 denylist added in 8.3.1. Add `TODAY` to `DENYLISTED_TICKET_PREFIXES`; the existing idempotent startup backfill from #499 cleans up existing `TODAY-*` tags on the next daemon restart. Numeric fallback correctly picks up the real ticket id (`502` in this case) from the `v8/NNN-...` slug.
- **`budi vitals --session <ambiguous-prefix>` returns 400 with the daemon's message instead of generic 500** (#519). Pre-fix `resolve_sid` routed every `analytics::resolve_session_id` error through the generic `internal_error` → 500 wrapper, so `budi vitals --session 6` against a DB with multiple `6*`-prefixed sessions returned a useless `Error: Daemon returned 500 Internal Server Error` instead of the daemon's actionable `ambiguous session prefix '6' — matches multiple sessions; use more characters`. String-match on the anyhow chain routes ambiguity to `bad_request()` (400) with the daemon message surfaced verbatim. Full-UUID / unique short prefix / no-match paths unchanged.
- **`budi stats` summary sub-line reconciles to the top-line cost under Cursor fast-mode / thinking-token spend** (#520). Pre-fix on the maintainer's machine `-p 30d` showed `Est. cost $3,915.47` but the four-component sub-line (`input / output / cache write / cache read`) summed to $3,800.47 — a $115 gap. `total_cost` is `SUM(cost_cents)` from ingest (authoritative, includes Cursor fast-mode / thinking / web-search contributions); the four components are re-derived from base-token sums × manifest rates. New `CostEstimate.other_cost` field captures the residual `(total_cost − components_sum).max(0.0)`; the summary render grows a fifth `other $N.NN` cell when it's non-zero, stays at the pre-8.3.2 four-cell shape when zero. Serde field gets `#[serde(default)]` so pre-8.3.2 JSON bodies keep deserializing.

### Changed

- **`budi cloud` error messages name the exact missing field** (#521). The pre-fix generic "ensure api_key, device_id, and org_id are set" surface has been replaced by a per-field list that spells out where to get each value (Settings page vs. auto-generated by `budi init` vs. Organization panel). Mirrors the #446 `budi cloud status` three-shape split; consistent with the #443 / #445 "name the problem, name the fix" discipline.

### Non-blocking, deferred

- **RC-4 Part B** (#504) — Cursor Usage API auth root-cause. Part A (#514, structured `cursor_auth_skipped` warn per reason) shipped with `v8.3.1`. Part B needs maintainer credential-level probing that can't be driven from CI; stays open under the next milestone.
- **`siropkin/budi-cloud#37`** — fresh-eyes UX audit of `app.getbudi.dev`. 11 findings across setup-flow / account-management / data-shape clarity. Closes independently of the core repo.

## 8.3.1 — 2026-04-22

8.3.1 is the post-tag hardening train on top of `v8.3.0`. It closes
release-candidate gaps surfaced by the 2026-04-22 fresh-user smoke
audit plus the dogfooding findings from a 30-day live walk on the
shipped binary. No re-scope, no new features, no ADR amendments
except the row-level-rejection change called out in §2 of
[ADR-0091](docs/adr/0091-model-pricing-manifest-source-of-truth.md).

### Fixed

- **Pricing manifest refresh: one bad upstream row no longer blocks the whole payload** (#483 / [ADR-0091 §2 amendment](docs/adr/0091-model-pricing-manifest-source-of-truth.md)) — on 2026-04-22 LiteLLM upstream added `wandb/Qwen/Qwen3-Coder-480B-A35B-Instruct` at `$100,000/M`; the pre-8.3.1 validator rejected the whole payload on the sanity ceiling, pinning every `v8.3.0` user to the embedded baseline until LiteLLM patched. The refresher now partitions rows: NaN, negative, or over-$1,000/M prices are dropped from the installed manifest (kept deterministically sorted for reproducible log/pricing-status output), and the rest of the payload still refreshes. Dropped rows surface on `GET /pricing/status` and in `budi pricing status` under a new "Rejected upstream rows" section, and on the refresh response so `--refresh` prints them inline. Structured `rejected_upstream_row` warns go to the daemon log one per row. The ≥ 95% retention floor applies to the kept rows, so a mass upstream mispricing regression still hard-fails the tick. The $1,000/M ceiling is unchanged — still the right guardrail; only the blast radius of a single bad row is different. Daemon warm-load re-runs the sanity partition on restart so a bad row cached to disk mid-incident cannot re-admit itself into the in-memory lookup after a restart. Cache still persists the raw upstream bytes for audit / replay fidelity.
- **`budi stats` Agents block counts user + assistant so it sums to Total** (#482) — pre-8.3.1 the Agents block labeled its column `msgs` but counted assistant-only messages while the Total row counted both (`3050 messages (1680 user, 1370 assistant)`). A fresh reader seeing `Claude Code 1358 msgs` on top of `Total 3050 messages` naturally read it as "1358 of 3050" — wildly wrong. Per-provider `msgs` now renders `ProviderStats.total_messages` (user + assistant) so `sum(displayed providers.total_messages) == summary.total_messages` on the same screen. `/analytics/providers` gains `assistant_messages` (renamed from `message_count`, kept as a serde alias for back-compat), `user_messages`, and `total_messages` fields; `budi stats --format json` now includes a `providers[]` envelope so scripts can reconcile both ways.
- **`budi stats` reconciliation holds to the cent under fractional per-row costs** (#484) — the #448 reconciliation contract (`sum(rows.cost_cents) + other.cost_cents == total_cost_cents`) drifted by 1–22 ¢ on live data across 13 of 21 view × period cells. Root cause in `paginate_breakdown`: `total_cost_cents` summed all rows in order; `other.cost_cents` was a separate sum over the drained tail. On the wire the CLI then computed `sum(kept) + other.cost_cents` and compared to `total_cost_cents` — three f64 sums accumulated in different orders, different by the last ULP per breakdown and deeper on wider windows. Fixed by deriving `other.cost_cents` from `total_cost_cents − kept_cost` where `kept_cost` is computed AFTER drain, i.e. from the exact same `Vec<T>` the caller will see on the wire. New property test `breakdown_reconciles_under_fractional_per_row_costs` seeds 120 rows with `input_tokens × 15/M × jitter × 100` across every breakdown view × `today / 7d / 30d`, asserting the delta < 1/1000 cent.
- **`budi pricing status --refresh` renders validation failures as a classified headline + detail instead of a raw 502** (#493) — pre-8.3.1 a validation failure surfaced as `Error: Daemon returned 502 Bad Gateway: {"error":"validation rejected: ...","ok":false}`. The `Bad Gateway` read as a connectivity problem even though the daemon intentionally returned a structured body. The CLI client now short-circuits `check_response` on 502 when the body parses as `{ok: false, error: ...}` and passes the structured body up to `render_refresh_text`, which maps every `ValidationError::Display` shape to a friendly headline + detail pair plus a `budi pricing status` pointer. Exit code stays `2` on `--refresh` failure so CI scripts still branch on status without parsing the body.
- **`budi stats` Agents block: tokens cell always has a `tok` suffix, cost cells use fixed `$X,XXX.XX` precision** (#486, #494) — pre-fix Claude Code rendered `159.0M` (humanized, unit-baked-in) while Cursor rendered bare `0` (no unit) on the same column; the top-line `Est. cost` rendered `$126` while the component sub-line kept cents (`$0.04 / $19.92 / $32.36 / $74.08` = `$126.40`). Every cost cell in the summary block (top-line + component sub-line + cache-savings + per-provider) now uses `format_cost_cents_fixed`; every tokens cell carries an explicit ` tok` suffix matching the breakdown views.
- **`budi sessions list` ↔ `budi sessions <id>` health dot parity regression guard** (#497) — ticket reported the list painted a red dot while detail rendered `insufficient_data` (⚪) for the same session. Root-cause audit showed current `main` already routes both surfaces through the shared `overall_state()` aggregator with identical inputs — they agree by construction. This PR pins the parity contract with `health_list_detail_parity_across_fixture_shapes` so a future threshold / aggregator change can't silently regress it.
- **`budi vitals --session <short-uuid>` resolves the prefix the same way `budi sessions <id>` does** (#496) — pre-fix an 8-char session prefix returned INSUFFICIENT DATA because the session-health route did an exact-match join. The shared resolver `budi_core::analytics::resolve_session_id` already exists; wired into `GET /analytics/session-health` via the pre-existing `resolve_sid` helper. Full UUID still works; ambiguous prefix surfaces as a 500 with the daemon's "ambiguous session prefix 'X'" message; no-match prefix returns 404 instead of silent INSUFFICIENT DATA.
- **Ticket extraction: denylist generic housekeeping prefixes + idempotent backfill** (#499) — pre-fix `budi stats --tickets` on a real machine showed `SWEEP-2` (from `chore/dead-code-sweep-2`) and `ADR-0091` (co-extracted alongside the real `375` on `v8/375-adr-0091-pricing-manifest`). 22 generic prefixes added to a new `DENYLISTED_TICKET_PREFIXES` constant: `ADR / CEP / CHORE / DEMO / DRAFT / FIX / ISSUE / ITER / LIMIT / PASS / PR / REFACTOR / RFC / ROUND / STEP / SWEEP / TASK / TMP / TODO / V / VERSION / WIP`. `extract_ticket_alpha` skips denylisted candidates and keeps scanning so legitimate tickets later in the branch name still resolve. New `backfill_remove_denylisted_ticket_tags` runs on daemon startup and removes the `ticket_id` / `ticket_source` / `ticket_prefix` triplets for existing denylisted attributions; safe to re-run (idempotent).
- **Flaky `breakdown_tickets_reconcile_across_today_7d_and_30d` test anchors `now` to noon UTC** (#502) — the test used `Utc::now()` inside the `today` window, so CI runs at UTC midnight ± 1h dropped every today-cohort row out of the filter. Surfaced while RC-1's CI hit the flake at 00:07 UTC. Fixed by anchoring to `today_utc_date + 12:00:00`; now deterministic regardless of when the test runs.

### Changed

- **`budi` CLI surface polish**:
  - `--help` output is free of internal issue numbers / ADR refs / `RN.N` round labels (#495). A new CI rust-checks step renders `budi <cmd> --help` for every subcommand and fails if any match `#NNN|ADR-NNN|RN.N` patterns, so the class can't come back.
  - `budi stats --tag <KEY>` help prose makes the escape-hatch-filter semantics explicit (#491); value name renamed `<TAG>` → `<KEY>`.
  - `budi statusline --format text` now parses as an alias for the default `claude` format (#485) so a fresh user who reflexively types `--format text` gets the expected render.
  - `budi doctor --quiet` suppresses individual PASS lines on a green run while keeping WARN / FAIL lines + final summary visible (#487). Safe for CI gates; no change to the default verbose path.
- **`budi stats` summary-block precision**: Agents block tokens cell carries an explicit `tok` suffix; cost cells across the whole summary (top-line `Est. cost`, four-component sub-line, cache-savings, per-provider cost column) use `format_cost_cents_fixed` so `$126` never shadows `$126.40` on the same screen (#486 + #494).
- **Cursor Usage API auth failure now emits a structured warn-once** (#504 Part A) — `extract_cursor_auth` routes every early-return through a new `warn_auth_once(CursorAuthIssue::...)` helper with seven typed reasons (`no_state_vscdb` / `state_vscdb_open_failed` / `token_row_missing` / `token_empty` / `token_malformed` / `token_expired` / `token_missing_subject`). Operators grep `daemon.log` for `cursor_auth_skipped` and find exactly which failure mode is firing. Does NOT log the JWT. Part B (auth root cause on specific machines) tracks in #504 under milestone 8.3.2.

### Docs

- **`v8.3.0` release-page callout for 8.2.x upgraders** (#489) — applied post-tag via `gh release edit`: if you run `budi uninstall` on your 8.2.x binary before `brew upgrade`, the 8.2.x uninstaller misses `~/Library/Logs/budi-daemon.log` (launchd's `StandardOutPath` path outside the 8.2.x data-dir walk, fixed in 8.3 via #439). Workaround: `rm` the log after upgrade, or skip the uninstall and let `brew upgrade` replace the binary in place — the 8.3 daemon supersedes the old one cleanly.
- **ADR-0091 §2 amendment** — row-level rejection is now the documented contract. SOUL.md and README.md propagate the amended language in the same commit as #483's code change.

## 8.3.0 — 2026-04-22

8.3.0 is the pricing source-of-truth pivot, plus the deferred 8.2
`docs/` + `scripts/` hygiene sweep, plus the two audit passes filed
against `v8.2.1` on 2026-04-20. The four hand-maintained
`*_pricing_for_model()` functions are gone — pricing now flows
through a single `pricing::lookup` call against an embedded LiteLLM
baseline with a daily refresh against the upstream manifest, every
row tagged `pricing_source` so history is immutable and auditable
([ADR-0091](docs/adr/0091-model-pricing-manifest-source-of-truth.md)).
The 2026-04-20 fresh-user smoke pass closed two release-blockers
(`budi doctor` no longer false-FAILs on live tailer drift; the Cursor
marketplace extension no longer ships 8.0/8.1-era proxy instructions);
the same-day `budi stats` audit closed two more (`-p week` / `-p month`
now resolve to rolling 7 / 30 days on every weekday to match the
README; breakdown views now reconcile to the cent via a shared
grand-total envelope). No proxy reintroduction, no new shell-profile
/ Cursor-settings / Codex-config mutation path, and no new runtime
network call except the ADR-0091 pricing refresh fetch from
`raw.githubusercontent.com/BerriAI/litellm/...`, gated by
`BUDI_PRICING_REFRESH` (default on).

### Added

- **Manifest-driven model pricing** (#376, [ADR-0091](docs/adr/0091-model-pricing-manifest-source-of-truth.md)) — the headline of 8.3.0. `pricing::lookup(model_id, provider)` is now the only path that produces a `ModelPricing` anywhere in the workspace. Resolution is three-layer: on-disk cache at `~/.local/share/budi/pricing.json` → embedded LiteLLM baseline built into the binary → hard-fail to `unknown` (`cost_cents = 0`, `pricing_source = 'unknown'`, warn icon, no silent default). A daemon-side refresh worker (`workers::pricing_refresh`) fetches `https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json` on startup if the cache is >24 h stale and every 24 h thereafter, validates the payload (JSON shape, ≥95% known-model retention floor, $1,000/M sanity ceiling, 10 MB body cap), writes atomically, and hot-swaps state under an `RwLock`. Fetch failures log `warn` and never block ingestion; `BUDI_PRICING_REFRESH=0` disables the worker entirely. Every `messages` row now carries a `pricing_source` tag (`manifest:vNNN` / `backfilled:vNNN` / `embedded:vBUILD` / `legacy:pre-manifest` / `unknown` / `upstream:api`) so every cent is auditable. History immutability is a first-class invariant: `unknown → backfilled:vNNN` is the only legal automatic rewrite; `manifest:vNNN` with a new upstream price is never recomputed; `legacy:pre-manifest` rows — including the buggy pre-pivot `claude-opus-4-7-*` rows priced off the substring-fallthrough Sonnet rate — are never auto-touched. There is **no** `budi pricing recompute` command and the ADR explicitly rejects filing one. Operator surface: `budi pricing status [--json] [--refresh]` prints the source, manifest version, fetched-at, next refresh, known-model count, and unknown-models-seen list; `GET /pricing/status` / `POST /pricing/refresh` are the loopback-only daemon endpoints. New schema column (`ALTER TABLE messages ADD COLUMN pricing_source TEXT NOT NULL DEFAULT 'legacy:pre-manifest'`) is idempotent. One net new permitted outbound destination, documented as a §Neutral amendment to [ADR-0083](docs/adr/0083-cloud-ingest-identity-and-privacy-contract.md) (public HTTPS GET of a public JSON file, no user data, no headers beyond `User-Agent`/`Accept`). Nine promotion-criteria test gates are green: (1) `manifest:vNNN` never recomputed across refresh, (2) `legacy:pre-manifest` never recomputed across refresh, (3) `unknown → backfilled:vNNN` rewrites do happen on refresh, (4) UTF-8 boundary safety on multi-byte model ids, (5) <95%-retention guard rejects a wiped payload, (6) $1,000/M sanity ceiling rejects a mispriced payload, (7) `BUDI_PRICING_REFRESH=0` suppresses network calls, (8) schema migration is idempotent (`pricing_manifests` + column), (9) `budi pricing status --json` golden-key shape is stable.
- **`budi cloud init` generates a commented `~/.config/budi/cloud.toml` template** (#446) — fresh-user cloud sync used to be a four-step adventure: `budi cloud status` pointed at `~/.config/budi/cloud.toml`, the file didn't exist, the only schema reference was `docs/adr/0083-cloud-ingest-identity-and-privacy-contract.md`, and the user had to `mkdir -p ~/.config/budi/`, hand-write TOML, guess the field names, and restart the daemon. The new `budi cloud init` subcommand writes a heavily commented starter config (every field documented inline, `enabled = false`, `api_key = "PASTE_YOUR_KEY_HERE"`) so the next steps are just "paste key, flip `enabled = true`, `budi init`." `budi cloud init --api-key <K>` is the one-shot variant that writes the key and `enabled = true` in a single command for users who already have their key on the clipboard. `--force` overwrites an existing config; a real (non-stub) api_key in the existing file triggers an interactive confirmation unless `--yes` is also passed, and non-TTY stdin defaults the confirmation to "no" so an accidental scripted re-init never silently clobbers a working config. `budi cloud status` now differentiates three not-ready shapes — "disabled (no config)" (→ run `budi cloud init`), "disabled (stub key)" (→ paste your key), and the pre-existing "disabled" / "enabled but missing api_key" / "enabled but not fully configured" — by reading two new fields on `GET /cloud/status` (`config_exists`, `api_key_stub`) instead of re-poking the filesystem on every render. Older CLIs talking to a newer daemon keep working because the new fields default to the pre-#446 branch (`config_exists = true`, `api_key_stub = false`). No new dependencies, no new shell-profile / Cursor-settings / Codex-config mutation paths, no new egress: the template is written purely to `~/.config/budi/` through `std::fs`.

### Fixed

- **`budi doctor` no longer false-FAILs on live Cursor tailer drift** (#438) — on a clean `v8.2.1` install with a Cursor agent actively writing its JSONL transcript, the 2026-04-20 fresh-user smoke pass caught `budi doctor` returning a red FAIL + non-zero exit within seconds of install, blaming a ~2.5 KB `file_len - tail_offset` gap that the tailer was going to close on the next tick. Worse, the fix-message told users to `restart with budi init` — which doesn't help (the daemon is fine) and resets more state than necessary. The visibility check now tiers the gap: **PASS** when gap ≤ 1 MB and the tailer read this specific file within the last 60 s (ordinary live-write drift, closes on next tick); **FAIL** when gap > 10 MB or the tailer has been silent for > 5 min while the file was modified within the last 60 s (genuine wedge — actively-written file that the tailer isn't reading); **WARN** for everything in between (possible wedge, not catastrophic; exit code stays 0 under the existing "0 failing check(s) and N warning(s)" contract). Fix-message copy now points at `~/.local/share/budi/logs/daemon.log` instead of `budi init`; a regression-guard test (`visibility_does_not_suggest_restart_with_budi_init`) asserts no tier's fix copy contains `budi init`. Per-file `latest_file_tail_seen` is stored as a dedicated `ProviderDoctorData` field (previously the value was lost to the `.or()` fallback that kept the provider-wide `MAX` from `tailer health`). Release-blocker for `v8.3.0`; one of the two Round F release-blockers from the fresh-user smoke pass.
- **`budi vitals` no longer paints a trust-killer GREEN over all-N/A sessions** (#441) — the session-health aggregator defaulted to `health_state="green"` + `tip="Session healthy"` whenever nothing tripped a yellow / red threshold, so a fresh session where every one of the four vitals came back `N/A` still rendered `🟢 Session Health: GREEN / Session healthy` above four `⚪ ... : N/A` rows. A new `insufficient_data` overall state is returned when at least 3 of 4 vitals are N/A (or no vitals scored at all), surfacing as `⚪ Session Health: INSUFFICIENT DATA / Not enough session data yet to assess` with dim styling. Exactly 2 of 4 N/A renders plain `green` but the tip now calls out the partial coverage (`Session healthy (partial — 2 metrics need more session data)`) so the verdict is honest about what was actually checked; 0-1 N/A still reads plain "Session healthy" so common steady-state sessions (where `thrashing` is absent post-v22 hook_events drop) aren't labelled partial on every invocation. Any scored yellow / red vital still dominates — one red vital with three N/A still renders red, because INSUFFICIENT DATA would hide an actual issue. `session_health_batch` (powering the `budi sessions` list view) inherits the same rule, so empty / fresh sessions fall through to a neutral open circle (`{dim}○`) instead of a green dot. `budi sessions <id>` detail view renders the same icon + label; the statusline and `/analytics/statusline` contract pass the new state through unchanged (the statusline CLI already gates on presence only, not green/yellow/red, and the Cursor extension doesn't yet consume `health_state`, so both surfaces inherit the neutral rendering when the state is `insufficient_data`). Unit coverage pins all four transition cases (all-N/A → insufficient_data, 3-N/A-plus-one-green → insufficient_data, 2-N/A → green with partial tip, 1-red-plus-3-N/A → red).
- **`budi uninstall` removes installer residue it previously left behind** (#439) — the 2026-04-20 smoke pass caught `budi uninstall --yes` claiming `✓ removed` while silently leaving `~/Library/Logs/budi-daemon.log` (launchd writes it through the plist's `StandardOutPath` / `StandardErrorPath`, outside the data dir the uninstaller walked), the `# Added by budi installer` block that `scripts/install-standalone.sh` / `scripts/install.sh` appended to `~/.zshrc` (or bash/fish equivalents) — permanently polluting `$PATH` after the user believed they had uninstalled — and any `+N other entries` in the data dir with no enumeration so users couldn't spot leftovers. The fix narrows all three without adding any new install surface. A new `budi_core::installer_residue` module scans `~/.zshrc` / `~/.bashrc` / `~/.bash_profile` / `~/.profile` / `~/.config/fish/config.fish` for the `# Added by budi installer` marker and its immediately-following PATH line (`export PATH=...`, `PATH=...`, `fish_add_path ...`), eating one preceding blank line so the installer's `printf '\n# Added...'` round-trips byte-identical; orphan markers without a matching PATH line on the next line are left alone (consent-first — never eat a stray comment). `budi_core::autostart::service_log_path()` exposes the macOS launchd log path (`None` on Linux/Windows where the daemon goes to journald / Task Scheduler and there's no standalone file); `cmd_uninstall` deletes this file after `uninstall_service()`. The data-dir walk now enumerates what was removed in the ADR-0083 / ADR-0086 contract shape (`analytics.db` + sidecars, `repos/ (N repo[s])`, `cursor-sessions.json`, `pricing.json`, `upgrade-flags/`), surfacing anything unknown as `+N other entries`. Shell-profile cleanup is consent-first per ADR-0081: interactive runs show the exact lines we plan to remove and prompt Y/n per file; `--yes` skips the prompt; non-interactive runs without `--yes` skip the removal with a reminder. Regression coverage pins both the scanner (`scan_finds_and_apply_removes_installer_block`) and the byte-identical round-trip invariant (`apply_cleanup_roundtrip_preserves_bytes_when_block_absent`). Folded into the Round D hygiene sweep per #436.
- **`budi stats -p week` and `-p month` now roll 7 / 30 days on every weekday** (#447) — `budi stats -p week` was silently returning the same data as `-p today` on Mondays, and `-p month` was silently returning the same data as `-p today` on the 1st of every month. The README has always contracted both as "the last 7 / 30 calendar days including today" (rolling), but `period_date_range` was resolving `Week` to calendar-week-starting-Monday and `Month` to first-of-this-month — so on those days the "week" / "month" window collapsed to a single calendar day of data. `StatsPeriod::Week` now resolves to `today − 7 days` and `StatsPeriod::Month` to `today − 30 days`, byte-identical to `-p 7d` / `-p 30d` on every weekday and every day of every month. `period_label` renders `Last 7 days` / `Last 30 days` so the header can't drift from the total again. Summary JSON output exposes `window_start` / `window_end` so scripts can verify which window `--period` mapped to. Weekday-parameterized regression tests cover Mon..Sun and the full 31-day month (`week_resolves_to_rolling_seven_days_on_every_weekday`, `month_resolves_to_rolling_thirty_days_on_every_day_of_month`), asserting `Week == Days(7) != Today` and `Month == Days(30) != Today` on every case and pinning `today − Week == 7 days` / `today − Month == 30 days`. Release-blocker for `v8.3.0`; one of the two Round S release-blockers from the `budi stats` audit.
- **`budi init` wires the Claude Code statusline by default** (#454) — fresh `brew install siropkin/budi/budi && budi init` used to leave Claude Code with no Budi statusline because `cmd_init` never called the integrations installer; the user had to discover `budi integrations install` on their own, and README / getbudi.dev both implied the statusline was automatic. `budi init` now calls `install_selected(&cfg, &default_recommended_components(), None)` after the daemon + autostart setup, installing the Claude Code statusline into `~/.claude/settings.json` (merging with an existing `statusLine.command` rather than clobbering it) and the Cursor extension when the Cursor CLI is on PATH. Opt-out is `budi init --no-integrations` for CI, containers, and users who manage Claude / Cursor settings by hand. The installer is idempotent — a second `budi init` neither duplicates the budi statusline suffix nor rewrites unchanged bytes — and skips the Claude step entirely when `~/.claude` does not exist so Budi never silently materializes Claude Code's directory on a non-Claude machine. `budi doctor` gains a `Claude statusline` check that passes when the statusline is wired (or when `~/.claude` is absent), and warns with the exact `budi integrations install --with claude-code-statusline` repair command when Claude Code is installed but the wiring is missing. README's "Status line" section, "Integrations" section, and the "Status line not showing" troubleshooting entry all rewrite to match; SOUL.md §Statusline contract documents the opt-out flag and the doctor nudge alongside the existing "default install path is quiet" rule. A new `scripts/e2e/test_454_init_installs_statusline.sh` regression guard pins all four scenarios (absent `~/.claude`, fresh install, idempotent re-run, `--no-integrations` opt-out, plus the doctor warn + repair-command surface).
- **`budi stats` breakdown views reconcile to the cent** (#448) — `budi stats --files 30d` on the maintainer's machine was underreporting total cost by ~9% because every breakdown view silently truncated at 30 rows with no grand total and no `(other)` bucket. All seven breakdown views (`--projects / --branches / --tickets / --activities / --files / --models / --tag`) now ship through a shared `BreakdownPage<T>` envelope carrying `rows`, an optional `other` truncation-tail aggregate, `total_cost_cents`, `total_rows`, `shown_rows`, and the effective `limit`. Contract: `sum(rows.cost_cents) + other.cost_cents == total_cost_cents` to the cent for every period and breakdown. Text output gains a `Total  $X  (M of N rows shown — pass --limit 0 for all)` footer plus an `(other — N more rows)` line when truncation occurs. New `--limit N` flag (default 30, `0` = unlimited) on every breakdown view; the daemon fetches the full ranked set (`BREAKDOWN_FETCH_ALL_LIMIT = 1_000_000` rows) and `paginate_breakdown` slices in Rust, so no SQL query rewrite was needed and `(untagged)` buckets remain part of the ranking. **Wire-format change (deliberate):** `GET /analytics/{projects,branches,tickets,activities,files,models,tags}` used to return a bare JSON array and now returns the envelope shape; scripts that parsed the top-level array need to read `.rows` (or `.rows + .other` for reconciliation). The CLI and the cloud dashboard are updated in the same PR. Ten new reconciliation tests in `analytics/tests.rs` seed ≥ 33 distinct dimensions per view, cap at 30, and assert `sum(rows) + other == total_cost_cents` to the cent (`breakdown_{tickets,files,activities,projects,branches,models,tags}_reconcile_with_other_row_when_truncated`); a cross-window test plants cohorts at `now - 1h` / `- 3d` / `- 20d` and reconciles across `today` / `7d` / `30d` separately. Release-blocker for `v8.3.0`; the second of the two Round S release-blockers.

### Changed

- **Cursor marketplace extension republished at v1.3.0 for the 8.2 tailer flow** (#437, cross-repo [siropkin/budi-cursor](https://github.com/siropkin/budi-cursor)) — the VS Code / Open VSX `siropkin.budi-cursor` extension at v1.0.1 was still shipping 8.0 / 8.1-era proxy instructions: `README.md`, the bundled `readme.md`, `src/welcomeView.ts`, and `src/sessionStore.ts` all told the user to "Override OpenAI Base URL to `http://localhost:9878`". Every fresh user who reached Budi through the Cursor marketplace path the homepage recommends was misdirected. The 1.3.0 republish rewrites the docs for the [ADR-0089](docs/adr/0089-reverse-proxy-first-jsonl-tailing-as-sole-live-path.md) tailer flow, rewords the welcome-view init hand-off (`budi init` starts the daemon + transcript tailer, not "the proxy"), adds an explicit "Cursor Usage API can lag ~10 min" caveat, and lands a CI grep guard in both `ci.yml` and `release.yml` that fails the build if `9878` or `Override OpenAI Base URL` ever reappears in `out/` or `README.md`. Marketplace and Open VSX both accepted v1.3.0 ahead of the `v8.3.0` tag — the homepage install path no longer lies. Release-blocker for `v8.3.0`; one of the two Round F release-blockers. The `siropkin/budi-cursor` PR is [`#9`](https://github.com/siropkin/budi-cursor/pull/9); the GitHub release is [v1.3.0](https://github.com/siropkin/budi-cursor/releases/tag/v1.3.0).
- **`budi db import` now renders per-agent progress and a reconciled final summary** (#440) — the 8.2 import flow was a silent 4–5 minute wait with a single `15s... 30s...` heartbeat and a collapsed one-line result. The CLI now polls the daemon's live sync state every 2 seconds while the sync runs and repaints a per-agent progress line (`[Claude Code] 1247 / 2035 files (61%), 72,421 messages`) so the user sees throughput instead of a stopwatch. On completion it prints a reconciled per-agent table (`Claude Code`, `Codex`, `Copilot CLI`, `Cursor`) keyed on totals returned by the daemon, plus a one-line 30-day cost preview pointing at `budi stats -p 30d`. TTY detection gates the ANSI cursor-clear so piped / CI output stays plain (one line per provider transition). New `--format json` flag emits a structured per-agent summary (`per_provider[]` with `files_total` / `files_synced` / `messages`) and suppresses progress chatter so the stream is scriptable. Wire-format additions: `SyncResponse.per_provider` on `POST /sync/all` and `POST /sync/reset`, `SyncStatusResponse.progress` on `GET /sync/status` (both `#[serde(skip_serializing_if = ...)]` so older consumers that don't know about the fields keep parsing the envelope). No new dependencies; the progress callback is a `Fn(&SyncProgress)` the daemon publishes through a `Mutex<Option<SyncProgress>>` slot on `AppState` and clears on RAII drop.
- **`budi stats --projects` stops mixing real repos with random non-repo dirs** (#442) — the Repositories table used to surface ad-hoc working directories (`Desktop`, `ivan.seredkin`, `.cursor`, `homebrew-budi`, `awesome-vibe-coding-1`) as if they were first-class rows alongside `github.com/siropkin/budi` and `github.com/verkada/Verkada-Web`. `resolve_repo_id` now returns `None` for any cwd that isn't inside a git repo with a remote origin (previously it fell back to the git-root folder name or the cwd folder name), so new ingests persist `repo_id = NULL` for non-repo work and roll up into the single `(no repository)` bucket the Projects view already rendered for NULL rows. An idempotent one-shot backfill on daemon startup rewrites pre-8.3 bare-folder-name values to NULL across `messages.repo_id` and `sessions.repo_id` (host must contain a `.` and the value must have at least two `/` separators — i.e. `host/owner/repo` — to be preserved), and rebuilds `message_rollups_hourly` / `message_rollups_daily` from the corrected `messages` rows so `budi stats` reconciles to the cent immediately after upgrade without requiring `budi db import --force`. New `--include-non-repo` flag on `budi stats --projects` reveals the per-folder-basename breakdown underneath the main table for operators who want the pre-8.3 detail view; JSON output in that mode splits into `{repositories: …, non_repo: […] }` so scripting consumers get a stable shape. Non-repo dirs with the same basename (`~/Desktop` and `/tmp/Desktop`) aggregate into one row, matching the label users already recognize from their history.
- **Model display names normalized across providers** (#443) — Claude Opus 4.7 used to show up under three different labels on the same machine: `claude-opus-4-7` (Claude Code), `claude-opus-4-7-thinking-high` (newer Cursor), `claude-4.7-opus-high-thinking` (older Cursor). A new Budi-owned display overlay (`budi_core::pricing::display`) resolves raw provider model ids to a canonical `(display_name, effort_modifier)` shape — every one of those three raw ids now renders as `Claude Opus 4.7` in the `display_name` column, with `effort_modifier = "thinking-high"` as a separate dimension (middot-joined in text as `Claude Opus 4.7 · thinking-high`, distinct `effort_modifier` key in JSON — never concatenated into `display_name`). Pair with [ADR-0091](docs/adr/0091-model-pricing-manifest-source-of-truth.md) per the ticket: the LiteLLM manifest stays the source of truth for **cost**, the display overlay is a separate Budi-owned layer that never mutates pricing. `default` / `(untagged)` / `(model pending)` rows collapse into a single per-provider `(model not yet attributed)` bucket; zero-cost placeholder rows stay suppressed unless `--include-pending` is passed; non-zero placeholders (Cursor Auto with real spend) always render so the user doesn't lose sight of real cost. `budi pricing status` surfaces the alias mapping (compact "Display-name aliases" section in text; `aliases` array in JSON). `--format json` grows `display_name`, `effort_modifier` (skip_if_none), `provider_model_id` (alias for `model`), and a `placeholder` tag (`none` / `cursor_auto` / `not_attributed`); existing `model` / `provider` / count fields are unchanged so scripts keep working. Unknown families fall through — `display_name == raw`, no silent relabel.
- **`budi stats` / `budi sessions` CLI output polish** (#445) — the six polish items from the 2026-04-20 fresh-user smoke pass, each one a small change to one render / serialize site. (1) `budi sessions` renders an 8-character UUID prefix by default with `--full-uuid` for scripting / lookup and drops the old byte-slice `shorten_model` truncation in favour of the canonical `pricing::display::combined_label()` from the #443 overlay, so `claude-opus-4-7-thinking-high` now renders as `Claude Opus 4.7 · thinking-high` with no mid-word cut. (2) A `+N = N additional model(s) used in this session (pass --full-uuid and budi sessions <id> to see the full list)` legend prints once at the bottom of the list whenever any row uses the `+N` compactor. (3) `work_outcome::derive_work_outcome` drops the jargon rationale `"no non-integration branch on session — nothing to correlate"` in favour of the plain-language replacement `"session wasn't tied to a feature branch, so no merge outcome can be inferred"` — names the reason without implying the user can change it. (4) Every `*_cents` field in every JSON surface (`budi stats` summary + every breakdown + every detail; `budi sessions` list + detail; `budi pricing status`; `budi cloud status`/`sync`; `budi db import --format json`; `budi statusline --format json`) is rounded to an integer via a shared `print_json` helper, so cents-by-definition no longer render as `151767.0` with a spurious fractional tail. (5) The `budi stats` summary `Tokens` line now lists all four components (`{input} input · {output} output · {cache-write} cache-write · {cache-read} cache-read`) so it reconciles against the per-agent rows — the pre-#445 `{input} in, {output} out` shape is gone and a snapshot assertion guards against its reintroduction. (6) Zero-token / zero-cost placeholder-row suppression is already end-to-end covered by the #443 merge + #450 pending logic; the behaviour is intentionally locked in. Column overflow on short-UUID / model-name rendering uses a char-boundary ellipsis (`truncate_on_char_boundary`) — the #389 / #383 / #404 UTF-8 bug class is guarded by a dedicated test on `café-1234-5678-abcd` and `αβγδe`. The `budi sessions` text-view column widths shifted (session id 8 chars instead of 36, model 28 with ellipsis-aware truncation); `--full-uuid` restores the old width for scripts, and the JSON output (`SessionListEntry.id`) still carries full ids unchanged.
- **`budi stats` breakdown bar charts standardized on the `--tag` layout** (#449) — every `budi stats` breakdown view (`--projects / --branches / --tickets / --activities / --files / --models / --tag`) now ships through a single shared bar renderer and header so the visual signal cannot drift again. Bars scale by `cost_cents` normalized against the max visible row (not by message count — a `$66 / 5814-msg` row no longer out-draws a `$548 / 381-msg` one); `$0` rows produce a blank bar cell of the same width so columns stay aligned (the `(untagged) … $0.00 ████████████████` regression is gone); every view carries a header row (`LABEL … COST EXTRAS` with `COST` right-aligned to the cost column and `SOURCE` / `CONFIDENCE` / `TOP_BRANCH` / `TOP_TICKET` / `MSGS` / `TOKENS` titles) so new users don't guess what `src=branch` or `conf=medium` mean; the bar sits immediately before the cost column so the eye scans `LABEL → BAR → COST` without crossing anything else. Layout primitives (`BREAKDOWN_BAR_WIDTH`, `BREAKDOWN_COST_WIDTH`, `render_bar`, `format_breakdown_header_text`, `format_breakdown_row_text`, `truncate_label`, `max_cost_for_rows`) are a single source of truth across the seven views. `truncate_label` is char-based, guarding the #389 / #383 / #404 / #445 UTF-8 boundary class on user-supplied labels (file paths, ticket IDs, tag values, model ids). Wire format is unchanged; this is text-render-only. Snapshot tests against approved baselines pin the layout for every view (`breakdown_row_snapshot_models_view`, `_tickets_view`, `_activities_view`, `_tag_view`) plus the proportional-bar (`render_bar_is_proportional_to_cost_not_message_count`) and all-$0 (`render_bar_is_blank_when_every_row_is_zero`) invariants.
- **`budi stats` breakdowns now render cleanly on a fresh install** (#450) — the external CLI audit that produced `#447` / `#448` also flagged five smaller UX issues on `budi stats --tickets / --branches / --activities / --files / --models`: no column headers surfaced the `src=…` / `conf=…` / dominant-branch / source-type legends; long branch names and file paths blew up row alignment; the cost column mixed humanized `$1.5K` and fixed `$90.39` in the same view; a single `(untagged)` row on an otherwise-empty `--files today` looked like a filesystem fault; and the same `(untagged)` sentinel meant five different things across views (no branch, no ticket, unclassified, no file tag, Cursor model lag). 8.3.0 applies the Round S polish pass: every breakdown now renders per-view labels (`(no branch)` / `(no ticket)` / `(unclassified)` / `(no file tag)` / `(no repository)`; `--models` hides the `(model pending)` Cursor-lag transient by default with a `* N model row(s) pending — Cursor lag (pass --include-pending to see)` footnote and a new `--include-pending` flag to restore them); labels and label-like columns (branch / path / ticket id) truncate with a middle ellipsis at a configurable `--label-width N` (default 40), so long values like `04-20-pava-1669_adds_an_optional_inputmode_prop_to_chararrayinput` render as `04-20-pava-1669…hararrayinput` and never bleed into the next column; breakdown tables use one fixed `$X,XXX.XX` currency shape everywhere (the humanized `$1.2K` form stays in the headline summary only); when the only row on a breakdown is the `(untagged)` bucket, the CLI prints a one-line empty-state (`No X attribution emitted in this window.` with a `Try --period 7d.` nudge on `today` / `1d`) instead of a misleading one-row table. The DB sentinel (`(untagged)`) is unchanged, so JSON output and existing queries keep reconciling to the cent — the translation happens purely at render time. Snapshot tests pin the new shape for every view (`breakdown_view_untagged_label_is_view_specific`, `snapshot_tickets_today_and_30d_layout_is_stable`, `snapshot_files_today_and_30d_layout_is_stable`, `snapshot_branches_today_and_30d_layout_is_stable`, `snapshot_activities_today_and_30d_layout_is_stable`, `snapshot_models_today_and_30d_layout_is_stable`) alongside the `format_cost_cents_fixed` thousands-separator rounding-carry guard and the `partition_pending_model_rows` include-pending behaviour.
- **`budi stats` summary shape unified across all periods** (#451) — pre-8.3 the summary changed shape based on period and provider count. The dispatcher routed `today` and `--provider P` invocations through `cmd_stats_summary_filtered` (no Agents block) and only used the multi-agent renderer when `providers.len() > 1`, so the most-used view (`today`) silently looked thinner than `1d` / `7d` / `month` on the same machine whenever the window happened to surface a single provider. Both renderers collapse into one `format_summary` that always emits the same blocks, regardless of period or provider count: header (title + optional `(provider)` decoration), Agents block (even with a single provider in the window), Total line with user / assistant counts, Tokens line, Est. cost line, cost-component sub-line (input / output / cache write / cache read — unconditional, even on all-$0 windows), cache savings line (unconditional, `$0.00` when no cache hits), and a Cursor-lag footnote whenever Cursor is displayed (not only on explicit `--provider cursor`). Pure `format_summary` is testable via a `SummaryPalette` (ANSI in production, empty strings in tests). Six new tests pin the contract (`summary_shape_is_identical_across_all_six_periods`, `summary_keeps_agents_block_with_one_provider`, `summary_cache_savings_line_is_unconditional`, `summary_provider_filter_renders_one_row_in_agents_block`, `summary_empty_window_renders_no_data_message_and_skips_blocks`, `summary_cursor_lag_footnote_rides_on_displayed_providers`).

### Removed

- **The four hardcoded `*_pricing_for_model()` functions** (#377) — Round P cleanup for the [ADR-0091](docs/adr/0091-model-pricing-manifest-source-of-truth.md) pricing pivot. Deletes `providers::claude_code::claude_pricing_for_model`, `providers::codex::codex_pricing_for_model`, `providers::cursor::cursor_pricing_for_model`, `providers::copilot::copilot_pricing_for_model`, and the `provider::pricing_for_model` dispatcher that fanned out to them — all four substring-dispatch tables and their test suites are gone. `pricing::lookup` is now the only code path that produces a `ModelPricing` anywhere in the workspace. The two remaining callers (`cost::estimate_cost_with_filters` per-model loop and `analytics::queries::cache_efficiency_with_filters` cache-savings loop) migrated to `pricing::lookup` with skip-on-`Unknown` semantics that match the `$0`-on-unknown contract enforced at ingest by `CostEnricher` (ADR-0091 §2). Test fixtures that used `claude-sonnet-4-6-20260321` normalized to `claude-sonnet-4-6` (the canonical id in the embedded manifest); the old fixtures only worked because the legacy `m.contains("sonnet")` substring branch silently accepted invented date-suffixed ids — exactly the class of bug ADR-0091 was designed to eliminate. Net change: **-750 LOC** (789 deletions, 39 insertions across 9 files). **Behavioural change for unknown models:** under the old code path the four dispatchers silently returned a default rate (Sonnet for Claude; GPT-5.2/5.3 for Codex; Composer 2 for Cursor). Under the new path any call site that previously resolved to those defaults now returns `Unknown` and stays out of the sum (contract: unknown models have cost `0`, not a guessed default rate).
- **Bare-verb DB admin aliases `budi migrate` / `budi repair` / `budi import`** (#428) — 8.2.1 (#368) grouped the three DB admin verbs under a single `budi db` namespace (`budi db migrate`, `budi db repair`, `budi db import`) and kept the pre-namespace bare verbs as hidden backward-compatibility aliases that printed a one-per-day stderr deprecation nudge. 8.3.0 removes the aliases, the nudge, and the `$BUDI_HOME/db-alias-nudge` rate-limit marker; `budi db <verb>` is now the only surface for the DB admin commands and `budi migrate` / `budi repair` / `budi import` return a clap "unknown subcommand" error. `budi update` best-effort deletes the stale marker file from 8.2.x installs on upgrade (silent on failure, no user prompt). `README.md` / `SOUL.md` / top-level help / the `budi db` `after_help` examples no longer mention the bare verbs.

### Process

- **ADR-0091 adopted — model pricing via embedded baseline + LiteLLM runtime refresh** (#375, [ADR-0091](docs/adr/0091-model-pricing-manifest-source-of-truth.md)) — the decision record that Round P implemented. Replaces the four hand-maintained `*_pricing_for_model()` functions with a single manifest-backed `pricing::lookup(model_id, provider) -> PricingOutcome`. Three-layer resolution (on-disk cache → embedded baseline → hard-fail to `unknown` — no silent default). Daemon-side 24 h refresh worker fetches `https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json` with validation (JSON shape, ≥95% retention floor, $1,000/M sanity ceiling, 10 MB body cap), atomic write, `RwLock` hot-swap. `BUDI_PRICING_REFRESH=0` operator opt-out. New `pricing_source` column (`manifest:vNNN` / `backfilled:vNNN` / `embedded:vBUILD` / `legacy:pre-manifest` / `unknown` / `upstream:api`) tags every row so cost history is auditable. History immutability is a first-class invariant: `unknown → backfilled:vNNN` is the only legal automatic rewrite; `manifest:vNNN` and `legacy:pre-manifest` rows are never auto-recomputed; there is **no** `budi pricing recompute` command and the ADR explicitly rejects filing one. Operator surface: `budi pricing status [--json] [--refresh]`, `GET /pricing/status`, `POST /pricing/refresh` (loopback-only). Amends [ADR-0083](docs/adr/0083-cloud-ingest-identity-and-privacy-contract.md) §Neutral to document the one new outbound destination (public HTTPS GET of a public JSON file, no user data, no headers beyond `User-Agent`/`Accept`). Propagates to `SOUL.md` / `README.md` / `docs/adr/0083-...md` in the same PR per the 8.2.1 lesson "ADR / policy / docs propagation rides in the same PR."
- **`docs/` + `scripts/` audit and hygiene sweep** (#365) — closes the deferred 8.2 R3.0 audit. Every top-level doc / script now has an explicit disposition, verified with `rg` against the rest of the repo. **Promoted to ADR:** `docs/research/cursor-usage-api.md` → `docs/adr/0090-cursor-usage-api-contract.md`; `providers/cursor.rs` module rustdoc links to the ADR so any upstream change lands as a paired edit. **Inlined as rustdoc:** `docs/research/ideas-repo-identity.md` → "Design history" section in `repo_id.rs` (the current rustdoc was already more accurate post-#442 than the 2026-03-22 research note). **Archived to #365 comments + deleted:** `docs/research/{agent-local-files,competitive-landscape,yc-landscape,ideas-alerts,ideas-multi-machine-sync}.md` and `docs/releases/{8.1.0,8.1.0-smoke-tests}.md` (wiki not yet initialized → durable-issue-comment fallback per the global rule; each archive comment carries an origin-commit footer pointing at `0d59f7c`). **One-shot script archived + deleted:** `homebrew/setup-tap.sh` (tap is long bootstrapped; `release.yml`'s `update-homebrew` job inlines every subsequent formula update; recovery runbook archived as a comment). **Empty directories removed:** `docs/research/`, `docs/releases/`. `scripts/research/` stays for the #407 `cursor_usage_api_lag.sh` carve-out. Link audit fixed two broken CHANGELOG references (lines 120 / 121) and the `CONTRIBUTING.md` Homebrew section. CI shellcheck loop now covers `scripts/{*.sh, pricing/*.sh, research/*.sh}` with skip-if-missing. Follow-up [#472](https://github.com/siropkin/budi/issues/472) filed against `8.4.0` to codify the permanent rule-12 ("no new `docs/research/` / `docs/releases/` / `scripts/research/`; operator-only measurement-script carve-out restated") as an ADR-0081 amendment.
- **`--provider` partition properties pinned against the 8.2.1 audit phantom** (#452) — investigation-only ticket closed on evidence per the #436 audit-discipline rule. The 8.2.1 external CLI audit reported a 74-message gap between `budi stats` text output and `--format json` for the same query, plus a `--provider cursor` count higher than the unfiltered count; neither reproduced on a second machine. Code-read findings: within one daemon invocation, text and JSON both call `GET /analytics/summary` backed by `usage_summary_with_filters`, so they cannot disagree by construction (the gap most likely came from two separate CLI invocations taking separate snapshots while a live tailer ingested in between); the rollup path `usage_summary_from_rollups` uses `provider = ?` without the COALESCE wrapper but that can only make `filtered <= unfiltered`, never the inverse. Four property tests in `analytics/tests.rs` pin the contract at the SQL layer: `provider_filtered_summary_count_is_at_most_unfiltered_count` (direct refutation of the audit's hypothesis B); `provider_filtered_summary_partitions_by_provider_to_the_message` (per-provider counts sum to unfiltered); `provider_filtered_cost_partitions_by_provider_to_the_cent` (same partitioning property on cost, with one-cent floating-point tolerance); `unknown_provider_filter_yields_zero_messages_and_zero_cost` (defensive degrade if a stale alias bypasses `normalize_provider`). If the anomaly ever reproduces under instrumented capture, a follow-up bug gets filed rather than chasing the 8.2.1 phantom.

## 8.2.1 — 2026-04-20

8.2.1 is the post-`v8.2.0` hardening train: it tightens tailer runtime
correctness (crash-consistent offset persistence, graceful shutdown,
UTF-8 boundary handling, post-boot watch-root materialization),
brings the analytics / cloud / CLI backlog deferred out of the
`8.2.0` shortest-path tag up to parity with the canonical pipeline,
and closes the four `v8.2.0` R3.5 release-audit follow-ups. No new
scope, no proxy / wrapper reintroduction, no live-path behaviour
change beyond hardening.

### Fixed

- **`tail_offset` is now persisted atomically with ingest** (#382) — the JSONL tailer used to commit `ingest_messages_with_sync` and `set_tail_offset` as two separate SQLite writes. A daemon crash (or power loss) between the two left `messages` rows correct (uuid dedup keeps them safe) but pinned `tail_offsets` at the pre-batch byte position, so the next `notify` event re-read the same byte range, did the uuid-dedup dance again, and overstated ingest volume in the structured log. `ingest_messages_with_sync` now accepts an optional `tail_file: (provider, path, new_offset)` tuple and upserts `tail_offsets` inside the same transaction as the message inserts. The tailer passes `Some(...)` on real batches; `budi import`'s `analytics::sync` path still passes `None` so the two ingestion paths stay cleanly separated. The empty-batch fast path keeps its standalone upsert — with no ingest tx to atomize against, re-scanning an empty tail on crash recovery is the simplest correct behaviour. Regression tests in `crates/budi-core/src/analytics/tests.rs` pin both the single-transaction invariant and the "no upsert for sync.rs callers" regression guard.
- **Tailer tolerates partial UTF-8 at the `read_tail` boundary** (#383) — `workers::tailer::read_tail` called `std::io::Read::read_to_string`, which validates the entire byte slice as UTF-8. When an agent was mid-write of a multi-byte character at the moment the `notify` event fired, the read returned `InvalidData`, `process_path` logged `read_tail failed`, and the offset stayed pinned until the next event or the 5 s backstop tick. Recovery worked but the warning rate on non-ASCII transcripts (emoji, CJK, escaped Unicode) was non-zero and looked like a real failure. `read_tail` now uses `read_to_end`, truncates to the longest valid-UTF-8 prefix via `std::str::from_utf8(..).err().map(|e| e.valid_up_to())`, then further truncates to the last `\n` so the returned content is always line-aligned. Partial characters and unterminated trailing lines are left on disk for the next tick, matching the incomplete-final-line contract already enforced by `jsonl::parse_transcript`. The `new_offset = effective_offset + content.len()` math in `process_path` is unchanged because `content` is now exactly the consumed, line-aligned byte slice.
- **Tailer drains on SIGINT / SIGTERM before the process exits** (#384) — the `shutdown: Arc<AtomicBool>` parameter on `workers::tailer::run` has always advertised a graceful-stop API, but nothing in `main.rs` ever flipped the flag in production; the tailer only stopped when tokio aborted the blocking pool at process teardown. The daemon now installs a tokio-side shutdown listener that awaits `ctrl_c` (all platforms) and `SIGTERM` (unix), flips the tailer shutdown flag, waits up to one backstop interval + 1 s for the blocking loop to return, and then calls `std::process::exit(0)` so axum's HTTP serve loop tears down with the runtime. `kill_existing_daemon` no longer sleeps a fixed 300 ms after sending SIGTERM — it polls `kill -0` for the same grace window and escalates to SIGKILL if the old daemon is still alive, so a new binary can still take over even when the old one now has a real drain phase. External scripts that sent SIGTERM and immediately assumed the process was gone need to allow the ~5 s grace window; SIGKILL still works unchanged. The Windows `kill_existing_daemon` path (which uses `taskkill /T`) is untouched — Windows has no SIGTERM, and the new shutdown listener still handles Ctrl+C via tokio there. No new dependencies; tokio's `signal` feature was already enabled at the workspace level.
- **Tailer stays alive when no watch roots exist at startup and attaches watchers as agent directories materialize** (#385) — `workers::tailer::run_blocking` used to resolve `Provider::watch_roots()` exactly once at startup. If every shipped provider returned an empty vec (because `~/.claude`, `~/.codex`, etc. didn't exist yet), the worker logged `no watch roots from enabled providers; tailer exiting` and stayed gone for the lifetime of the process. Two real-world paths hit this: late-mounted encrypted / network `$HOME` under `launchd` / `systemd --user`, and the fresh-install sequence where the user installs Budi before any AI agent. Per ADR-0089 §6 ("invisible install"), that's exactly the papercut 8.2 was meant to remove. `run()` now snapshots `agents.toml` enable/disable flags at boot but no longer filters by filesystem-level `is_available()`; `run_blocking` rebuilds `Routes` on every backstop tick from fresh `Provider::watch_roots()` results; an `attach_new_watchers` helper tracks already-attached roots in a `HashSet<PathBuf>` so reconcile ticks are idempotent and never re-register a root. `seed_offsets` stays a one-shot boot step — a transcript file that first appears after boot under a post-boot-materialized root is treated as live content (ingested from offset 0) rather than skipped as history. Watcher-attach failures during reconcile are now logged at `debug` (previously `warn` for the boot-time attach), so a persistently unreachable root does not spam one warning per 5 s tick; the backstop scan still covers it. Hot-reload of `agents.toml` enable/disable flags remains out of scope.
- **`cloud_sync` worker clears `cloud_syncing` on panic paths** (#343) — `workers::cloud_sync` called `flag.store(false, ...)` manually after `sync_tick` returned. If `sync_tick` ever panicked inside `spawn_blocking`, the flag stayed asserted forever, the next tick could not start, and `POST /cloud/sync` returned `409 Conflict` permanently. The worker now lifts `CloudBusyFlagGuard` out of `routes/cloud.rs`, holds it across `sync_tick`, and relies on RAII drop to clear the flag on both the success path and the unwind path. Two new unit tests exercise the panic-unwind case (the guard's `Drop` still clears the flag) alongside normal drop.
- **Statusline `branch_cost` is scoped to `(repo_id, branch)` instead of `branch` alone** (#347) — developers who keep `main` / `master` / `develop` checked out across several local repos previously saw every repo's activity on that branch silently merged into the statusline `branch:` slot (pre-existing, re-emitted under the 8.1 `#224` provider-scoping pass, flagged in the R2.5 review). `StatuslineParams` gains an optional `repo_id`; when it's set alongside `branch`, `statusline_stats` now filters on `COALESCE(repo_id, '') = ?` so the value is `(repo_id, branch)` rather than `branch` alone. `budi statusline` resolves the active repo identity via the shared `budi_core::repo_id::resolve_repo_id` helper (the same one ingest uses, so the value matches what's stored in `messages.repo_id`) and forwards it alongside `branch` whenever `branch` is forwarded. When the shell is outside a git repo, only `branch` is sent. `docs/statusline-contract.md` documents the new `repo_id` query param and the scoping rule so `siropkin/budi-cursor` and `siropkin/budi-cloud` can adopt it without reading source. No rename or retype of existing fields; `branch_cost` semantics get tighter when callers pass `repo_id`, identical when they don't. `/analytics/branches` and `/analytics/branches/:branch` already grouped / filtered on `(git_branch, repo_id)` and are unchanged.

### Changed

- **`cloud_sync` delegates ticket extraction to the canonical pipeline helper** (#333) — `crates/budi-core/src/cloud_sync.rs` used to carry its own `extract_ticket_from_branch` that diverged from `pipeline::extract_ticket_from_branch` (different integration-branch filter, different ordering, missing the ADR-0082 §9 numeric fallback). Cloud rollups now call through to the pipeline helper, so `budi stats --tickets` and the cloud dashboard see the same ticket value for the same branch. `DailyRollupRecord` and `SessionSummaryRecord` gain a `ticket_source: Option<String>` field that carries the `branch` / `branch_numeric` marker (populated only when `ticket` is set; `serde(skip_serializing_if = "Option::is_none")`, so existing cloud servers that don't know the field still accept envelopes unchanged). ADR-0083 wire-format samples and the Supabase schema snippet are updated so the cloud side has the same column available. Behavioural delta on branches the old local helper silently dropped: `feature/1234` now produces `ticket=1234 ticket_source=branch_numeric` instead of `ticket=None`, matching local CLI output.
- **Classification sibling-tag emission is centralized behind helpers that make siblings non-optional** (#335) — the R1.2 / R1.3 / R1.4 / R1.5 classification emitters' contract says that whenever a first-class dimension tag is emitted (`activity`, `ticket_id`, `file_path`, `tool_outcome`), a sibling `*_source` tag must also be emitted, and for activity / file / tool-outcome a `*_confidence` tag as well. Previously the invariant was maintained by construction in two independent emitters (the batch pipeline in `crates/budi-core/src/pipeline/mod.rs` and the live proxy ingest in `crates/budi-core/src/proxy.rs`), held up only by reviewer discipline. A new `crates/budi-core/src/pipeline/emit.rs` module routes all first-class dimension emissions through helpers (`emit::ticket`, `emit::activity`, `emit::file_paths`, `emit::tool_outcomes`) that accept sibling values as non-optional parameters, so a future contributor cannot silently ship a headline tag alone and degrade `budi stats --activities` / `--tickets` / `--files` (and the equivalent cloud surfaces) to `src=?` / `confidence=?`. Tag keys and values are unchanged; `tool_outcome` multi-value ordering moves from "first-seen `tool_use` order" to deterministic `BTreeSet` lexicographic order (nothing in the codebase relied on the old ordering — existing tests use `contains` / `any` / single-outcome `vec!` asserts).
- **Shared `is_integration_branch` predicate; `tool_result` extraction scope documented** (#336) — two adjacent R1.5 audit gaps from #217, bundled per the ticket. (1) The pipeline ticket extractor treated `main` / `master` / `develop` / `HEAD` as non-feature branches, but `budi_core::work_outcome` only treated the first three. The proxy and JSONL paths normalize detached HEAD to empty today, so this was latent — but any future importer that let the literal string `HEAD` through would reach `branch_was_merged`, where `git merge-base --is-ancestor HEAD main` happily succeeds and the session would be incorrectly credited as `branch_merged`. A new `budi_core::pipeline::is_integration_branch(&str)` helper is now the single source of truth; both `extract_ticket_from_branch` and `derive_work_outcome` call it. `work_outcome` keeps a narrower `MERGE_TARGETS` list (`main` / `master` / `develop`) for the ref-resolution / merge-base loops because `HEAD` is not a meaningful ref to resolve against. (2) The JSONL `tool_result` extractor only walks `UserContent::Blocks` — the only shape Claude Code has ever used for tool results. Picked the documentation branch of the ticket rather than inventing a raw-JSON fallback that would widen classification into untested territory for no current provider. Rustdoc on `extract_user_tool_outcomes` and the `tool_outcome` contract in `SOUL.md` now explicitly state the scope, non-goals, and extension pattern so operators know when the derivation is authoritative.
- **`/cloud/status` counts pending rollups / sessions without rebuilding the full sync envelope** (#344) — `current_cloud_status` used to call `build_sync_envelope` on every `/cloud/status` hit just to take two `.len()`s, materializing every unsynced rollup and session row for each poll. The status endpoint now runs bounded `SELECT COUNT(*)` queries (`count_pending_rollups` / `count_pending_sessions`) that reuse the exact predicates `fetch_daily_rollups` / `fetch_session_summaries` use, so the counts reported by `/cloud/status` stay in lockstep with whatever the next sync tick would actually send. `CloudSyncStatus` field types and meaning are unchanged; the `ready` gate on the counts is preserved, so behaviour for not-configured cloud installs (counts reported as `0`) is identical.
- **Statusline nudges once/day when a custom template still uses the pre-8.2 `{today}` / `{week}` / `{month}` tokens** (#345) — users whose `~/.config/budi/statusline.toml` references the pre-8.2 slot vocabulary keep rendering after the R2.3 rolling-window shift (`#224`), but the underlying number silently moved from calendar to rolling semantics. `docs/statusline-contract.md` documents the shift, but a prompt-hot surface cannot assume a user with a custom template ever reads the contract doc. `budi statusline --format custom` now prints a one-line stderr nudge the first time per UTC day that a rendered template contains any legacy token: `budi: {today} / {week} / {month} in ~/.config/budi/statusline.toml now render the rolling 1d / 7d / 30d values from the statusline contract. Switch to {1d} / {7d} / {30d} to silence this notice.` The "already nudged today" marker lives at `$BUDI_HOME/statusline-legacy-nudge` (defaults to `~/.local/share/budi/statusline-legacy-nudge`). All filesystem errors on read / write are swallowed — the statusline is a prompt-hot path and must never fail a render because a marker file could not be read or written. The default slots path (`slots = [...]`) already normalizes legacy tokens to the canonical `1d` / `7d` / `30d` at load time, so it does not need the nudge. No shell-profile / Cursor-settings / Codex-config mutation paths and no new dependencies.
- **`CloudSyncStatus::configured` is derived from `effective_api_key()` alone** (#346) — `current_cloud_status` was computing `configured` as `config.api_key.is_some() || config.effective_api_key().is_some()`. Since `CloudConfig::effective_api_key()` is `self.api_key.clone().or_else(env lookups)`, the first disjunct is strictly dominated by the second. Collapsed to `config.effective_api_key().is_some()` with a short comment recording why the belt-and-suspenders OR was cosmetic. No behaviour change: `configured` returns the same boolean for every input that was reachable before, including the env-override case where `api_key` is `None` but `BUDI_CLOUD_API_KEY` is set.
- **Daemon returns `503 Service Unavailable` with an actionable body on `/analytics/*` when the schema is stale** (#366) — ships the acceptance criteria originally specified on `#309` as real code, not as a smoke-plan checkbox. A new `require_current_schema` middleware short-circuits every `/analytics/*` route with a structured `503` when the SQLite `user_version` is lower than `budi_core::migration::SCHEMA_VERSION`. The response body (`ok`, `error`, `needs_migration`, `current`, `target`) is emitted through a single `routes::schema_unavailable` helper so future surfaces stay consistent. `POST /sync` now maps its stale-schema-without-`migrate=true` branch to the same `503` body instead of an opaque `500`. `/admin/migrate`, `/admin/repair`, `/health*`, `/sync/status`, `/cloud/status`, and `/favicon.ico` stay un-gated — operators must still be able to observe daemon status and run the recovery endpoints on a stale-schema box. `budi-cli::client::check_response` recognizes the `needs_migration: true` 503 body and surfaces the daemon's actionable message verbatim, so `budi stats` / `budi sessions` / the Cursor extension stop rendering `Daemon returned 503 Internal Server Error: {json}`. Daemon boot also logs a loud `WARN` for `current < target` (and for downgrade `current > target`) and defaults `RUST_LOG` to `info,hyper=warn,reqwest=warn,h2=warn` so `~/.local/share/budi/logs/daemon.log` is not a 0-byte file after spawn — the lingering half of `#309` that the R4.2 disposition was supposed to catch. `routes::internal_error` also logs the full `anyhow` chain for 500s. Behaviour change to be aware of: `POST /sync` with `migrate=false` on a stale DB used to return `500` + an unstructured message; it now returns `503` + the structured `#366` body — only `budi-cli` pattern-matched on the old 500 body, and it has been updated in the same PR.
- **`budi health` renamed to `budi vitals`** (#367) — the old `budi health` verb overlapped too easily with `budi doctor` (daemon/install self-check). The session-vitals command is now `budi vitals` with identical output and the same `--session` flag. `budi health` keeps working in 8.2.x as a hidden backward-compatibility alias: the first invocation each UTC day prints a one-line stderr hint pointing users at `budi vitals`, subsequent invocations on the same day stay quiet. Slated for removal in 8.3. Help output, `after_help`, `README.md`, and `SOUL.md` all describe `budi vitals` as the canonical command.
- **`budi migrate` / `budi repair` / `budi import` moved under `budi db`** (#368) — closes the last R2.1 CLI layout outlier flagged in #225. The three DB admin verbs were the only surviving top-level bare verbs after the 8.1 `budi autostart` / `budi integrations` / `budi cloud` namespace work, and they all operate on the same analytics DB, so they now live under a single `budi db` namespace (`budi db migrate`, `budi db repair`, `budi db import`). The bare verbs still parse in 8.2.x as hidden backward-compatibility aliases: the first invocation each UTC day prints a one-line stderr hint pointing users at the new namespace, subsequent invocations on the same day stay quiet. Slated for removal in 8.3. `budi doctor` recovery hints, the `analytics schema` 503 error body surfaced by `GET /analytics/*` and `POST /sync`, the startup warning the daemon emits when the schema is stale, the empty-stats tip lines, `after_help`, `README.md`, and `SOUL.md` all describe the `budi db …` shape as the canonical form. The 503 wire contract (`needs_migration: true`, `current`, `target`) is unchanged; only the human-readable verb in the `error` field moved. No behavior change to the underlying migrate / repair / import implementations.

### Added

- **Relative `--period` / `-p` windows for `budi stats` and `budi sessions`** (#404) — in addition to the calendar windows (`today`, `week`, `month`, `all`), the CLI now accepts rolling windows of the form `Nd` / `Nw` / `Nm` where `N` is a positive integer (e.g. `budi stats -p 7d`, `budi sessions -p 2w`, `budi stats -p 3m --models`). Days and weeks subtract exactly that many local calendar days / weeks from today; months use calendar-month subtraction (clamped to the end of the target month, so `2026-03-31 - 1m = 2026-02-28`). This aligns the CLI time axis with the rolling `1d` / `7d` / `30d` windows used by the statusline surface and the cloud dashboard (ADR-0088 §4, #350). Parsing is UTF-8 safe, rejects zero (`0d` / `0w` / `0m`), rejects unknown units with an actionable error, and is case-insensitive on the unit suffix. `period_label` renders singular forms (`Last 1 day`, `Last 1 week`, `Last 1 month`) so the output never reads "Last 1 days". No wire-format changes — the daemon still consumes UTC RFC3339 `since` / `until` bounds.

### Process

- **Tailer perf baseline harness for 8.2** (#410) — filed from the R3.5 release code review audit on `#360` finding F-4. The R3.5 audit reviewed the tailer's resource profile from code (single blocking worker, 500 ms debounce, 5 s backstop, no new HTTP listeners) but did not capture a live baseline; with `v8.2.0` shipped, the tailer is the only live ingestion path so future regressions in RSS / FD / CPU are only visible against a measured record. `scripts/e2e/test_410_tailer_baseline.sh` is the reproducible instrument: it stands up an isolated daemon against a `mktemp` `HOME` + `BUDI_HOME`, seeds empty watch roots for all four providers (so the tailer attaches a `notify` watcher per provider), runs a configurable idle soak (default 10 min at 30 s sample interval), replays a synthetic 100-event Claude Code session at 1 ms / event into one watch root while sampling RSS / CPU / FD at 100 ms granularity for a post-burst observation window, counts attached watchers by grepping the daemon log for the tailer's `watching provider=…` span, and prints a Markdown summary table suitable for pasting into the #410 baseline comment. Configurable via `SOAK_SECS`, `SAMPLE_EVERY`, `BURST_EVENTS`, `BURST_GAP_MS`, `BURST_WINDOW_SECS`, `BURST_SAMPLE_MS`; `KEEP_TMP=1` preserves the raw CSVs + daemon log for wiki archiving. Lives under `scripts/e2e/` (not `scripts/research/`, which would have required a `#321`-shaped justifying ticket per the `#407` carve-out) so it sits alongside the existing `test_328_release_smoke.sh` / `test_326_proxy_events_upgrade.sh` harnesses. Ticket closes on posted evidence, not on the harness landing — this entry pins the instrument, not the baseline.
- **Added `cargo-deny` supply-chain policy and CI check** (#409) — filed from the R3.5 release code review audit on `#360` finding F-3. The repo now ships a `deny.toml` pinning a permissive license allowlist (`MIT`, `Apache-2.0`, `Apache-2.0 WITH LLVM-exception`, `BSD-2-Clause`, `BSD-3-Clause`, `BSL-1.0`, `CC0-1.0`, `CDLA-Permissive-2.0`, `ISC`, `0BSD`, `Unicode-3.0`, `Unlicense`, `Zlib`), banning TLS backends that contradict our rustls-only posture (`openssl`, `openssl-sys`, `native-tls`), restricting crate sources to `crates.io`, denying unknown git sources, and treating wildcard version requirements on published registries as errors. CI runs `cargo deny check` on every PR via `EmbarkStudios/cargo-deny-action@v2`; the job is non-blocking for one release cycle before promotion to a required status check. Workspace members are marked `publish = false` to match reality (binaries ship via GitHub Releases, not crates.io) and to let the wildcard policy apply only to external dependencies. `CONTRIBUTING.md` documents the policy and the allowlist-update workflow. The existing inline `cargo audit` step in `.github/workflows/ci.yml` remains in place; the RustSec advisory DB is also consulted by `cargo deny check advisories`.
- **Retired the "R2.1 net-negative binary-size" framing in the 8.2 narrative** (#408) — the R3.5 release code review audit on #360 (finding F-2) showed that against `v8.1.0` on macOS arm64, `budi` grew +1.32 MB (+13.4%) and `budi-daemon` grew +0.22 MB (+1.8%) at the `v8.2.0` tag. The growth is intentional: R2.4 (#394) made `budi doctor` self-contained by opening the analytics DB directly, which pulled `rusqlite` with a bundled SQLite into the CLI binary for the first time, and proxy removal on the daemon side was offset by the `notify` family the tailer brought in. The `#322` R2.1 acceptance criterion "Diff stat shows a net-negative LOC change" was met in `git diff` terms, but the durable narrative in [ADR-0089](docs/adr/0089-reverse-proxy-first-jsonl-tailing-as-sole-live-path.md) §Consequences/Positive previously read "Code surface shrinks substantially. 8.2 R2.1 is a net-negative LOC release" and could be (and was) misread as implying a release-binary-size win. The ADR now reads "Source surface shrinks; binary size is roughly flat" with the measured per-binary deltas and the honest framing *"proxy runtime removed, replaced by a tailer of comparable size, plus a self-contained `doctor`."* The `v8.2.0` [GitHub release notes](https://github.com/siropkin/budi/releases/tag/v8.2.0) and `CHANGELOG.md` §8.2.0 never claimed a binary-size win and are unchanged. Retrospective framing correction noted on #316 and #322.
- **Docs / research discipline rule amended to permit operator-only measurement scripts** (#407) — the `8.2.1` rule "no new files under `docs/research/`, `docs/releases/`, or `scripts/research/`" (inherited from the `#316` epic body and restated on `#396`) now carries an explicit carve-out: an operator-only measurement script may live under `scripts/research/` when it is the explicit deliverable of a tracked ticket and its verdict is load-bearing for an ADR, release decision, or other durable record. Narrative output from running such a script still belongs in the wiki or a durable issue comment, not in `docs/research/`. The carve-out exists so `scripts/research/cursor_usage_api_lag.sh` (#321, [ADR-0089 §7](docs/adr/0089-reverse-proxy-first-jsonl-tailing-as-sole-live-path.md)) can stay in the repo as the durable in-tree artifact of the Cursor Usage API lag measurement without being treated as a rule exception in every subsequent audit pass. The rule wording on `#396` is updated to match; the broader `docs/research/` / `docs/releases/` prohibition is unchanged — `v8.2.1` release notes still live on the GitHub release page and research narrative still belongs in the wiki. `ADR-0089 §7` now points at this amendment so the cross-reference reflects the post-`8.2.0` state rather than the original `#316` rule 12 wording.
- **`README.md` and `docs/statusline-contract.md` explain rolling vs calendar window semantics** (#350) — propagates the R3.3 code-review note from #231 into user-facing docs. `README.md` now has a "Windows: rolling vs calendar" section right after the CLI reference so developers understand why the `1d` / `7d` / `30d` chip on `budi statusline` (rolling, last-24h / last-7d / last-30d ending now) and the `budi stats` / cloud dashboard cost charts (calendar today / last-7 / last-30 calendar days) can legitimately show different totals for the "same" window. `docs/statusline-contract.md` cross-links back to the new README section and extends its existing rolling-vs-calendar note to mention that the cloud dashboard's cost charts use calendar semantics (the statusline contract itself remains the only surface that stays rolling end-to-end). No code change; ADR-0088 §4 already governs the rolling-statusline rule — this is the `#371`-shaped docs propagation the #396 lessons call out.
- **`proxy_events` schema dimension follow-up closed on evidence** (#334) — the R1.6 audit (#217) filed #334 for missing first-class dimension columns (`ticket_source`, `activity`, `activity_source`, `activity_confidence`) on the legacy `proxy_events` table. The table itself was dropped in the `v8.2.0` R2.5 wrapper-removal pass (#326), so there's nothing left to backfill. ADR-0089, `SOUL.md`, and `CHANGELOG.md` §8.2.0 already record the decision; the ticket closes on that evidence rather than on a schema change.

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

- **R4.2 smoke test plan for v8.1.0** drafted (#297). New `docs/releases/8.1.0-smoke-tests.md` (archived to the [v8.3.0 `docs/` + `scripts/` audit comment thread](https://github.com/siropkin/budi/issues/365#issuecomment-4292479838) when that path was removed) is the structured release-gate plan for v8.1.0 — it ports forward the 8.0 regression set from #280 where behavior changed, adds explicit per-test coverage for every user-visible 8.1 surface (R1.0 attribution bugs #302–#305, R1.4 file-level attribution #292, R1.5 tool / session outcomes #293, R2.1 CLI normalization + `budi cloud sync` / `status` #225, R2.2 onboarding #228, R2.3 statusline #224, R2.4 Cursor extension as onboarding entry point #314, R3.1 cloud dashboard windows + linking flow #235, R3.2 Cursor extension alignment #232), folds the four threaded comments from #297 into first-class test IDs (`ST-81-SV-01..04`, `ST-81-CX-01..04`, `ST-81-CL-01..05`), pins a 16-test minimum-viable pre-release set as the tag gate, defines the required PASS/FAIL comment shape for #297, and records #309's stale-schema disposition as a release-blocking check (503 + actionable error vs opaque 500). No code changes in this ticket — it is the release-gate plan itself. The hard release gates remain unchanged: #297 must close with a full PASS record and #296 must be merged in `siropkin/getbudi.dev` before #202 (R4.3) may tag v8.1.0. Governed by ADR-0088 §3.
- **R4.1 release readiness for v8.1.0** drafted (#230). New `docs/releases/8.1.0.md` (archived to the [v8.3.0 `docs/` + `scripts/` audit comment thread](https://github.com/siropkin/budi/issues/365#issuecomment-4292479109) when that path was removed) is the single place where the tag-blocking checklist lives and where the GitHub release notes are drafted before #202 runs. It records the full roadmap closure (R1.0 through R3 review passes) with issue numbers, the validation matrix for the four repos in the ecosystem, the explicit release artifact checklist, the privacy re-check against ADR-0083 now that file-level attribution (#292) and tool / session outcomes (#293) have shipped, and the explicit deferrals into 8.2 (#316 / #294, with 8.3 absorbing the broader-agent coverage per ADR-0089) and 9.0 (#159). It also calls out #309 (opaque 500 on stale analytics schema) as the single known-open bug in the `8.1.0` milestone at R4.1 drafting time and documents the disposition rule: covered by the R4.2 smoke run (#297); if the smoke run reproduces it, it blocks v8.1.0 because it contradicts the R2.2 first-run-trust promise, otherwise it moves to 8.2. No code changes in this ticket — it is release-readiness documentation. Governed by ADR-0088 §3. The hard release gates (R4.2 #297 PASS record and #296 merged in `siropkin/getbudi.dev`) are still required before #202 tags.
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
