# ADR-0090: Cursor Usage API Contract

- **Date**: 2026-04-21
- **Status**: Accepted
- **Issue**: [#365](https://github.com/siropkin/budi/issues/365) — promoted from `docs/research/cursor-usage-api.md` as part of the v8.3.0 `docs/` + `scripts/` audit
- **Milestone**: 8.3.0 (epic: [#436](https://github.com/siropkin/budi/issues/436))
- **Related**: [ADR-0089 §7](./0089-reverse-proxy-first-jsonl-tailing-as-sole-live-path.md) (Cursor Usage API lag verdict, [#321](https://github.com/siropkin/budi/issues/321)), [ADR-0083](./0083-cloud-ingest-identity-and-privacy-contract.md) §Neutral (outbound-network surface)
- **Supersedes**: `docs/research/cursor-usage-api.md` (2026-03-25 research note; content folded into this ADR)

## Context

Unlike Claude Code, Codex CLI, and Copilot CLI, the Cursor editor does not write a plain-text JSONL transcript of the conversation to disk at event time. Cursor persists composer state in `state.vscdb` (a SQLite database), and the only source of truth for **per-request tokens and cost** is Cursor's undocumented dashboard API at `cursor.com/api/dashboard/*`. Every other Cursor signal Budi observes (composer headers, transcript fragments under `~/.cursor/projects/*/agent-transcripts/`) is either derived, lagging, or missing the fields needed to price a row.

`crates/budi-core/src/providers/cursor.rs` reads `state.vscdb` to extract the auth token, then posts to `/api/dashboard/get-filtered-usage-events` to get the exact token and cost data for the current billing period. The code uses the API as-is; the shape of the call, the auth material, the headers, and the response format are all undocumented upstream contracts.

This ADR pins that undocumented contract as a durable project-level record. The previous research note (`docs/research/cursor-usage-api.md`) went stale silently when upstream changed; an ADR gets reviewed when the contract shifts. Promoting it also satisfies the "Promote to ADR" disposition in the v8.3.0 docs/scripts hygiene audit ([#365](https://github.com/siropkin/budi/issues/365)).

## Decision

The Cursor Usage API contract below is the authoritative Budi-side description of the surface. All Cursor-provider code (`crates/budi-core/src/providers/cursor.rs`) reads and writes to this surface; any change in upstream behavior is handled by updating this ADR in lockstep.

### 1. Endpoints

#### Filtered Usage Events (per-request, JSON)

```
POST https://cursor.com/api/dashboard/get-filtered-usage-events
```

Response shape:

```json
{
  "totalUsageEventsCount": 4980,
  "usageEventsDisplay": [
    {
      "timestamp": "1774455909363",
      "model": "composer-2-fast",
      "kind": "USAGE_EVENT_KIND_INCLUDED_IN_BUSINESS",
      "tokenUsage": {
        "inputTokens": 2958,
        "outputTokens": 1663,
        "cacheReadTokens": 48214,
        "totalCents": 1.68
      },
      "chargedCents": 0,
      "isChargeable": false,
      "isTokenBasedCall": false,
      "owningUser": "273223875",
      "owningTeam": "9890257"
    }
  ]
}
```

#### CSV Export (all events in billing period)

```
GET https://cursor.com/api/dashboard/export-usage-events-csv?strategy=tokens
```

Columns (in order): `Date`, `Kind`, `Model`, `Max Mode`, `Input (w/ Cache Write)`, `Input (w/o Cache Write)`, `Cache Read`, `Output Tokens`, `Total Tokens`, `Cost`.

#### Basic Usage (aggregate)

```
POST https://cursor.com/api/dashboard/get-current-period-usage
```

Returns: `{ billingCycleStart, billingCycleEnd, displayThreshold }`.

### 2. Authentication

The auth material lives in the same `state.vscdb` Budi already reads for Cursor composer data:

- **Table**: `ItemTable` (not `cursorDiskKV`).
- **Key**: `cursorAuth/accessToken`.
- **Value**: JWT.
- **User ID**: decode the JWT payload → `sub` field → split on `|` → second part.

Cookie format: `WorkosCursorSessionToken={userId}%3A%3A{JWT}`.

Required headers (CSRF protection — Cloudflare rejects otherwise):

- `Origin: https://cursor.com`
- `Referer: https://cursor.com/dashboard`
- Base URL: `https://cursor.com` (no `www` — the `www` host returns a 308 redirect).

### 3. Privacy surface

This is the only outbound HTTPS call the Cursor provider makes during historical import. The request carries the user's own auth token to Cursor's own servers — no Budi-owned infrastructure sits between the client and Cursor. The ingested rows carry `repo_id` hashes (not raw paths) per [ADR-0083 §6](./0083-cloud-ingest-identity-and-privacy-contract.md), and the tokens / cost fields are what Cursor already bills the user for. No new privacy obligations.

### 4. Caveats

These are the known failure modes and limits observed during the 2026-03-25 verification and re-verified when the 8.2 pivot shipped:

- **Undocumented API.** May change without notice. Budi treats any non-200 or JSON-shape mismatch as a recoverable error — it logs once at `warn` and falls back to the composer-header path.
- **Cloudflare challenge.** May block plain `curl`/`ureq` clients without a JS engine. Budi uses `reqwest` with the required `Origin`/`Referer` headers and observes no challenge from the daemon's User-Agent as of 2026-04-21.
- **JWT expiration.** Tokens expire, but Cursor auto-refreshes them in `state.vscdb`. Budi re-reads on every call rather than caching in memory.
- **No conversation_id in API events.** Event rows correlate to Cursor sessions by timestamp only. The provider matches on `|timestamp_ms - session_last_event_ms| < 60_000` to bucket events into sessions.
- **Current billing period only.** The API never returns historical periods. The CSV export likewise only covers the current period. Pre-current-period attribution comes from the composer-header path or is simply absent.
- **Event volume.** 4,980 events verified in a single heavy-use billing period (March 2026). The endpoint paginates at 1,000 events per call.
- **`kind` vocabulary.** Observed values: `USAGE_EVENT_KIND_INCLUDED_IN_BUSINESS`, `FREE_CREDIT`, `USAGE_BASED`. Anything else is treated as opaque and stored verbatim — the provider does not parse on `kind` beyond logging.

### 5. Lag characterization

The numeric verdict for Cursor Usage API lag ships as a [comment on #321](https://github.com/siropkin/budi/issues/321#issuecomment-4275063605) and is summarized in [ADR-0089 §7](./0089-reverse-proxy-first-jsonl-tailing-as-sole-live-path.md). In short: events appear on the endpoint within a few minutes of the wire call in the common case, with tail latency that can stretch to hours under load. The live-path contract in ADR-0089 accepts this lag as a Cursor-specific tax; Budi's statusline and vitals flag Cursor sessions with a `(Cursor: usage API lag)` footnote when the most-recent Cursor event is older than the most-recent transcript event.

The measurement instrument lives at `scripts/research/cursor_usage_api_lag.sh` per the [#407](https://github.com/siropkin/budi/issues/407) carve-out to the docs/research discipline rule. Operator-only measurement scripts that are load-bearing for an ADR may stay in `scripts/research/`; narrative output belongs in the wiki or a durable issue comment.

### 6. What lives in code and what lives in this ADR

This ADR pins the **contract** (endpoints, auth, response shape, caveats). The **code** lives in `crates/budi-core/src/providers/cursor.rs` and must reference this ADR at the top of the module. When upstream ships a breaking change, the fix is:

1. Update this ADR with the new shape.
2. Update `providers/cursor.rs` to match.
3. Cut the two changes in the same PR so the ADR and the code never disagree.

## Consequences

- **Contract surface is versioned.** Every change to the Cursor Usage API lands with a paired ADR edit, which forces a code review on both sides of the integration.
- **Research note is retired.** `docs/research/cursor-usage-api.md` is removed by the same PR that lands this ADR. Its content (2026-03-25 findings, 4,980-event verification, caveats list) is folded into §1, §2, §4, §5 above with the content otherwise unchanged. Historical commit trail preserved via git history.
- **No new dependencies.** This ADR documents existing code surface; no new crates, no new outbound calls, no new schema.
- **Cross-links.** ADR-0089 §7 continues to point at [#321](https://github.com/siropkin/budi/issues/321) for the lag verdict; this ADR is the Cursor-specific companion for the contract itself.

## Out of scope

- **Historical pre-billing-period attribution.** The API does not expose it; Budi recovers what it can from composer headers and accepts the rest as unattributed (surfaced as `(model not yet attributed)` per [#443](https://github.com/siropkin/budi/issues/443)).
- **Rewriting the `state.vscdb` schema read path.** That is a Cursor-provider implementation detail tracked separately.
- **Adding a second outbound endpoint.** Any new Cursor-API endpoint Budi reads in the future requires amending this ADR before landing.

## 2026-04-23 — `cursorDiskKV` bubbles become primary data source ([#553](https://github.com/siropkin/budi/issues/553))

Per-message pricing now reads `state.vscdb::cursorDiskKV` bubble rows directly. Per-request tokens and model live in that table under keys shaped `bubbleId:<uuid>`, with JSON values exposing `tokenCount.inputTokens`, `tokenCount.outputTokens`, `modelInfo.modelName`, `createdAt`, `conversationId`, and `type` — every field needed to price the row without any network call. This is the data source the v8.3.5 post-tag dogfood smoke was missing: the Usage API path from §1 only returns the user's billable overage events, so the whole subscription-included consumption (the bulk of real Cursor use) read as $0 in `budi stats`.

Implementation: `read_cursor_bubbles` in `crates/budi-core/src/providers/cursor.rs` opens the DB with `SQLITE_OPEN_READ_ONLY` and runs a single `json_extract`-powered SELECT against `cursorDiskKV`. `type = 1` rows map to `role = "user"` (zero tokens, `CostEnricher` tags them `unpriced:no_tokens` per #533); other rows map to `role = "assistant"`. When `modelInfo.modelName` is empty or the literal `"default"`, we rewrite to the `CURSOR_AUTO_MODEL_FALLBACK` constant (`claude-sonnet-4-5`), matching Cursor's public stance that Auto pricing tracks Sonnet. Deterministic row ids (`cursor:bubble:<conversationId>:<createdAt>:<inputTokens>:<outputTokens>`) dedup against Usage API events that describe the same activity.

The `/api/dashboard/get-filtered-usage-events` path from §1 stays operational as a supplementary overage-attribution signal; both paths run in the same sync tick and advance independent watermark keys (`cursor-bubbles` vs `cursor-api-usage`). A future train will supersede this ADR wholesale once the bubbles path has been validated on live data for one release cycle.

**Semantic note** — the resulting cost number is what the equivalent consumption would cost at direct-upstream rates (Anthropic / OpenAI). Cursor is a proxy with a flat subscription + overage, so this number is NOT a Cursor bill — it's the consumption value at list price, the same framing every other Budi provider surfaces. Cache-read savings read slightly low for Cursor because Cursor's backend-managed cache tiers are not exposed in the bubble schema.

**Schema risk** — Cursor owns `cursorDiskKV`'s shape and can change field names / types in any point release. Mitigations: every `json_extract` path sits in a single SQL query, `createdAt` is cast to TEXT to tolerate both ISO-8601 and epoch-ms shapes, and a missing `cursorDiskKV` table emits one `cursor_bubble_schema_unrecognized` warn per process and falls through to the Usage API path so the provider degrades gracefully.

**Supersede status** — not yet. The Usage API path, `extract_cursor_auth`, `CursorAuthIssue`, and the warn-once infrastructure all stay in place during the validation window. A later train removes them as a block once the bubbles path has been observed reliable on live data.

Reference implementation: [`getagentseal/codeburn`](https://github.com/getagentseal/codeburn/blob/main/src/providers/cursor.ts) does the same `cursorDiskKV` parse in TypeScript. The SQL query shape and the Auto → Sonnet fallback are direct adaptations; everything else is Budi-native.

---

*Last verified against code on 2026-05-14.*
