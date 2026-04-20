# ADR-0089: Reverse Proxy-First Architecture — JSONL Tailing as Sole Live Path

- **Date**: 2026-04-17
- **Status**: Accepted
- **Accepted on**: 2026-04-18 (promotion criteria below all satisfied; recorded in [#356](https://github.com/siropkin/budi/issues/356) R1.7 docs review pass)
- **Issue**: [#317](https://github.com/siropkin/budi/issues/317)
- **Milestone**: 8.2.0 (epic: [#316](https://github.com/siropkin/budi/issues/316))
- **Supersedes**: [ADR-0082](./0082-proxy-compatibility-matrix-and-gateway-contract.md)
- **Amends**: [ADR-0081](./0081-product-contract-and-deprecation-policy.md) §Provider System, [ADR-0088](./0088-8x-local-developer-first-product-contract.md) §2 and §5

## Context

ADR-0081 declared Budi 8.0's product contract with a hard choice: the proxy would be "the sole live ingestion path," JSONL file sync would be "removed from the continuous sync loop when proxy mode ships," and adding a new agent would reduce to documenting its base URL configuration. ADR-0082 operationalized that into a compatibility matrix, a gateway contract, and an attribution protocol based on `X-Budi-Repo` / `X-Budi-Branch` / `X-Budi-Cwd` / `X-Budi-Session` request headers. ADR-0088 reaffirmed the proxy as the sole live ingestion path in the 8.x local-developer-first contract.

Three things happened in practice between 8.0 and 8.1 that contradict the proxy-first contract:

1. **No stock agent emits the `X-Budi-*` headers.** `budi enable <agent>`, `budi launch <agent>`, and the proxy install flow all inject `ANTHROPIC_BASE_URL` / `OPENAI_BASE_URL` / `COPILOT_PROVIDER_BASE_URL` so the agent talks to `http://localhost:9878`. None of them inject the attribution headers the proxy needs to assign `repo_id` and `git_branch`. Live proxy attribution depends on session-level backfill from later messages that happen to carry context — not on live capture.
2. **The value-add features do not run on the proxy path.** 8.1's R1.3 ticket extraction (#221), R1.4 file-level attribution (#292), and the rest of the classification stack are implemented in `Pipeline::process` and the `Enricher` chain (`GitEnricher`, `ToolEnricher`, `FileEnricher`, `TagEnricher`). These run on JSONL import. The proxy path calls a different code path (`insert_proxy_message`) that bypasses the pipeline, reads `cwd` from headers that are empty, and leans on session-level SQL backfill to fill in the blanks. Deep classification on live proxy data is, in practice, not happening.
3. **JSONL transcripts already carry the full context.** Claude Code, Codex, and Cursor write structured JSONL to disk with `cwd` and `gitBranch` fields on every message. `budi import` reads them today, runs `Pipeline::default_pipeline()`, and produces correctly classified rows — without needing a proxy, a base URL, or attribution headers. The 8.1 analytics the product actually ships on are already JSONL-derived.

On top of the contract drift, the proxy path carries costs that the product has started paying in real user friction:

- Shell profile mutation (`~/.zshrc`, `~/.bashrc`, `~/.config/fish/config.fish`) that enterprise developers flag on first install, because Budi modifying a shared dotfile during onboarding is a hard no for a non-trivial fraction of the target persona.
- Cursor `settings.json` and `~/.codex/config.toml` mutation that breaks when users later reconfigure those tools, creating support load.
- `budi launch <agent>` as a CLI wrapper that users won't use in practice. Developers run `claude` or `codex` or click Cursor — they do not launch agents through Budi. The launch command exists primarily because without it the attribution headers cannot be injected. See point (1).
- A daemon-is-single-point-of-failure risk: if the Budi daemon dies, every agent that has `ANTHROPIC_BASE_URL=http://localhost:9878` exported also stops working. The "invisible background tool" persona becomes "that thing that broke my AI coding session" the first time the daemon crashes.
- A full second protocol maintenance burden. Anthropic's Messages API and OpenAI's Chat Completions API both evolve; the proxy has to keep forwarding correctly through every version change, including streaming edge cases, tool-use block changes, and new parameters. This is protocol work that does not move the product forward.
- The `proxy_cutoff` rule in `analytics/sync.rs`: a ~30-line dedup patch that exists only because we now have two ingestion paths racing on the same messages. It is a bug we are paying to maintain, not a feature.
- "Unassigned" live rows in stats output because attribution cannot be resolved in real time. The most common user-visible symptom of the contract drift.

The underlying problem is that the 8.0 / 8.1 architecture over-indexed on the theoretical benefits of live proxy ingestion (sub-second freshness, inline classification, single path) and under-counted the operational costs of actually making it work (header injection in agents we do not control, daemon criticality, two-code-path maintenance, onboarding surface mutation). Meanwhile, the JSONL path quietly became the one that actually delivers the product's value propositions.

This ADR closes the drift by matching the contract to the implementation — and then deleting the code that supported the old contract.

## Decision

### 1. JSONL tailing is the sole live ingestion path

Budi's live ingestion is a file watcher over agent-written transcript files. Every supported agent implements a `Provider` trait with:

- `discover_files() -> Vec<DiscoveredFile>` — one-shot enumeration, used by `budi import` for backfill
- `parse_file(path, content, offset) -> (messages, new_offset)` — incremental parse
- `watch_roots() -> Vec<PathBuf>` — directories the daemon's tailer watches live (new in 8.2, see [#318](https://github.com/siropkin/budi/issues/318))
- `sync_direct(...)` — optional, only for agents with a real Usage API (currently Cursor)

A single daemon-side worker (new in 8.2, see [#319](https://github.com/siropkin/budi/issues/319)) uses `notify` for filesystem events, with polling fallback where needed. It maintains per-file offsets in a new `tail_offsets` table, calls `Provider::parse_file` with the stored offset on each event, feeds the resulting messages through `Pipeline::default_pipeline()`, and writes to the same `messages` / tag tables the batch `budi import` writes to. There is no second code path.

### 2. The proxy is removed

All of the following code is deleted in 8.2 R2.1 (#322):

- `crates/budi-core/src/proxy.rs`
- `crates/budi-daemon/src/routes/proxy.rs`
- `crates/budi-cli/src/commands/proxy_install.rs`
- `crates/budi-cli/src/commands/launch.rs`
- CLI subcommands `budi launch`, `budi enable <agent>`, `budi disable <agent>`
- Anthropic Messages + OpenAI Chat Completions protocol types that existed only for pass-through
- Integration tests exercising proxy forwarding

The `X-Budi-Repo` / `X-Budi-Branch` / `X-Budi-Cwd` / `X-Budi-Session` header contract is deprecated. No client-side tooling is ever going to emit them, and the JSONL path does not need them.

### 3. The install surface stops mutating user config

Starting in 8.2 R2.2 (#323) and R2.3 (#324), `budi init` does exactly four things:

1. Creates the Budi data directory
2. Registers and starts the daemon (launchd / systemd user unit / Windows Service)
3. Prints the list of detected agents based on `Provider::watch_roots()` existence
4. Exits

No shell profile writes. No Cursor `settings.json` patching. No `~/.codex/config.toml` mutation. No prompts. No flags beyond `--cleanup`.

`budi init --cleanup` is a new subcommand that removes previously-injected 8.1-era Budi blocks from shell profiles and agent configs. It is opt-in, shows a diff preview, and asks for confirmation. It is the only surviving path that writes to user config files, and it only deletes.

### 4. Attribution comes from the transcript, not from headers

Every supported agent already writes `cwd` and `gitBranch` into its local JSONL on every message (Claude Code, Codex, Cursor, Copilot). The `GitEnricher` consumes these directly inside the pipeline. The ticket extraction rules from ADR-0086 and #221 apply unchanged. File-level attribution (#292) continues to run in `FileEnricher`.

There is no live attribution header contract. There is no session-level SQL backfill shim. There is only the pipeline, running on the transcripts the agent already writes.

### 5. Latency budget: single-digit seconds is accepted

All downstream Budi surfaces — `budi stats`, `budi sessions`, `budi status`, `budi statusline`, `budi doctor`, daemon analytics routes — explicitly accept tail latency on the order of 1–10 seconds. Filesystem events fire within ~1 s on all three supported OSes under normal conditions; the pipeline adds tens of milliseconds. No surface in 8.x requires sub-second freshness. The design-principles doc (§4) is rewritten to reflect this.

### 6. Daemon outage does not break the user's agent

Because the agent no longer routes traffic through the proxy, a Budi daemon crash is invisible to the agent. The user keeps working. When the daemon comes back, the tailer picks up from the persisted offsets and catches up. This is a hard design property, not an accident. It is also the single biggest user-facing argument for this ADR.

### 7. Fallback policy: none

There is no proxy fallback. There is no dual-path reconciliation. There is no "if JSONL isn't there, use the proxy" branch. Having two live paths was the bug. If a future agent cannot be tailed (no on-disk transcript), that is a scoping issue for that agent, not a license to reintroduce two paths of ingestion for everyone.

The Cursor Usage API remains a **pull** used only for cost/token reconciliation where the JSONL does not carry that data. It is scheduled separately from the tailer and is not part of the live hot path. Its lag profile is measured in [#321](https://github.com/siropkin/budi/issues/321), and the verdict (below) shapes the UX of cost surfaces, not the live hot path.

The measurement instrument lives at `scripts/research/cursor_usage_api_lag.sh`. It reads Cursor's auth from `state.vscdb` exactly as `crates/budi-core/src/providers/cursor.rs::extract_cursor_auth` does, polls the dashboard endpoint, baselines the first page (a critical methodology detail — see the verdict comment below for why), and records `lag_ms = first_seen_at_ms − event_timestamp_ms` for every event that appears on subsequent polls. The numeric verdict and recommendation are published as a comment on #321 ([verdict](https://github.com/siropkin/budi/issues/321#issuecomment-4275063605)); the wiki was the original target and remains the long-term home for the narrative memo, but was uninitialized at `8.2.0` tag time so the verdict lives on the issue. The script itself stays under `scripts/research/` as the durable in-tree artifact per the `8.2.1` carve-out to the docs/research discipline rule ([#396](https://github.com/siropkin/budi/issues/396), [#407](https://github.com/siropkin/budi/issues/407)): operator-only measurement scripts that are the explicit deliverable of a tracked ticket and whose verdict is load-bearing for an ADR may live under `scripts/research/`; narrative output still belongs in the wiki or a durable issue comment.

**Verdict (real-machine run, agent session ~30 min, model `claude-opus-4-7-thinking-high`, N = 12 fresh events):** `min` = 1.6 s, `p50` = 69.5 s, `p90` = 6.0 min, `p99` = 6.2 min, `max` = 6.2 min. The lag is therefore **bounded but not real-time**: roughly half of agent calls take longer than one minute to surface in the API, and the slowest event in this window took ~6 minutes. This rejects §C.a of #321 (treating the API as a real-time path) and does not motivate §C.b (Cursor-only proxy passthrough), and **adopts §C.c**: the Usage API stays a scheduled pull driven by `budi sync`, with a UX-level "Cursor cost data may lag up to ~10 minutes" disclaimer surfaced wherever per-call Cursor cost is shown. This is consistent with §1 of this ADR (live ingestion = JSONL only): the Usage API was never the live cost path, and 8.2's contribution is to be honest about that.

Caveats are spelled out in the verdict comment: N = 12 is small (so `p99` ≈ `max` and the precise tail is loose), the sample covers a single model and a single user-session, and the run was duration-bounded rather than event-bounded. The qualitative verdict ("p50 > 1 min, tail bounded under ~10 min") is stable; the numeric tail should be re-measured if operator complaints surface or before any decision to revisit §C.b. The follow-up UX disclaimer work is **not** a #321 deliverable and is tracked separately.

### 8. Plugin model is preserved

The `Provider` trait is the only extension point. Adding a new agent under the future coverage epic ([#294](https://github.com/siropkin/budi/issues/294)) is one new `Provider` impl plus its registration — no proxy adapter, no base URL matrix, no env-var injection, no shell profile work. The plugin model survives intact; what changes is that it is also the live model, not just the import model.

## Consequences

### Positive

- **One code path, one contract.** The live ingestion path and the historical import path are the same path. Every feature (ticket extraction, file attribution, activity classification, tool outcomes) lands for both, always. No more "proxy mode doesn't do that yet."
- **Invisible install.** The single biggest onboarding objection (shell profile mutation) is gone. `budi init` becomes a ten-second, no-decisions operation.
- **Daemon outage is safe.** Users keep coding. Budi catches up. The "what if the daemon dies while I'm on a deadline" fear vanishes.
- **Protocol maintenance burden drops.** Anthropic and OpenAI API evolutions are no longer Budi's problem.
- **Code surface shrinks substantially.** 8.2 R2.1 is a net-negative LOC release — a rare and healthy property.
- **Plugin story becomes real.** Adding Gemini CLI or Windsurf in 8.3 is one `Provider` impl away, not a proxy adapter plus a compatibility matrix plus setup docs per agent.

### Negative

- **Accepts 1–10 s freshness instead of sub-second.** The statusline is no longer live-live. In practice this is already the case because the statusline polls the daemon on a schedule, but it is worth naming.
- **Forces an 8.1 → 8.2 breaking upgrade.** Users with 8.1 exports in their shell profile need to run `budi init --cleanup`. Release notes must lead on this.
- **Concedes that agents not writing transcripts cannot be supported.** Any future agent that only holds state in-memory is out of scope for the tailer path until that agent ships a transcript option. We treat this as a scoping decision per agent, not a whole-product fallback.
- **Cursor cost/token accuracy depends on Usage API cadence.** This is an existing limitation, but it stops being mitigated by proxy pass-through. The [#321](https://github.com/siropkin/budi/issues/321) measurement is the gate on whether any compensating work is needed.
- **Proxy-era history stays in `messages`, not in `proxy_events`.** [#326](https://github.com/siropkin/budi/issues/326) settles the upgrade policy: 8.2 drops the obsolete `proxy_events` table on migration, keeps proxy-sourced `messages` rows read-only for historical analytics, and surfaces that retained legacy state in `budi doctor`.

### Neutral

- Privacy envelope is unchanged. ADR-0083 still governs. The tailer reads the same files `budi import` already reads.
- Cloud sync is unchanged. The cloud consumes the `messages` table regardless of which ingest path populated it.
- Provider-scoped status contract (#224) is unchanged.
- MCP server reintroduction (9.0) is unchanged.

## Alternatives Considered

### A. Keep the proxy as the sole live path and fix the attribution header gap

Requires shipping a client-side shim that wraps every supported agent (`claude`, `codex`, `cursor`, `copilot`, `gh-copilot-chat`, any future agent) and injects `X-Budi-*` headers before forwarding requests to the real proxy. This is effectively building `budi launch` for every agent, forever, and convincing users to use it. Rejected as not achievable: enterprise developers will not route their agents through a Budi-provided wrapper, and there is no stable way to do it for GUI-based tools like Cursor without patching the app itself.

### B. Dual-path: keep both proxy and tailer

What 8.1 effectively is today, unintentionally. It keeps all the costs of the proxy (shell mutation, protocol burden, daemon criticality) while adding tailer complexity, and still requires `proxy_cutoff` dedup. The worst of both worlds on an ongoing basis. Rejected.

### C. Tailer-first, proxy retained for Cursor cost capture only

Considered seriously. The argument is that Cursor's JSONL does not carry per-request tokens and costs — those come back via the Usage API, which is currently polled once per `budi sync`. If the Usage API lag is materially larger than the proxy's real-time capture, Cursor users pay a visible freshness cost from this ADR.

Rejected as the default, conditionally reconsiderable. [#321](https://github.com/siropkin/budi/issues/321) measures the lag empirically. If the lag is bounded and acceptable (expected outcome based on prior spot checks), the Usage API pull is sufficient. If it is not, a narrowly-scoped Cursor-only proxy passthrough for cost capture can be reintroduced as a follow-up ADR — but that reintroduction would be measured in hundreds of lines of code, not thousands, and would not touch shell profiles, `budi launch`, or the attribution model.

### D. Defer the pivot to 9.0

Considered. Rejected because the contract drift is visible to users today (unassigned rows, "my AI broke because the daemon died" support stories, `budi launch` as an unused subcommand that still ships) and because every release that does not reverse the contract spends cycles maintaining a path the product does not actually use. 8.2 is scoped intentionally narrow to make this a credible single-release pivot.

## Amendments to Prior ADRs

This ADR amends the following prior decisions. A banner pointing at this ADR is added to each affected file:

### ADR-0081 §Provider System

The statement that "JSONL file sync will be removed from the continuous sync loop when proxy mode ships (R2)" is rescinded. JSONL tailing **is** the continuous sync loop in 8.2 and after. The `Provider` trait is extended with `watch_roots()` to serve that loop.

### ADR-0082 (entire document)

Superseded. The proxy compatibility matrix, gateway contract, streaming behavior, and attribution header protocol are retired in 8.2 R2.1. Agent compatibility in 8.2+ is a function of `Provider::discover_files` + `Provider::parse_file` + `Provider::watch_roots`, not of proxy base-URL configuration.

### ADR-0088 §2 and §5

§2's table row "Proxy | `siropkin/budi` | Sole live ingestion path (ADR-0082)." is replaced with "Tailer | `siropkin/budi` | Sole live ingestion path (ADR-0089). Filesystem-watches agent transcripts; no proxy." The proxy row is removed entirely.

§5's language on "rule-based activity/ticket/branch/file/outcome signals inside the proxy + pipeline" is replaced with "inside the pipeline, over JSONL tailed from agent transcripts." The proxy is no longer mentioned.

### `docs/design-principles.md` §4 ("Proxy-First Architecture (8.0+)")

Rewritten in full in [#317](https://github.com/siropkin/budi/issues/317) as "JSONL Tailing as Sole Live Path (8.2+)". The principle "Don't reintroduce hooks, OTEL, or continuous file watching for live data" is reversed: continuous file watching **is** the live data mechanism.

### `SOUL.md`

Any section that still describes the proxy as the live path is updated during the 8.2 docs passes; `#359` is the Round 2 scrub that removes the remaining misleading live-path language.

## Promotion Criteria

This ADR is promoted from `Proposed` to `Accepted` only when all of the following are true. As of 2026-04-18 every entry below is satisfied and the status banner at the top of this document reads `Accepted`.

- [#321](https://github.com/siropkin/budi/issues/321) Cursor Usage API lag verdict is published — **satisfied**: instrument shipped (`scripts/research/cursor_usage_api_lag.sh`), real-machine run completed, numeric verdict and §C.c recommendation posted as a [comment on #321](https://github.com/siropkin/budi/issues/321#issuecomment-4275063605), §7 above embeds those findings, and the recommendation is consistent with this ADR's §7
- [#318](https://github.com/siropkin/budi/issues/318) `Provider::watch_roots()` is merged — **satisfied** (PR #369)
- [#319](https://github.com/siropkin/budi/issues/319) daemon tailer is merged behind `BUDI_LIVE_TAIL=1` — **satisfied** (PR #370)
- [#320](https://github.com/siropkin/budi/issues/320) tailer is promoted to default and proxy ingestion is short-circuited — **satisfied** (PR #372)

The 8.2 R2 proxy-removal work is unblocked once the R1 exit gate ([#362](https://github.com/siropkin/budi/issues/362), R1.8 smoke + E2E) closes with a full PASS — proxy + tailer must demonstrate analytics parity through at least one RC before the proxy module is deleted in [#322](https://github.com/siropkin/budi/issues/322). That gating is intentional: removing the proxy before the tailer is trusted is the one failure mode this ADR is trying to avoid.

## References

- [ADR-0081: Product Contract and Deprecation Policy](./0081-product-contract-and-deprecation-policy.md)
- [ADR-0082: Proxy Compatibility Matrix and Gateway Contract](./0082-proxy-compatibility-matrix-and-gateway-contract.md) (superseded by this ADR)
- [ADR-0083: Cloud Ingest Identity and Privacy Contract](./0083-cloud-ingest-identity-and-privacy-contract.md)
- [ADR-0086: Extraction Boundaries](./0086-extraction-boundaries.md)
- [ADR-0088: 8.x Local-Developer-First Product Contract](./0088-8x-local-developer-first-product-contract.md) (amended by this ADR)
- [#316](https://github.com/siropkin/budi/issues/316) — 8.2.0 Invisible Budi epic
- [#317](https://github.com/siropkin/budi/issues/317) — this ADR's tracking issue
- [#201](https://github.com/siropkin/budi/issues/201) — 8.1.0 epic (must ship before 8.2 work starts)
- [#294](https://github.com/siropkin/budi/issues/294) — future AI coding tool coverage epic (built on the tailer)
