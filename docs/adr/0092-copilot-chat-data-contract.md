# ADR-0092: Copilot Chat Data Contract (Local Tail + GitHub Billing API)

- **Date**: 2026-05-06
- **Status**: Accepted
- **Issue**: [#649](https://github.com/siropkin/budi/issues/649)
- **Milestone**: 8.4.0 (epic: [#647](https://github.com/siropkin/budi/issues/647))
- **Related**: [ADR-0088 §7](./0088-8x-local-developer-first-product-contract.md) (host-scoped vs. provider-scoped surfaces, amended in [#648](https://github.com/siropkin/budi/issues/648)), [ADR-0089](./0089-reverse-proxy-first-jsonl-tailing-as-sole-live-path.md) (local-tail-first live-path contract), [ADR-0090](./0090-cursor-usage-api-contract.md) (Cursor Usage API contract — direct precedent for this ADR), [ADR-0091](./0091-model-pricing-manifest-source-of-truth.md) (pricing manifest used to dollarize tailed tokens)

## Context

Budi 8.4 extends the host extension to live inside both Cursor and VS Code, with **GitHub Copilot Chat** as the first non-Cursor provider. Unlike Cursor — where per-message tokens and cost are read from the local `state.vscdb::cursorDiskKV` bubbles (ADR-0090, §2026-04-23) and the Cursor Usage API plays a supplementary overage-attribution role — Copilot Chat splits its surface across two upstreams:

1. **Local JSON/JSONL** under VS Code's `workspaceStorage` and `globalStorage`. Per-request tokens and model id land here within seconds of the wire call. This is the live signal.
2. **GitHub Billing API** (`/users/{username}/settings/billing/premium_request/usage`). Per-user dollar truth-up for individually-licensed users. Empty for org-managed-license users.

Both surfaces are undocumented contracts owned by GitHub, and the local format in particular has shifted at least four times in the last year — VS Code delta → Copilot CLI shape → legacy `usage.*` shape → Feb-2026 `result.metadata.*` shape. Without a versioned ADR pinning the contract, the next Copilot Chat release silently breaks the parser and the next Budi train re-discovers the format from scratch. ADR-0090 set the precedent for this pattern with the Cursor Usage API; this ADR is the Copilot Chat sibling.

The implementation tickets that land against this contract are [#651](https://github.com/siropkin/budi/issues/651) (R1.4 — local tailer) and [#652](https://github.com/siropkin/budi/issues/652) (R1.5 — `sync_direct` reconciliation against the Billing API). This ADR is the spec they implement to; any change in upstream behavior is handled by amending this ADR in the same PR as the parser update so the contract and the code never disagree.

## Decision

The Copilot Chat data contract below is the authoritative Budi-side description of the surface. The forthcoming Copilot Chat provider (`crates/budi-core/src/providers/copilot_chat.rs`, R1.4) reads and writes against this contract; the `sync_direct` reconciliation worker (R1.5) consumes the Billing API contract in §3. Any divergence in upstream behavior is handled by amending this ADR in lockstep with the parser change.

### 1. Provider identity and scope

- **Provider id**: `copilot_chat`. Distinct from the existing `copilot_cli` provider (`crates/budi-core/src/providers/copilot.rs`) — Copilot CLI tails `~/.copilot/session-state/` and is unrelated to the VS Code/Insiders/Codium/Cursor extension surface.
- **Host scope**: Copilot Chat is a VS Code-family provider only. The same provider plugin handles every VS Code variant (stable, Insiders, Exploration, VSCodium, Cursor) and remote-server installs because all of them write to identically-shaped `User/` directories.
- **Surface scope**: provider-scoped on the cloud dashboard, on `budi stats`, and on `budi statusline --format json` with a single `?provider=copilot_chat`. Host-scoped only when aggregated alongside other providers via the multi-provider statusline endpoint (R1.3, [#650](https://github.com/siropkin/budi/issues/650)) per ADR-0088 §7.

### 2. Local-tail contract

#### 2.1 Path roots (per OS)

The provider iterates the cross-product of OS-specific application-support roots and a small set of VS Code-family directory names. Anything matching is a candidate root; missing roots are silently skipped.

- **macOS**: `~/Library/Application Support/{Code,Code - Insiders,Code - Exploration,VSCodium,Cursor}/User`
- **Linux**: `~/.config/{Code,Code - Insiders,Code - Exploration,VSCodium,Cursor}/User`
- **Windows**: `%APPDATA%\{Code,Code - Insiders,Code - Exploration,VSCodium,Cursor}\User`
- **Remote / dev-container**: `~/.vscode-server/data/User`, `~/.vscode-server-insiders/data/User`, `~/.vscode-remote/data/User`, `/tmp/.vscode-server/data/User`, `/workspace/.vscode-server/data/User`

The remote roots cover SSH remote, dev containers, Codespaces, and VS Code Tunnels. They are checked unconditionally on every host so a developer who SSHes into a workstation that itself has Copilot Chat running picks up both local and remote sessions.

#### 2.2 Subpaths under `User/`

Five candidate subpath shapes, all checked. The `{GitHub,github}` brace expansion is required because the Copilot extension's publisher-id casing has flipped at least once between releases.

- `workspaceStorage/<hash>/chatSessions/`
- `workspaceStorage/<hash>/{GitHub,github}.copilot-chat/{chatSessions,debug-logs}/`
- `workspaceStorage/<hash>/{GitHub,github}.copilot/{chatSessions,debug-logs}/`
- `globalStorage/emptyWindowChatSessions/`
- `globalStorage/{GitHub,github}.copilot-chat/**` (recursive)
- `globalStorage/{GitHub,github}.copilot/**` (recursive)

The two `globalStorage/{GitHub,github}.copilot{,-chat}/**` patterns are intentionally recursive: GitHub has shipped at least three different sub-directory layouts under that prefix (`chatSessions/`, `chat-sessions/`, `sessions/<lang>/`), and pinning to any one shape regresses on the next release. The recursion bottoms out at any `*.json` or `*.jsonl` file.

#### 2.3 File shapes and token-key dispatch

Each candidate path is read as a stream of newline-delimited JSON (`*.jsonl`) or as a JSON document (`*.json`). Both forms wrap their per-message records in an envelope key — the parser flattens the envelope before applying the token-key dispatch.

**Envelope keys** (any one of these may be present; first match wins):

| Format | Envelope shape | Notes |
|---|---|---|
| JSONL mutation log (v4, 8.4.1) | `{ "kind": 0\|1\|2, "k": [...], "v": ... }` | The `github.copilot-chat` extension's authoritative on-disk shape since VS Code 1.109 / extension ≥0.47.0. `kind: 0` is a full session snapshot; `kind: 1` is a `set` mutation at JSON-Pointer-shaped path `k`; `kind: 2` is an array splice/append at `k`. Tokens land via later `kind: 1` patches like `{"kind":1,"k":["requests",8,"completionTokens"],"v":39}` — never at the top of the line. The parser **replays** the mutation log onto a per-session reducer state and applies the token-key dispatch below to the **materialized** request, not the raw line. |
| JSONL line wrapper (v2/v3 compat) | `{ "kind": N, "v": [ ... ] }` | Pre-mutation-log shape kept for hand-trimmed fixtures and any build that wraps records in `v` without `k`. The reducer treats `kind: 2` with no `k` as an append to `requests`. |
| JSON document | `{ "requests": [ ... ] }` | Persisted-on-close session snapshot. Each item in `requests` is a turn with optional `result.metadata.{promptTokens,outputTokens}`. |
| JSON document (legacy) | `{ "messages": [ ... ] }` | Older synthetic-fixture shape. Retained for back-compat. |
| Bare record (no envelope) | `{ ...token keys... }` | Treated as a single-record envelope. Used by the v1 unit fixtures and any future format that drops the wrapper. |

**Token-key shapes** (applied to each flattened record; first non-zero pair wins):

| Format | Shape origin | Input-tokens key | Output-tokens key |
|---|---|---|---|
| VS Code delta | original VS Code Copilot extension delta-event format | `promptTokens` | `outputTokens` |
| Copilot CLI | shape inherited from the standalone `copilot` CLI in 2025 | `modelMetrics.inputTokens` | `modelMetrics.outputTokens` |
| Legacy | the OpenAI-ish shape Copilot Chat used through 2025-Q4 | `usage.promptTokens` | `usage.completionTokens` |
| Feb 2026+ | nested-result shape introduced in the Feb-2026 Copilot Chat release | `result.metadata.promptTokens` | `result.metadata.outputTokens` |
| Output-only fallback (v3, 8.4.0) | shape used by VS Code Copilot Chat builds circa May-2026 that persist response token counts but **not** prompt token counts | _(input = 0)_ | `completionTokens` |

The parser tries the **four full-pair shapes** in the order above per record and uses the first one that yields both a non-zero input and a non-zero output count. If none match, it tries the **output-only fallback** as a last resort and emits the row with `input_tokens = 0`. Records that match no shape — full-pair or fallback — are skipped (see §2.6).

The envelope split was added in 8.4.0 after the smoke-gate fixtures uncovered that real on-disk JSONL files write tokens at `v[].result.metadata.{promptTokens,outputTokens}` rather than at the top of the line — the four token-key shapes are unchanged, the parser just looks one level deeper before applying them. `MIN_API_VERSION` was bumped to `2` in lockstep (§2.6).

The output-only fallback was added in the same 8.4.0 cycle after smoke-gate verification on a freshly captured session uncovered that newer VS Code Copilot Chat builds (May-2026) persist `completionTokens` at the top of each response record but **drop the prompt-token counterpart entirely** — `result.metadata` no longer contains `promptTokens` or `outputTokens` for these records. The fallback is the only shape allowed to relax the both-non-zero invariant from the four full-pair shapes; rows it emits flow through downstream pricing as output-only at the manifest layer and are truthed up to the real bill by the §3 Billing API reconciliation worker on the next tick (for users with a configured PAT). `MIN_API_VERSION` was bumped to `3`.

**Mutation-log reducer (v4, 8.4.1, R1.1, [#668](https://github.com/siropkin/budi/issues/668)).** VS Code 1.109+ (and `github.copilot-chat` ≥0.47.0) persist sessions as a JSON Pointer mutation log: a `kind:0` snapshot followed by `kind:1` set-at-pointer and `kind:2` array-splice patches. Token counts arrive on later `kind:1` patches like `{"kind":1,"k":["requests",8,"completionTokens"],"v":39}` — buried inside `k`, never at the top of the line. The v3 parser saw these as flat records with no token keys at the top level and emitted **zero rows** from active sessions on extension ≥0.47.0; only historical sessions whose `kind:0` snapshot already inlined the tokens produced rows. The v4 parser is therefore a per-session reducer:

- `kind:0` — `v` is the initial state. Top-level keys are merged into the reducer state (so `state.requests`, `state.sessionId`, etc. start populated).
- `kind:1` — `set` at JSON-Pointer-shaped path `k` to value `v`. Numeric segments index arrays (auto-grown with placeholder objects/arrays); string segments key objects (auto-created on missing intermediates).
- `kind:2` — array splice/append at `k`. `v` is an array of items to push. When `k` is missing or empty (hand-trimmed fixtures and a few older builds), defaults to `["requests"]`.

After each mutation is applied, the reducer scans `state.requests` and runs the **four-then-five token-key dispatch above against the materialized request** — that is, the request object as it exists after the patch — rather than against the raw line. The four full-pair shapes and the output-only fallback are unchanged; what changed is *what* the dispatch is applied to. A request emits a row the moment the dispatch returns `Some`, keyed by `requestId` so a future patch on the same request (e.g. an updated `timestamp`) does not double-emit.

Tail offset semantics: `tail_offsets.byte_offset` still records bytes consumed and the framework still hands the parser only the appended chunk, but the parser reads the full file from disk so the reducer can replay from byte 0 every tick. Cross-call re-emission is safe because the `(session_id, requestId)` deterministic UUID collides at the database upsert layer. For long sessions a per-session "last-processed line index" cache can be added later if cost matters; on a typical session file (~100 KB) full replay is sub-millisecond.

`MIN_API_VERSION` was bumped to `4`.

**Canonical fixture (R1.2, 8.4.1, [#669](https://github.com/siropkin/budi/issues/669)).** The shape described above is pinned to a real on-disk capture at `crates/budi-core/src/providers/copilot_chat/fixtures/vscode_chat_0_47_0.jsonl` (sanitized — prompt text, response markdown, code citations, file paths, and local-machine metadata stripped; envelope keys, `requestId`, timestamps, `agent.*`, `modelId`, `responseId`, `modelState`, and `completionTokens` / `promptTokens` patches preserved). A sibling `vscode_chat_0_47_0.expected.json` lists the per-request `(requestId, output_tokens, input_tokens, model)` tuples the reducer must materialize. A truncated companion `vscode_chat_0_47_0_streaming.jsonl` slices the fixture mid-stream — kind:2 stub written, kind:1 `completionTokens` patch not yet — and pins the no-emit-until-completion-token contract from R1.1. When extension N+1 changes the format again, the next bump captures a new fixture and the previous one is kept as a regression for the older format.

A side note from the same investigation: `result.metadata.resolvedModel` is **not** safe to use as a pricing key. On older sessions it was a dated version suffix (`claude-haiku-4-5-20251001`); on May-2026+ sessions it is an internal GPU-fleet code (`capi-noe-ptuc-h200-oswe-vscode-prime`) that does not map to manifest entries. The parser uses `modelId` (post-`copilot/` strip per §2.4) as the pricing key. When the value is the router placeholder `auto`, the §2.4.1 resolver maps it to a concrete model via `agent.id`; if no mapping exists the literal `"auto"` is preserved and pricing falls through to `unpriced:no_pricing` with the §3 Billing API reconciliation supplying the dollar truth.

Cache-token keys, when present, follow the same per-shape pattern under `cacheReadTokens` / `cacheWriteTokens` (delta and Feb-2026 shapes) or under `usage.cacheReadInputTokens` / `usage.cacheCreationInputTokens` (legacy). Cache tokens are best-effort — Copilot Chat does not expose cache fields on every record.

#### 2.4 Model-id key

- Top-level `modelId` — strip the `copilot/` prefix if present (e.g. `copilot/claude-sonnet-4-5` → `claude-sonnet-4-5`).
- `result.metadata.modelId` — Feb-2026+ shape.
- Fall back to a per-session default if a record carries tokens without a model id (e.g. interrupted sessions). Default is the per-session model recorded in the session manifest, if any; otherwise the model id is left empty and the row is tagged `unpriced:no_model` by the cost enricher (consistent with how `unpriced:no_tokens` is handled in ADR-0090 §2026-04-23).

#### 2.4.1 `auto` router resolution (R1.4, 8.4.1, [#671](https://github.com/siropkin/budi/issues/671))

When the user picks `auto` in the Copilot Chat model selector, GitHub picks the actual model server-side and persists the literal string `"auto"` as the request's `modelId`. The LiteLLM pricing manifest has no `auto` entry, so a literal `"auto"` model id falls through to `unpriced:no_pricing` and rows price at \$0 — the headline post-#R1.1 user-visible defect (#671) on the surface of `budi sessions` for any developer who leaves the model picker on the default.

The parser therefore resolves `"auto"` to a concrete model id via `agent.id` immediately after the §2.4 prefix-strip, before the row is handed to the pricing layer:

1. If `modelId != "auto"` — pass through as-is (current §2.4 behavior, unchanged).
2. Otherwise, look at `agent.id` on the same record and resolve via the table below.
3. If `agent.id` is missing or unrecognised, preserve the literal `"auto"` so the row still emits — pricing then falls through to `unpriced:no_pricing` and the §3 Billing API reconciliation worker trues the dollar number up to the real bill on the next tick (for individually-licensed users with a configured PAT).

| `agent.id` | Resolves to | Notes |
|---|---|---|
| `github.copilot.editsAgent` | `claude-sonnet-4-5` | Edit-mode chat. Copilot has routed to Claude Sonnet for code-edit-heavy turns since the GPT-5 / Sonnet 4.5 dual-default rollout in early 2026. |
| `github.copilot.codingAgent` | `claude-sonnet-4-5` | Newer agent-mode coding flow; same routing default as `editsAgent`. |
| `github.copilot.workspaceAgent` | `gpt-4.1` | `@workspace` chat. |
| `github.copilot.terminalAgent` | `gpt-4.1` | `@terminal` chat. |
| `github.copilot.default` | `gpt-4.1` | Plain chat panel (default participant). |
| `github.copilot.chat-default` | `gpt-4.1` | Older alias for the default participant. |
| `github.copilot` | `gpt-4.1` | Bare publisher id seen on a small fraction of older sessions. |

Three options were considered (per #671): **Option A** — resolve forward in the file via `agent.id`. **Option B** — accept the \$0 and lean on the §3 reconciliation worker. **Option C** — move the table into a `model_aliases` block on the LiteLLM manifest cache. Option A ships in 8.4.1 with the table living inline in `crates/budi-core/src/providers/copilot_chat.rs::resolve_auto_model_id`. Option B alone is unacceptable because for org-managed-license users the §3.4 path returns empty and a wrong guess would leave them at \$0 indefinitely; Option A guarantees a non-zero list-price-equivalent number for every user. Option C is the longer-term home — defer to 9.0.0 unless the inline table proves unreliable in practice.

GitHub does not contractually pin which model `auto` resolves to — it can shift between Copilot updates. The table is therefore the **current most-common default** at the time of the 8.4.1 patch, not a contract. When upstream rotates a default, the fix is the same as for §2.3 shape drift: amend this section, edit `resolve_auto_model_id`, cut both in the same PR. Wrong guesses only affect org-managed-license users (the §3 reconciliation trues up dollars for individually-licensed users on its next tick), so the cost of a stale entry is bounded.

Re-using the resolver: Continue / Cline / Roo Code are deferred to 9.0.0 (#295) and will hit the same `auto`-router shape. The `agent.id` → model mapping is per-provider — each provider plugin will ship its own table, but the resolution rule (§2.4.1) is the canonical pattern they implement.

#### 2.5 Pricing path

Tokens come from §2.3, the model id from §2.4, and dollarization runs through `pricing::lookup` from ADR-0091. Copilot Chat is not on a per-call dollar API at the local-tail layer — the dollar number is `tokens × manifest_price`. This is identical to the framing in ADR-0090 §2026-04-23: a list-price equivalent, not a Copilot bill. The Copilot bill itself is reconciled via §3 below for individually-licensed users, and is necessarily absent for org-managed-license users.

#### 2.6 Versioning rule (parser tolerance)

When the parser encounters a record whose token keys match none of the four shapes in §2.3, it logs **once per `(file_path, shape_signature)` per daemon run** at `warn` level with the message `copilot_chat_unknown_record_shape` and a redacted set of the top-level keys present, then skips the record. **Skipping a record never fails the file** — partial parses are valid, the next record may match, and a future Copilot Chat release that adds a fifth shape does not break ingestion silently.

When a fifth shape is observed in the wild, the fix is:

1. Update §2.3 of this ADR with the new shape.
2. Add the matching arm to the parser dispatch.
3. Bump the `copilot_chat` provider's `MIN_API_VERSION` constant (defined in `crates/budi-core/src/providers/copilot_chat.rs` as a monotonically-incrementing integer, mirroring the pattern in `budi-cursor`'s `MIN_API_VERSION`).
4. Cut the ADR amendment, the parser change, and the version bump in the same PR.

The `MIN_API_VERSION` bump is what makes ADR/code drift visible: a manifest version mismatch surfaces in `budi doctor` (R1.6, [#653](https://github.com/siropkin/budi/issues/653)) and is the signal that an upgrade requires an ADR review.

#### 2.7 Tailer placement and offset semantics

Live ingestion uses the existing reverse-proxy-first JSONL tailing infrastructure from ADR-0089. Each file's byte offset is persisted in the existing `tailer_offsets` table keyed by `(provider = "copilot_chat", path)`. JSON-document files (the non-JSONL `chatSessions/*.json` shape) are tailed by re-parsing on `mtime` change and tracking the last-seen `messages[]` length; this is more conservative than byte-offset tailing but matches how the document is rewritten in place by the extension. Sessions are correlated to messages by the file's parent-directory `<session-id>` segment for `chatSessions/` paths, by the JSON document's own `sessionId` field where present, and otherwise by a deterministic-uuid derived from `(file_path, message_index)` (consistent with the `copilot_cli` provider's `deterministic_uuid` shape at `crates/budi-core/src/providers/copilot.rs`).

### 3. GitHub Billing API reconciliation contract

#### 3.1 Endpoint (current, pre-2026-06-01)

```
GET https://api.github.com/users/{username}/settings/billing/premium_request/usage
```

Response shape (representative — only the fields Budi reads are pinned here):

```json
{
  "billing_cycle_start": "2026-05-01T00:00:00Z",
  "billing_cycle_end": "2026-05-31T23:59:59Z",
  "premium_request_usage": [
    {
      "date": "2026-05-04",
      "model": "gpt-4.1",
      "request_count": 142,
      "premium_requests_used": 35.5,
      "amount_in_cents": 875,
      "is_overage": false
    }
  ]
}
```

#### 3.2 Endpoint (post-2026-06-01 transition)

```
GET https://api.github.com/users/{username}/settings/billing/usage
```

GitHub's public roadmap has Premium Request Units (PRUs) replaced by **GitHub AI Credits** — a token-based unit — on 2026-06-01. The endpoint path drops the `/premium_request` segment; the response shape changes the per-row unit from `premium_requests_used` to `credits_used`, adds explicit `input_tokens` and `output_tokens` columns, and keeps `amount_in_cents` as the dollar truth.

The `sync_direct` worker probes the post-transition endpoint first; on `404` it falls back to the pre-transition endpoint. This makes the cutover seamless without a release gate. The Budi-side type is a `BillingUsageRow` enum with `Pru { premium_requests_used, .. }` and `Credit { credits_used, input_tokens, output_tokens, .. }` arms; the `cost_cents` column is populated from `amount_in_cents` either way.

#### 3.3 Authentication

- **Method**: GitHub Personal Access Token (PAT).
- **Required scope**: `manage_billing:copilot`. Fine-grained tokens require the equivalent **Plan: read-only** permission on the target user.
- **User opt-in only.** The PAT is stored in the daemon's existing keyring-backed secret store under the key `copilot_chat:billing_pat`. The user supplies it via `budi config set copilot_chat.billing_pat` or via the host extension's settings panel. **The daemon never auto-prompts for a PAT** and never falls back to the `gh` CLI's session token (that token does not carry `manage_billing:copilot` and the silent fallback would create a confusing auth-error surface). If no PAT is configured, the `sync_direct` worker is a no-op and `budi doctor` reports `copilot_chat: billing reconciliation unconfigured (local tail only)`.
- **Headers**:
  - `Authorization: Bearer <PAT>`
  - `Accept: application/vnd.github+json`
  - `X-GitHub-Api-Version: 2022-11-28` (current). When the post-transition endpoint requires a newer API-Version pin, the bump rides with the same PR that adds the post-transition arm.
  - `User-Agent: budi/<version>` (consistent with other Budi outbound calls).

#### 3.4 Org-managed-license caveat

For users whose Copilot license is org-billed (Copilot Business / Enterprise seat), the endpoint returns `200` with an empty `premium_request_usage: []` (or `credits_used: 0` in the post-transition shape). **This is the documented behavior, not a bug.** GitHub bills the org, not the user, and the per-user endpoint has no truth to surface.

The local-tail path from §2 still produces tokens × pricing-manifest dollars for these users — the dollar number is exactly the list-price-equivalent framing from ADR-0091, so a Copilot Business user gets a meaningful dashboard number, just not a Copilot bill. `budi doctor` flags org-managed users with `copilot_chat: org-managed license — billing reconciliation unavailable, local-tail tokens × manifest pricing in effect`. The ingest path tags rows from this state with `cost_confidence = "estimated"` (not `"exact"`), matching the existing taxonomy.

Detection of the org-managed state: the empty-but-200 response **persisted across two consecutive successful ticks within the same billing cycle** is treated as org-managed. A single empty response can be a "no usage yet this cycle" individual user; the second consecutive empty response inside an active cycle is the unambiguous signal.

#### 3.5 Reconciliation semantics (truth-up vs replace)

For individually-licensed users, the Billing API is the dollar truth. Local-tail rows for the same `(date, model)` are **truthed-up** but **not replaced**:

- `tokens` from local tail are preserved (Billing API doesn't always carry them in the pre-2026-06-01 shape).
- `cost_cents` is overwritten from `amount_in_cents` on a `(date, model)`-bucketed basis. Overwrites are tagged `pricing_source = "billing_api:copilot_chat"` to distinguish them from manifest-priced rows per ADR-0091 §4.
- `cost_confidence` is bumped from `"estimated"` to `"exact"`.
- The pre-existing `manifest:vNNN` `pricing_source` tag is shadowed by the `billing_api:copilot_chat` tag on the same row. This is a *single* exception to ADR-0091 Rule B (known→known with a new price is never a rewrite): the rewrite is from estimated-from-manifest to exact-from-vendor, which is a confidence increase, not a price change in the Rule B sense. The exception is logged for the audit trail.
- Bucketing granularity is `(date, model)` because that is the granularity the Billing API exposes. Per-message attribution is preserved on the local-tail row; the dollar correction is a per-bucket scaling factor applied uniformly to every message in the bucket.

### 4. Privacy surface

This ADR adds two new outbound destinations to Budi's surface:

- `https://api.github.com/users/{username}/settings/billing/...` — only for users who have explicitly configured a PAT under §3.3. The request carries the user's own GitHub PAT to GitHub's own API. No Budi-owned infrastructure sits between the client and GitHub.
- No new outbound for the local-tail path — files are read from the user's own machine.

ADR-0083 §Neutral is amended to add the `api.github.com` Billing API destination to the list of permitted outbound endpoints, gated on the user's opt-in PAT configuration. The trust class is the same as `gh api /users/.../billing` from any developer's terminal. Budi never logs the PAT, never includes it in diagnostic bundles (`budi support bundle` redacts the keyring path), and rotates it on every successful keyring read (no in-memory caching beyond the active request).

### 5. Caveats

These are the known failure modes and limits the parser and reconciliation worker must tolerate:

- **Undocumented local format.** Has shifted four times (§2.3); will shift again. The §2.6 versioning rule bounds the blast radius.
- **Session files rewritten in place.** Some Copilot Chat sub-versions rewrite the entire session JSON document on every message. The byte-offset tailer cannot be used for these files (§2.7); the document-path uses `mtime` + `messages[]` length tracking. This is more expensive than byte-offset tailing but bounded by the per-session message count.
- **Dual-publisher casing.** The publisher id flips between `GitHub` and `github` across releases (§2.2). Path matching is case-insensitive on the publisher segment.
- **PAT scope drift.** GitHub has renamed `manage_billing:copilot` once already (was `read:billing` pre-2024). The §3.3 scope name is current as of 2026-05; on a `403` with a clear scope-related body, `budi doctor` surfaces a remediation message that names the current required scope.
- **Org-managed empty response.** §3.4 — needs the two-consecutive-empty heuristic to disambiguate from "no usage yet this cycle."
- **Billing cycle alignment.** GitHub bills on user-account anniversary, not calendar month. The `billing_cycle_start` / `billing_cycle_end` fields drive the reconciliation window; Budi does not assume month boundaries.
- **2026-06-01 transition.** PRUs → AI Credits. §3.2 covers both shapes and the probe-then-fallback handles the cutover.
- **Rate limit.** GitHub's primary rate limit is per-PAT (5,000/h authenticated); the `sync_direct` cadence is one call per billing cycle per active day, well under the limit. `403` with `X-RateLimit-Remaining: 0` is logged once per process and the worker retries on the next tick.

### 6. What lives in code and what lives in this ADR

This ADR pins the **contract** (paths, key shapes, endpoints, auth, response shape, caveats). The **code** lives in:

- `crates/budi-core/src/providers/copilot_chat.rs` (R1.4) — local-tail parser, path discovery, and `Provider` impl.
- `crates/budi-core/src/sync/copilot_chat_billing.rs` (R1.5) — `sync_direct` reconciliation worker against the Billing API.

Both modules must reference this ADR at the top of the file. When upstream ships a breaking change in either surface, the fix is:

1. Update this ADR with the new shape (§2.3 for local, §3 for billing).
2. Update the relevant code path to match.
3. Bump `MIN_API_VERSION` (§2.6) for local-tail changes.
4. Cut the ADR edit, the code change, and the version bump in the same PR so the contract and the code never disagree.

## Consequences

### Positive

- **Both surfaces are pinned.** A future format shift (the fifth, or sixth, …) lands as a paired ADR-amendment + parser-change PR instead of a silent regression discovered via dashboard drift.
- **Org-managed users are first-class.** The empty-billing case has an explicit contract path; the dashboard number is meaningful (list-price equivalent via ADR-0091) instead of a confusing zero.
- **Truth-up without rewriting history.** Reconciliation upgrades `estimated → exact` confidence on `(date, model)` buckets; ADR-0091's Rule B is preserved with a single, audited carve-out for confidence increases.
- **Cutover is seamless.** The probe-first / fallback shape in §3.2 means the 2026-06-01 PRU→Credits transition does not require a Budi release boundary.
- **VS Code variants share one provider.** Code, Insiders, Exploration, VSCodium, Cursor, and remote-server installs all hit the same parser via §2.1 cross-product.

### Negative

- **One new opt-in outbound.** §4 — `api.github.com` joins the list of permitted destinations. Privacy class is the same as `gh api`; opt-in via PAT keeps it user-driven.
- **Document-rewrite tail mode is more expensive.** §2.7 — `mtime` + `messages[]` length tracking is unavoidable for the rewrite-in-place sub-versions; CPU bounded by per-session message count, which is small in practice.
- **Two-tick org-managed disambiguation has a one-tick lag.** §3.4 — the first empty response can't be classified; the second consecutive one can. In the worst case, a new individual user with no usage yet looks unconfigured for one tick. Acceptable.

### Neutral

- **Cloud sync shape unchanged.** Cloud receives `cost_cents` with no knowledge of `pricing_source = "billing_api:copilot_chat"` vs `manifest:vNNN`. Provider-scoped semantics from ADR-0083 are preserved.
- **`copilot_cli` provider unchanged.** That provider continues to tail `~/.copilot/session-state/` and is unrelated to the VS Code-family `copilot_chat` surface.
- **Statusline contract unchanged.** Copilot Chat plugs into the existing single-provider response shape; aggregation across Copilot Chat + Cursor + Continue happens via R1.3's `?provider=a,b,c` host-scoped path per ADR-0088 §7.

## Out of scope

- **A `copilot_chat` reverse-proxy live path.** The Copilot Chat extension does not route through a configurable HTTP proxy in a way Budi can intercept; the local-tail path is the live signal per ADR-0089.
- **Per-message dollar attribution from the Billing API.** The endpoint exposes `(date, model)` granularity; per-message dollar is recovered via §3.5 bucket-scaling, not as a separate Billing API call.
- **Continue, Cline, Roo Code, Aider, Windsurf providers.** Deferred to 9.0.0 per [#647](https://github.com/siropkin/budi/issues/647) "out of scope". Each gets its own ADR following the same pattern as this one.
- **JetBrains Copilot Chat coverage.** Out of 8.4 scope; revisited if/when budi-cursor grows a JetBrains sibling.
- **Org-billing-side reconciliation** (Copilot Business / Enterprise admin surfaces). That is an org admin's data, not the developer's, and lives outside the local-developer-first contract of ADR-0088.

## References

- [ADR-0083: Cloud Ingest Identity and Privacy Contract](./0083-cloud-ingest-identity-and-privacy-contract.md) (amended by §4 — Billing API destination added to permitted outbound list)
- [ADR-0088: 8.x Local-Developer-First Product Contract](./0088-8x-local-developer-first-product-contract.md) §7 (host-scoped vs. provider-scoped surfaces, amended in [#648](https://github.com/siropkin/budi/issues/648))
- [ADR-0089: JSONL Tailing as Sole Live Path](./0089-reverse-proxy-first-jsonl-tailing-as-sole-live-path.md)
- [ADR-0090: Cursor Usage API Contract](./0090-cursor-usage-api-contract.md) — direct precedent for this ADR's pattern (pin an undocumented contract, amend in lockstep with the parser)
- [ADR-0091: Model Pricing Manifest Source of Truth](./0091-model-pricing-manifest-source-of-truth.md) — `pricing::lookup` is what dollarizes locally-tailed Copilot Chat tokens (§2.5)
- [#647](https://github.com/siropkin/budi/issues/647) — 8.4.0 epic
- [#649](https://github.com/siropkin/budi/issues/649) — this ADR's tracking issue
- [#650](https://github.com/siropkin/budi/issues/650) — R1.3, multi-provider statusline endpoint that aggregates across `copilot_chat`
- [#651](https://github.com/siropkin/budi/issues/651) — R1.4, local-tail provider plugin (implements §2)
- [#652](https://github.com/siropkin/budi/issues/652) — R1.5, `sync_direct` Billing API reconciliation (implements §3)
- [#653](https://github.com/siropkin/budi/issues/653) — R1.6, `budi doctor` surfaces installed VS Code AI extensions and tailer health (consumes §2.6 `MIN_API_VERSION` + §3.4 org-managed signal + §3.3 unconfigured-PAT signal)
