# ADR-0094: Custom Team Pricing and Effective-Cost Recalculation

- **Date**: 2026-05-11
- **Status**: Proposed
- **Issue**: [#725](https://github.com/siropkin/budi/issues/725)
- **Epic**: [#724](https://github.com/siropkin/budi/issues/724)
- **Milestone**: 8.4.3
- **Amends**: [ADR-0091](./0091-model-pricing-manifest-source-of-truth.md) §5 (history immutability is rescoped to a new `_ingested` column; a new Rule D defines effective-cost recompute on `_effective`); [ADR-0083](./0083-cloud-ingest-identity-and-privacy-contract.md) §Neutral (outbound-network surface gains one additional destination, `GET app.getbudi.dev/v1/pricing/active`)

## Context

ADR-0091 made the LiteLLM manifest the single source of truth for model pricing, ended substring-dispatch fallthrough, and locked history immutability: a row's `cost_cents` is whatever the manifest said at ingest time, and no subsequent manifest refresh ever rewrites it. That design is correct for **what a single user was told they spent**, which is what the local product reports.

It does not, however, answer **what an organisation actually paid**. The two diverge in three common ways:

- **Bedrock regional pricing.** Anthropic models on AWS Bedrock are billed at a different rate than the same model called against the Anthropic-direct API, and Bedrock charges a 10 % surcharge for regional (non-global) inference. The LiteLLM manifest does not — and cannot — encode the inference platform a given customer's traffic actually flowed through.
- **Enterprise contracts.** Organisations on a committed-spend tier with Anthropic, OpenAI, or Cursor routinely negotiate a discount off list price (the example pricing CSV that triggered this ADR shows ~3 % sale-vs-list on Bedrock global and ~3 % on Anthropic-direct, plus a 10 % regional uplift). Procurement teams maintain these rates in spreadsheets; ops teams ask "what's our actual run-rate?" and get the LiteLLM-published number, which is wrong by exactly the negotiated discount.
- **Sales price vs. list price.** Even when no committed contract is in place, vendors quote a "list" price on their pricing pages and a "sale" price in customer-specific invoices. The numbers in the dashboard need to match the invoice, not the marketing page.

The cloud dashboard exists precisely to answer organisation-level cost questions. Today it cannot, because every cost column in `daily_rollups` carries the LiteLLM-priced number the local daemon computed at ingest. There is no place to put the team's actual rate, no path to recompute, and no way to keep local and cloud showing the same dollar amount once a team rate is in effect.

This ADR adds custom team pricing as a first-class cloud feature and extends the local daemon to mirror the team rate so that **the same dollar amount renders on every surface** — local CLI, statusline, Cursor extension, JetBrains extension, and the cloud dashboard.

### Why this doesn't violate ADR-0091

ADR-0091's §5 immutability rules — "known → known is never a rewrite" (Rule B), "legacy:pre-manifest rows are never automatically touched" (Rule C), and the explicit rejection of a `budi pricing recompute` command (§Alternatives E) — were written to prevent silent foot-guns: the user is shown a number, makes a budget decision against it, and Budi never retroactively rewrites that number. **That spirit is preserved.** The number Budi computed at ingest is preserved unchanged in a new `cost_cents_ingested` column. The number every surface *displays* is a new `cost_cents_effective` column that defaults to `_ingested` and is rewritten only when the team has explicitly authored a price list. There is no automatic retroactive rewrite based on a vendor price change; the only thing that triggers a `_effective` rewrite is an admin uploading a list of rates the team is actually being invoiced at. The user is not lied to — they are shown the rate their employer pays.

## Decision

### 1. The dual-column pattern

Every cost column on every cost-bearing table is split in two:

```
cost_cents_ingested   -- what Budi calculated at ingest, via pricing::lookup (ADR-0091). NEVER overwritten after insert.
cost_cents_effective  -- what every CLI / statusline / extension / dashboard query reads. Defaults to _ingested; rewritten only by an explicit team-pricing recompute.
```

Tables affected:

- Local: `messages`, `message_rollups_hourly`, `message_rollups_daily`.
- Cloud: `daily_rollups`, `session_summaries`.

At insert time the pipeline writes the same number into both columns (`_effective := _ingested`). The cost-display read path everywhere reads `_effective`. A user with no team pricing in effect sees identical numbers to today, forever.

### 2. The accepted CSV format

The canonical input is the procurement-spreadsheet shape that motivated this ADR:

```
Platform,Model,Type,Region,List Price (USD/MTok/Month),Sale Price (USD/MTok/Month)
Bedrock,Claude Sonnet 4.5,Prompts,Regional (Non-global),$3.30,$3.20
Bedrock,Claude Sonnet 4.5,Outputs,Global,$15.00,$14.55
Anthropic,Claude Opus 4.6,Prompts,US,$5.50,$5.34
```

Normalisation rules:

- `Type`: `Prompts` → `input`, `Outputs` → `output`, `Cache Read` → `cache_read`, `Cache Write` → `cache_write`. If the column says `Prompts` or `Outputs` and no Cache rows exist, cache-token rates default to the LiteLLM-published rate for that model.
- `Region`: `Global` → `global`, `Regional (Non-global)` → `regional`, `US` → `us`. Other strings are accepted verbatim into the row and matched literally.
- `Platform`: `Bedrock`, `Anthropic`, `Vertex`, `Azure-OpenAI`, etc. — matched literally against the org default platform.
- `List Price` / `Sale Price`: `$` prefix is optional; the parser strips whitespace and the `$`. Unit is **dollars per million tokens** ($/MTok). Internally converted to dollars-per-token at lookup time.

Only the sale price is load-bearing for cost math. The list price is preserved alongside (and surfaced as the "savings vs published price" delta in the dashboard polish ticket #733) but is never used for cost computation.

### 3. Model alias resolution

A new cloud-maintained table `model_aliases(display_name PK, patterns TEXT[])` bridges procurement-friendly display names ("Claude Sonnet 4.5") to canonical model-id glob patterns ("claude-sonnet-4-5-*", "anthropic.claude-sonnet-4-5-*"). The table is seeded from the LiteLLM display-name normalisation work already done for ADR-0091 (#443) and is admin-extensible. CSV upload that hits an unmapped display name surfaces it in the preview screen but does not block the commit — the row lands in `org_price_list_rows` with the literal display string in `model_pattern`, and a follow-up alias-add operation lights it up.

### 4. Org defaults for missing dimensions

The local ingest envelope today does not carry `inference_platform` (Bedrock vs Anthropic-direct vs Vertex etc.) or `inference_region` (global vs US vs regional). Extending the envelope is a future-work item explicitly out of scope for this ADR (see §Out of scope). For v1, every org declares a default in `org_pricing_defaults` — "we run on Bedrock, US region" — and the recompute interprets every ingested row under that default unless the price-list row specifies a more specific platform/region.

This is the right v1 tradeoff: most orgs run their AI traffic through a single platform/region pair; the orgs that don't can wait for the envelope extension.

### 5. Cloud-side data model

```
org_pricing_defaults (org_id PK, default_platform, default_region, updated_by, updated_at)

org_price_lists (
  id PK, org_id, name, description, source_file_name,
  effective_from DATE NOT NULL,
  effective_to   DATE,                                  -- NULL = open-ended; set to the next list's effective_from on activation
  status TEXT CHECK in ('draft', 'active', 'archived'),
  uploaded_by, uploaded_at
)

org_price_list_rows (
  list_id FK, platform, model_pattern, region,
  token_type CHECK in ('input', 'output', 'cache_read', 'cache_write'),
  list_usd_per_mtok NUMERIC(10,4),
  sale_usd_per_mtok NUMERIC(10,4) NOT NULL,
  raw_row JSONB                                         -- original CSV row preserved for audit
)

model_aliases (display_name PK, patterns TEXT[])

recalculation_runs (
  id PK, org_id, started_at, finished_at, status,
  scope_from_date, scope_to_date,
  price_list_ids INTEGER[],
  rows_processed, rows_changed,
  before_total_cents, after_total_cents,
  triggered_by                                          -- user_id or 'cron'
)
```

All new tables have RLS scoped to `org_id = current_org()`. Writes to `org_price_lists` / `org_price_list_rows` / `org_pricing_defaults` require admin role.

### 6. Cloud → local pricing pull

The local daemon polls `GET app.getbudi.dev/v1/pricing/active?since_version=N` once per hour while running (configurable via `BUDI_TEAM_PRICING_REFRESH_SECS`; disabled when `BUDI_PRICING_REFRESH=0` — same env switch as the LiteLLM manifest refresher).

Response shape (200):

```json
{
  "org_id": "...",
  "list_version": 3,
  "effective_from": "2026-04-01",
  "effective_to": null,
  "defaults": { "platform": "bedrock", "region": "us" },
  "rows": [
    {
      "platform": "bedrock",
      "model_pattern": "claude-sonnet-4-5-*",
      "region": "global",
      "token_type": "input",
      "sale_usd_per_mtok": 2.91
    }
  ],
  "generated_at": "2026-05-11T18:14:02Z"
}
```

Privacy posture:

- The response contains the org's negotiated rate. Not a secret to org members; the same data is visible to any developer who can read the dashboard.
- No per-user data. No emails, no other developers' device info, no rollups, no token counts.
- Trust class: the same as the LiteLLM manifest fetch — a small JSON describing public-to-the-org pricing, fetched over HTTPS with the existing Bearer token.

Other status codes:

| Code | Daemon behaviour |
|------|------------------|
| 304  | List version unchanged. Noop. |
| 404  | No active price list for this org. Clear in-memory team-pricing state; rewrite `cost_cents_effective := cost_cents_ingested` if it had previously been overridden. |
| 401  | Auth failure. Log warn, stop polling pricing endpoint until next daemon restart. Mirrors cloud-sync auth-fail behaviour. |
| 5xx / network | Log warn at debug level. Retry next tick. No exponential backoff beyond the 1 h cadence. |

### 7. Recalculation semantics

The recompute is a single SQL UPDATE over each affected table:

```
UPDATE messages
SET cost_cents_effective =
      coalesce(
        team_rate(model, provider, region, token_type) * token_count / 1_000_000,
        cost_cents_ingested
      )
WHERE bucket_day BETWEEN :from_date AND :to_date
  [AND org_id = :org_id]   -- cloud-side only
```

Triggers:

- **Cloud, synchronous**: activating a draft price list calls the recompute over `[list.effective_from, today]`. Admin can also press a "Recompute from \<date>" button on Settings → Pricing to widen the window.
- **Cloud, async**: pg_cron at 03:00 UTC iterates orgs with an active list and recomputes yesterday's bucket. Orgs with no active list are skipped.
- **Local, automatic**: on `list_version` bump (detected by the hourly pricing poll), the daemon runs the full recompute over `messages` and lets the existing rollup triggers cascade to the derived tables. The recompute is fast (a single UPDATE; even pathological multi-million-row local DBs complete in seconds).
- **Local, manual**: `budi pricing recompute --force` for support cases.

Idempotency is guaranteed because the formula is a pure function of `(token_count, model, provider, region, list_version)`. Running twice in a row produces identical results and a second `recalculation_runs` row with `rows_changed = 0`.

### 8. Membership rule (v1)

Only members of the org that owns a price list receive that list. Strict org membership. The endpoint scopes responses to the caller's org from the Bearer token; cross-org access is structurally impossible.

Deferred: per-user opt-out ("I want to see published Anthropic prices even though my org has a list"), per-device-group pricing (contractors on a different rate basis). Neither is hard to add later; both add UX surface that should follow a user request rather than precede one.

### 9. Amendment to ADR-0091 §5

ADR-0091's §5 immutability rules are unchanged in spirit but are now scoped to the `_ingested` column. The amendment adds one rule:

> **Rule D (team-rate effective cost).** When a team price list is active for the org the daemon is signed into, `cost_cents_effective` is deterministically recomputed from `(token_count × team_rate_per_token)`. `cost_cents_ingested` is **never** touched by this path; ADR-0091's Rules A, B, and C continue to govern it without modification. The rejection of `budi pricing recompute` from ADR-0091 §Alternatives E stands for the `_ingested` column; this ADR introduces `budi pricing recompute --force` only for the `_effective` column, only when a team list is active, and only as a support escape hatch (the normal path is automatic on `list_version` bump).

### 10. Amendment to ADR-0083 §Neutral

The permitted outbound-network surface gains one destination:

> `GET https://app.getbudi.dev/v1/pricing/active` with `Authorization: Bearer budi_<key>` — same token, same TLS posture, same operator opt-out (`BUDI_PRICING_REFRESH=0`) as the existing ingest call and the LiteLLM manifest refresh. Response body contains the calling org's negotiated rates; no per-user data crosses the boundary. The user-data privacy contract in §1 of ADR-0083 is unchanged.

## Consequences

### Positive

- **Cost matches everywhere by construction.** Local CLI, statusline, Cursor extension, JetBrains extension, and cloud dashboard read the same `_effective` column, populated by the same algorithm. The "my manager sees $487, my statusline says $612" support burden is structurally eliminated.
- **History is still honest.** `cost_cents_ingested` preserves the ADR-0091 contract: the number Budi computed at ingest is auditable forever. Any disagreement between `_ingested` and `_effective` is explained by an `org_price_lists` row and a `recalculation_runs` audit entry.
- **The savings story is free.** Because we keep both columns, the dashboard can show "Saved \$1,287 this month at negotiated rates" without any extra computation.
- **No new privacy boundary.** The cloud-pull endpoint carries no user content; the ingest envelope is unchanged.
- **No-cloud users see no change.** Without an active cloud price list, `_effective = _ingested` forever. The local product is unmodified for solo developers.
- **Rollback is cheap.** Archiving a price list triggers a recompute that restores `_effective := _ingested`. There is no irreversible state change.

### Negative

- **Schema migration touches every cost-bearing table.** Local migration is straightforward (`ADD COLUMN _ingested`, one-time `UPDATE`, rename `cost_cents` → `cost_cents_effective`). Cloud migration is the same shape against Supabase. Both are idempotent and routine, but they do touch hot tables. Mitigated by deploying out of business hours and by the dual-column writes being additive (no data is lost; old reads can be served from a view during cutover if needed).
- **One new outbound call from the daemon.** Argued at §6 above as the same trust class as the existing ingest call and the LiteLLM manifest refresh. `BUDI_PRICING_REFRESH=0` is the single operator opt-out for all three.
- **Org admins now have a foot-gun.** Activating a buggy price list (typo in a rate, wrong region) results in incorrect numbers on the dashboard until corrected. Mitigated by the draft → preview → activate flow (#727), by the audit history (#733), and by rollback being a single archive-and-recompute step.
- **Two-column reads everywhere.** Every cost-reading query in `analytics/mod.rs`, every cloud aggregate query, every rollup trigger must be touched. This is mechanical, well-bounded work but it is not small.

### Neutral

- The LiteLLM manifest refresh path (ADR-0091 §3) is unchanged.
- The `CostEnricher` is unchanged at the API level — it now writes the same number into two columns instead of one. The pipeline order is unchanged.
- Cloud sync envelope grows by one field per rollup record (`cost_cents_ingested`); the rest of ADR-0083's data shape is unchanged.
- Provider plugin model is unchanged. Adding a new agent still ships a single `Provider` impl with no per-provider pricing table.

## Alternatives Considered

### A. Cloud-only recompute; local stays LiteLLM-priced

The minimal version: only the cloud dashboard reflects team pricing; local CLI / statusline / extensions continue to show LiteLLM list prices. **Rejected.** The recurring user complaint that motivated this ADR was specifically that local and cloud disagree. Shipping a feature that hard-codes the disagreement defeats the point. The implementation cost of mirroring locally is bounded (one schema migration, one worker, one CLI surface update) and pays for itself the first time a user opens the dashboard alongside their statusline and the numbers match.

### B. Cloud pushes price lists to local via the existing cloud-sync channel (server-initiated)

Considered. **Rejected for v1.** The existing cloud-sync transport is one-way local → cloud (`POST /v1/ingest`). Adding a server-push channel (WebSocket, SSE, or polling-disguised-as-push) is materially more infrastructure than is justified by an artefact that changes once per quarter at most. Polling at 1 h cadence is well within the freshness budget for pricing changes — no procurement team needs sub-hour propagation — and reuses the same Bearer auth, same TLS posture, and same opt-out env var as the ingest call.

### C. Compute `_effective` lazily at read time instead of materialising it

Considered. **Rejected.** Lazy computation means every dashboard query joins against the active price list and runs the per-row rate lookup inline. Performance is acceptable on small data but degrades sharply as `daily_rollups` grows; more importantly, it loses the audit trail (no `recalculation_runs` row to point at when someone asks "when did this number change and why?"). Materialising `_effective` is a one-line UPDATE that runs in seconds, costs almost nothing in storage, and gives the audit story for free.

### D. Single column with a side table that records the override

Considered (replace `cost_cents` in place; keep an `pricing_overrides` table that records the original value before each rewrite). **Rejected.** Same audit story as the dual-column design, but every read needs to know whether to display the column or the override, and the override semantics get complicated fast for partial-window recomputes. The dual-column design is strictly simpler at the cost of a few bytes per row.

### E. Extend the ingest envelope with `inference_platform` and `inference_region` now

Considered. **Deferred.** The cleanest design encodes platform and region on every ingested row so price-list resolution doesn't need to fall back to org defaults. But it requires every provider (Claude Code, Codex, Copilot CLI, Copilot Chat, Cursor) to detect and emit these fields, which is a non-trivial cross-provider plumbing exercise. The org-default v1 covers the realistic majority of orgs (single-platform shops) and is the right place to start. A follow-up ADR can promote platform/region to first-class envelope fields once the v1 surface is in use and we have a forcing function.

### F. Make custom pricing local-only, with a TOML file under `~/.config/budi/`

Considered as the per-user override Alternative D in ADR-0091. **Rejected for the team-pricing case.** A local TOML file scales to one user; team pricing is, by definition, shared across an org. The cloud is where the org abstraction lives. Local team pricing without a cloud authoring surface re-creates the problem we already have with the LiteLLM manifest — every developer hand-maintains a config and they drift. ADR-0091's Alternative D (per-user self-hosted-model override) remains separately deferred; it is not the same feature as team pricing.

## Out of scope

- Extending the ingest envelope with `inference_platform` / `inference_region` (Alternative E). Captured as future work.
- Tiered or committed pricing (volume discounts, monthly commits, true-up at end of period). The CSV input model assumes flat rates per `(platform, model, type, region)` tuple. Tiered models are a future ADR.
- Currencies other than USD. The CSV format and storage are USD-only.
- Per-user opt-out and per-device-group pricing (deferred per §8).
- Editing recalc runs. `recalculation_runs` is an immutable audit log.
- Backporting team pricing to `legacy:pre-manifest` rows. `_effective` for those rows is whatever `_ingested` is (i.e. the 8.1/8.2 hand-maintained code's number, including the Opus 4.7 fallthrough bug). A user who wants their pre-manifest history repriced should run `budi db import --force` after this lands, which will re-ingest those rows through the modern pipeline.

## Promotion Criteria

This ADR is promoted from `Proposed` to `Accepted` only when all of the following are demonstrable:

- [#726](https://github.com/siropkin/budi/issues/726) ships the cloud schema migration. Existing dashboard pages render unchanged numbers after migration (because `_effective := _ingested`).
- [#727](https://github.com/siropkin/budi/issues/727) ships the Settings → Pricing UI. Admin can upload the example CSV from this ADR and see 25/25 rows mapped.
- [#728](https://github.com/siropkin/budi/issues/728) ships the recalculation engine. Activating a list triggers a synchronous recompute; `recalculation_runs.rows_changed > 0`.
- [#729](https://github.com/siropkin/budi/issues/729) ships the `GET /v1/pricing/active` endpoint. Cross-org access is structurally blocked (RLS + token scoping verified).
- [#730](https://github.com/siropkin/budi/issues/730) ships the local schema migration. `budi db check` reports no drift; all analytics tests pass.
- [#731](https://github.com/siropkin/budi/issues/731) ships the local team-pricing worker. Cost numbers from `budi stats -p 7d` match the cloud dashboard's 7-day window to the cent.
- [#732](https://github.com/siropkin/budi/issues/732) ships the `budi pricing status` extension showing the team-pricing layer.
- This ADR's propagation rides with the merge: `docs/adr/0091-…md` §5 carries the Rule D amendment banner; `docs/adr/0083-…md` §Neutral grows by one URL; `docs/adr/README.md` index lists ADR-0094; `SOUL.md` references ADR-0094 in the Cost sources / Key files narrative where it references ADR-0091.

Until every bullet above is demonstrable, the status stays `Proposed`. [#733](https://github.com/siropkin/budi/issues/733) (dashboard polish) is not on the promotion gate — it is value-add that follows the contract being correct.

## References

- [ADR-0083: Cloud Ingest, Identity, and Privacy Contract](./0083-cloud-ingest-identity-and-privacy-contract.md) (amended by this ADR, §Neutral)
- [ADR-0086: Extraction Boundaries for budi-cursor and budi-cloud](./0086-extraction-boundaries.md)
- [ADR-0088: 8.x Local-Developer-First Product Contract](./0088-8x-local-developer-first-product-contract.md)
- [ADR-0091: Model Pricing via Embedded Baseline + LiteLLM Runtime Refresh](./0091-model-pricing-manifest-source-of-truth.md) (amended by this ADR, §5 Rule D)
- [#724](https://github.com/siropkin/budi/issues/724) — epic
- [#725](https://github.com/siropkin/budi/issues/725) — this ADR
- [#726](https://github.com/siropkin/budi/issues/726) – [#733](https://github.com/siropkin/budi/issues/733) — implementation tickets

---

*Last verified against code on 2026-05-14.*
