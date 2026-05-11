# ADR-0091: Model Pricing via Embedded Baseline + LiteLLM Runtime Refresh

- **Date**: 2026-04-21 (§2 amendment 2026-04-22 — 8.3.1 / [#483](https://github.com/siropkin/budi/issues/483); §5 amendment 2026-05-11 — 8.4.3 / [#725](https://github.com/siropkin/budi/issues/725))
- **Status**: Accepted (promoted 2026-04-21 after [#376](https://github.com/siropkin/budi/issues/376) and [#377](https://github.com/siropkin/budi/issues/377) merged with all Promotion Criteria test gates green; §2 amendment landed 2026-04-22 alongside v8.3.1 post-tag hardening; §5 amendment proposed 2026-05-11 alongside [ADR-0094](./0094-custom-team-pricing-and-effective-cost-recalculation.md))
- **Issue**: [#375](https://github.com/siropkin/budi/issues/375) (§2 amendment: [#483](https://github.com/siropkin/budi/issues/483); §5 amendment: [#725](https://github.com/siropkin/budi/issues/725))
- **Milestone**: 8.3.0 (epic: [#436](https://github.com/siropkin/budi/issues/436); §2 amendment: 8.3.1 / [#481](https://github.com/siropkin/budi/issues/481); §5 amendment: 8.4.3 / [#724](https://github.com/siropkin/budi/issues/724))
- **Amends**: [ADR-0083](./0083-cloud-ingest-identity-and-privacy-contract.md) §Neutral (outbound-network surface; see §6 below)
- **Amended by**: [ADR-0094](./0094-custom-team-pricing-and-effective-cost-recalculation.md) §9 — §5 immutability rules are rescoped to a new `cost_cents_ingested` column; a new Rule D defines explicit team-rate recompute on a sibling `cost_cents_effective` column.
- **Closes**: [#373](https://github.com/siropkin/budi/issues/373) — superseded by this ADR

> **Amended by [ADR-0094](./0094-custom-team-pricing-and-effective-cost-recalculation.md) (2026-05-11), §5.** History immutability for the column previously named `cost_cents` is preserved, rescoped to a new `cost_cents_ingested` column. Rules A, B, and C continue to govern `_ingested` without modification. A sibling `cost_cents_effective` column is added; it defaults to `_ingested` at insert time and is rewritten only when an org admin has uploaded a team price list via budi-cloud. The rejection of `budi pricing recompute` from §Alternatives E stands for the `_ingested` column; ADR-0094 introduces a scoped recompute on `_effective` only. See [ADR-0094](./0094-custom-team-pricing-and-effective-cost-recalculation.md) §9 for the full amendment text.

## Context

Budi's 8.1/8.2 architecture for model pricing is four hand-edited Rust functions that use substring matching against the lowercased model id to dispatch to one of a small number of hardcoded `ModelPricing` literals:

- `crates/budi-core/src/providers/claude_code.rs::claude_pricing_for_model`
- `crates/budi-core/src/providers/codex.rs::codex_pricing_for_model`
- `crates/budi-core/src/providers/cursor.rs::cursor_pricing_for_model`
- `crates/budi-core/src/providers/copilot.rs::copilot_pricing_for_model` — a delegator that routes to `claude_pricing_for_model` when the model id contains `claude` and to `codex_pricing_for_model` otherwise

`crates/budi-core/src/pipeline/enrichers.rs::CostEnricher` calls `provider::pricing_for_model(model, provider)` — defined in `crates/budi-core/src/provider.rs` — which fans out to one of the four functions above. The resulting `ModelPricing` is multiplied by token counts to produce `cost_cents`. The result is written to every assistant row with `cost_confidence = "estimated"`.

Two things are wrong with this architecture — one concrete bug, and one class of bugs it guarantees.

### The concrete bug

`claude_pricing_for_model` dispatches on `m.contains("opus-4-6") || m.contains("opus-4-5")` first, then falls through to `m.contains("opus")`. The `opus-4-7` family does not match the first arm (neither `opus-4-6` nor `opus-4-5` is a substring of `opus-4-7`), so it falls through to the legacy `opus` arm, which applies the `{ input: 15.0, output: 75.0, cache_write: 18.75, cache_read: 1.50 }` three-tier rate instead of the `{ input: 5.0, output: 25.0, cache_write: 6.25, cache_read: 0.50 }` flat-tier rate.

The result: every `claude-opus-4-7-*` row in `messages` is priced at roughly **3× the correct rate**. `cost_cents` is stored as if it were correct. `cost_confidence` is set to `"estimated"` — the same value a correctly priced row carries. There is no visible signal in the dashboard that the price is wrong. Users trust the number and make budget decisions against it.

### The class of bugs this architecture guarantees

The `opus-4-7` fallthrough is not a one-off missed update. It is the shape the architecture produces by design. Every time a vendor ships a minor model version whose id does not contain an already-enumerated substring, the dispatch falls through to the nearest-looking tier — which is almost never the right tier, because vendors routinely reprice major versions downward as the tier becomes the new default. The same shape risk exists in:

- `codex_pricing_for_model` for any post-`gpt-5.4` OpenAI minor release.
- `cursor_pricing_for_model` for any new Cursor composer or Gemini/Grok/DeepSeek/Llama variant that Cursor starts billing against.
- `copilot_pricing_for_model` for every model the Copilot CLI learns to route to after it ships.

The `unknown`-arm fallback in each function silently assigns a default price (Sonnet-class for Claude, GPT-4o-class for Codex, composer-2-class for Cursor). This is worse than visible failure: it guarantees that new models silently get a wrong number instead of a warn-and-zero that the user can act on.

### Why this is architectural, not editorial

Keeping the four tables in sync with every vendor announcement was the 8.0/8.1 plan, and it has not held. Budi is a two-person side project; Anthropic, OpenAI, Cursor, Google, DeepSeek, Grok, and Meta collectively ship new model variants faster than Budi ships releases. The only way to stop paying this tax is to stop hand-maintaining the pricing tables.

The community has already solved this. [BerriAI/litellm](https://github.com/BerriAI/litellm) maintains `model_prices_and_context_window.json`, a JSON map from model id to per-provider metadata including `input_cost_per_token`, `output_cost_per_token`, `cache_creation_input_token_cost`, `cache_read_input_token_cost`, `max_tokens`, `max_input_tokens`, `max_output_tokens`, `litellm_provider`, `mode`, and a few dozen other fields. It is MIT-licensed, ~100 distinct contributors touch it in a given quarter, and the file is updated on the order of weekly. Every major commercial AI gateway (LiteLLM, OpenRouter-adjacent tooling, a handful of observability vendors) already depends on it.

Adopting the LiteLLM manifest as Budi's pricing source of truth is the smallest change that fixes the current `opus-4-7` bug, ends the substring-fallthrough bug class, and gets Budi off the treadmill of tracking vendor price announcements.

## Decision

### 1. A single source of truth for pricing data

The LiteLLM manifest becomes Budi's pricing source of truth. Pricing is looked up via one call, `pricing::lookup(model_id, provider) -> PricingOutcome`, from exactly one call site (the existing `CostEnricher`). All other code paths call `pricing::lookup`, not the per-provider functions.

### 2. Three layers, in priority order

The `pricing::lookup` call resolves through three layers, in the order given:

1. **On-disk cache** — `~/.local/share/budi/pricing.json` on Linux/macOS, `%LOCALAPPDATA%\budi\pricing.json` on Windows (same platform conventions as the existing Budi data directory). Last successful runtime fetch from LiteLLM. If present and JSON-valid, it is authoritative.
2. **Embedded baseline** — a vendored snapshot of `model_prices_and_context_window.json` pulled at Budi build time, included via `include_str!`. Guarantees that Budi works on first run, fully offline, and on a box with outbound HTTPS blocked. The embedded snapshot is refreshed by hand once per Budi release as part of the release checklist (see §10 below).
3. **Hard-fail to `unknown`** — if a model id appears in transcripts but is not found in the disk cache or the embedded baseline, the lookup returns `PricingOutcome::Unknown { model_id }`. The row is stored with `cost_cents = 0`, `cost_confidence = "estimated_unknown_model"`, and `pricing_source = "unknown"`. A structured warn is logged once per `(provider, model_id)` per daemon run. The dashboard surfaces the unknown model in the "Unknown models seen" count in `budi pricing status` and as a warn icon next to any affected row or aggregate.

**There is no silent fallback to a per-provider default price.** The "unknown model → nearest-matching tier" behavior in all four functions is removed. An unknown model becomes a visible event the user can act on, not a silent wrong number.

### 3. Daemon-side refresh worker

A single worker inside `budi-daemon` is responsible for keeping the on-disk cache fresh.

- **Cadence**: once on daemon startup if the on-disk cache is absent or older than 24 h; once per 24 h thereafter while the daemon is running.
- **Upstream**: `https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json`. HTTPS only. No auth. No query parameters.
- **Transport**: the existing `reqwest` client already in the dependency tree (used for cloud sync). No new HTTP stack.
- **Validation**: before writing the fetched payload to disk, the worker asserts:
  - The body parses as a JSON object.
  - Every per-model entry with a price field has all price fields ≥ 0 and ≤ a sanity ceiling of $1,000 per million tokens (guards against a stray decimal-point upstream). **§2 amendment (2026-04-22, 8.3.1 / [#483](https://github.com/siropkin/budi/issues/483))**: row-level rejection, not whole-payload rejection. Rows failing the per-row sanity checks (NaN, negative, or over the $1,000 / M ceiling) are filtered out of the installed manifest and surfaced via `rejected_upstream_rows[]` on `GET /pricing/status` and on the text `budi pricing status` output. The rest of the payload still refreshes. Rationale: the pre-amendment shape hard-failed the whole tick on one bad upstream row (2026-04-22: `wandb/Qwen/Qwen3-Coder-480B-A35B-Instruct` at $100,000/M blocked every `v8.3.0` user's refresh), which defeats the daily-refresh guarantee. Ceiling is unchanged — still $1,000 / M, still the right guardrail; the amendment only changes the blast radius.
  - At least **95 %** of the models currently in the on-disk cache (or the embedded baseline if the cache is absent) are still present in the fetched payload (after per-row partitioning — §2 amendment). Guards against accidental upstream wipe, supply-chain tampering, or a mid-rewrite commit. A mass per-row-rejection upstream regression that pushed kept-row retention below 95 % still hard-fails the whole tick; the amendment does not weaken this fail-safe.
  - The payload is ≤ 10 MB (current file is ~400 KB; 25× headroom).
- **Write**: the validated payload is written atomically — write to a sibling temp file in the same directory, `fsync`, then `rename`. The in-memory lookup table is swapped under an `RwLock` on success.
- **Failure behavior**: any validation or network failure is logged at `warn` level and does not block ingestion. The previous cache (or embedded baseline) continues to serve lookups. The worker retries at the next scheduled interval. No exponential backoff beyond the 24 h cadence — the cost of a stale cache is bounded by the embedded baseline being correct at release time.
- **Version identifier**: each successful fetch is tagged with a monotonically incrementing `manifest_version` integer, persisted in a new `pricing_manifests` table keyed by version. The `pricing_source` string on a backfilled row is `backfilled:vNNN` where `NNN` is this integer. The source for new ingests is `manifest:vNNN`. This is what makes immutability auditable — every row's price can be traced to a specific manifest snapshot.

### 4. `pricing::lookup` API and schema contract

The public surface is exactly one function, plus a small enum:

```
pub enum PricingOutcome {
    Known { pricing: ModelPricing, source: PricingSource },
    Unknown { model_id: String },
}

pub enum PricingSource {
    Manifest { version: u32 },          // fresh ingest at cost time
    Backfill { version: u32 },          // unknown → known rewrite
    EmbeddedBaseline,                   // first-run / offline
    LegacyPreManifest,                  // historical rows only, never written by lookup
}

pub fn lookup(model_id: &str, provider: &str) -> PricingOutcome;
```

A new column `pricing_source TEXT NOT NULL DEFAULT 'unknown'` is added to the `messages` table in the schema migration that ships with the manifest loader (#376). Values are:

- `manifest:vNNN` — priced at ingest time against manifest version `NNN`.
- `backfilled:vNNN` — was originally `unknown`, rewritten at refresh time when manifest version `NNN` first included the model id.
- `embedded:vBUILD` — priced at ingest time against the embedded baseline, where `BUILD` is the Budi build git-SHA suffix or release tag. Appears only on first-run machines before the initial online refresh succeeds.
- `legacy:pre-manifest` — rows that existed before the migration ran. Tagged at migration time, never rewritten. Cost is whatever the 8.1/8.2 hand-maintained code produced — including the buggy Opus 4.7 rows.
- `unknown` — model id was not found in any layer at cost time. `cost_cents = 0`.

### 5. History immutability (the two backfill rules)

This is the section the rest of the design turns on. It is the reason this ADR is worth writing instead of just patching `claude_pricing_for_model`.

**Rule A — Unknown → known is a legal rewrite.**

When a manifest refresh resolves a `model_id` that was previously `unknown`, the refresher runs a single `UPDATE` inside the daemon:

```
UPDATE messages
SET cost_cents = :calc(tokens, new_pricing),
    cost_confidence = 'estimated',
    pricing_source = 'backfilled:vNNN'
WHERE pricing_source = 'unknown'
  AND model = :model
  AND provider = :provider;
```

Backfilling a previously-unknown row is filling in a blank, not rewriting a number — the user was shown `$0` plus a warn, and now they are shown a real cost. This is the only automatic write to historical rows that is ever performed. It is auditable via the `pricing_source` column.

**Rule B — Known → known with a new price is never a rewrite.**

If Anthropic drops Sonnet 20 % tomorrow and the next manifest refresh reflects that, rows already tagged `manifest:vNNN` are **not** retouched. The user was charged the old price at the time, no vendor refunds the difference, and Budi's history faithfully reflects what the user was actually charged. New ingests going forward are priced against the newer manifest version.

**Rule C — `legacy:pre-manifest` rows are never automatically touched.**

Pre-migration rows priced by the 8.1/8.2 hand-maintained code are tagged `legacy:pre-manifest` by the one-time migration (see §7) and are never automatically rewritten by any refresh. This means existing `claude-opus-4-7-*` rows stay priced at the buggy 3× rate until a user takes an explicit action. Since there is no user-facing `budi pricing recompute` command (§9), the effective behavior is: **history is immutable for known-at-the-time rows.** A one-time dashboard banner surfaces the migration date so the user can interpret the step change in `budi stats` output the first time they see it.

**Corollary — There is no `budi pricing recompute`.**

A subcommand that lets the user bulk-rewrite historical `cost_cents` is explicitly rejected. It is a foot-gun (accidental rewrite of every pre-migration row), it breaks the `cost_confidence` story (how would the rewritten row be labelled?), and it does not meaningfully help the user (no AI vendor issues backdated refunds). Per-user override for self-hosted or proxied models is a separate concern and is out of scope for 8.3.0 (§Out of scope).

### 6. Privacy posture (ADR-0083 §Neutral amendment)

The refresher adds exactly one new outbound network call to Budi's surface: an HTTPS `GET` against `https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json`. No user data leaves the machine — not in the URL, not in headers, not in a request body. The request is indistinguishable from `curl`'ing the public GitHub raw URL from any developer's terminal.

The trust class is the same as `cargo install` reaching `crates.io`, or `apt update` reaching a package mirror: fetching a public, versioned, community-maintained manifest over HTTPS. ADR-0083's privacy contract is unchanged with respect to user content; its outbound-network clause is amended to add the pricing manifest URL to the short list of permitted destinations (previously: the cloud ingest endpoint only).

Operator opt-out: `BUDI_PRICING_REFRESH=0` in the daemon's environment disables the refresher entirely. The embedded baseline becomes authoritative. The default is `on`.

### 7. Schema migration

A single schema migration runs once, at first daemon startup on the upgraded binary:

1. `ALTER TABLE messages ADD COLUMN pricing_source TEXT NOT NULL DEFAULT 'legacy:pre-manifest'`. Every existing row is now labelled.
2. Create the `pricing_manifests` table (`version INTEGER PRIMARY KEY, fetched_at TEXT, source TEXT, upstream_etag TEXT, known_model_count INTEGER`).
3. Record a synthetic row in `pricing_manifests` with `version = 0, source = 'pre-manifest'`. Anchors the `legacy:pre-manifest` label to a version number for future debugging.
4. Attempt an initial manifest fetch; on success, record it as `version = 1, source = 'network'`.
5. If the initial fetch fails (offline first run), load the embedded baseline, record it as `version = 1, source = 'embedded'`. The daemon will try the network again on the next refresh tick.

The migration is idempotent — `ADD COLUMN` with `NOT NULL DEFAULT` is a single SQLite statement; `pricing_manifests` has `IF NOT EXISTS`; the synthetic version-0 row is keyed by `version` and ignored if it already exists.

### 8. Operator surface: `budi pricing status`

One new CLI subcommand, no others:

```
$ budi pricing status
Pricing manifest
  Source           disk cache
  Manifest version 14
  Fetched at       2026-04-21T13:02:14Z (3h 12m ago)
  Next refresh     in ~20h 48m
  Known models     847
  Embedded baseline v8 (2026-04-21 release snapshot)

Unknown models seen in the last 7 days
  grok-5-mini-thinking              42 messages   claude_code
  gemini-4-pro-deep-reasoning        7 messages   cursor
  (2 more; run with --verbose)
```

`--json` emits the same data in machine-readable form for scripting. `--refresh` triggers an immediate refresh (subject to the same validation as the scheduled path) and then prints the resulting status. No other flags.

The dashboard surface is: a single small warn icon next to any row whose `pricing_source = 'unknown'`, and a single "Unknown models" count in the daily rollup. These exist so the user sees an unknown model has landed in their data without having to run a CLI.

### 9. What is removed

The four `*_pricing_for_model` functions and their test suites are deleted in [#377](https://github.com/siropkin/budi/issues/377), the cleanup ticket that runs after #376 lands. Their call sites are migrated to `pricing::lookup`. The dispatch shim at `crates/budi-core/src/provider.rs::pricing_for_model` is deleted. The `ModelPricing` struct itself is preserved as the output type of `pricing::lookup`. The pre-existing unit tests that assert numerical prices for specific model ids are reframed as assertions against the embedded baseline (so a regression in the baseline is a build-time failure) rather than as assertions against a hand-maintained Rust literal. This keeps the regression coverage but moves its source of truth to the manifest.

### 10. Embedded baseline refresh discipline

The embedded baseline is refreshed as a checklist item in every Budi release (starting with 8.3.0). The refresh is a single `curl` of the upstream URL, `git add`, commit with message `chore: refresh LiteLLM pricing baseline for v8.X.Y`. No hand-editing. No exceptions. A CI check asserts the embedded baseline parses, passes the same validation the runtime refresher runs, and has ≥ the model count of the previous baseline (missing models in a release baseline is a release-blocker bug, on the theory that upstream never deletes a well-known model id).

## Consequences

### Positive

- **The `opus-4-7` pricing bug is fixed at the root, not patched.** Every `claude-opus-4-7-*` row ingested after the manifest loader ships is priced at the correct flat-tier rate. The substring-fallthrough bug class is structurally impossible going forward because there is no substring matching.
- **Budi stops tracking vendor price announcements.** Releases no longer need a "did we miss a price change this cycle?" step. The embedded baseline refresh is a `curl` and a commit.
- **New models become visible, not silently mispriced.** A model id that isn't in the manifest shows up in `budi pricing status`, costs $0 with a warn icon, and gets backfilled automatically when the upstream catches up. The user sees the gap; they are not lied to.
- **History is honest.** Immutable `pricing_source` tagging means `budi stats` output is defensible: every cent is traceable to the manifest version in effect at ingest time. Users can reason about step changes in their dashboard instead of wondering if Budi silently rewrote yesterday's cost.
- **Offline-first is preserved.** The embedded baseline guarantees first-run correctness and survival on an airgapped box. `BUDI_PRICING_REFRESH=0` is a one-env-var opt-out for operators who want the embedded baseline as the only authority.
- **Code surface shrinks.** #377 deletes four sizeable functions and their substring-branching test suites. Net LOC decreases across #376+#377 taken together.

### Negative

- **One new dependency on an external community-maintained manifest.** LiteLLM is healthy (~100 contributors per quarter, weekly commits, MIT-licensed), but it is a third-party project. If upstream goes dormant or changes format, Budi has to respond. Validation (§3) bounds the blast radius of upstream misbehavior; the embedded baseline bounds the blast radius of upstream disappearance.
- **One new outbound network call.** §6 argues the trust class is `apt update`, not user data exfiltration. `BUDI_PRICING_REFRESH=0` is the operator opt-out. `cargo-deny`'s existing policy (see [#432](https://github.com/siropkin/budi/issues/432)) continues to govern the HTTP stack; no new crates are pulled in.
- **A visible one-time step change in the dashboard at the migration date.** Users whose history includes `claude-opus-4-7-*` rows will see the cost for those rows stay at the buggy 3× rate (because history is immutable) and see correctly-priced rows from the migration forward. Release notes lead on this. A one-time banner in the dashboard explains the step change on first view after upgrade.
- **An unknown-model row costs $0 until upstream catches up.** The alternative (silently assign a nearest-tier default) is exactly what this ADR rejects. $0-plus-warn is the honest answer; the backfill rule recovers the cost on next refresh.

### Neutral

- Privacy envelope is unchanged for user data. ADR-0083 is amended only to add the public pricing manifest URL to the list of permitted outbound destinations.
- The `CostEnricher` call graph is unchanged. `cost_cents` is still calculated inside the pipeline, still cached on the row, still overridden by API-provided exact costs (Cursor Usage API).
- Provider plugin model is unchanged. Adding a new agent is still one `Provider` impl (ADR-0089 §8). What goes away is the per-provider pricing table that new-agent work used to have to ship alongside the provider.
- Cloud sync is unchanged. Cloud consumes `cost_cents` from the same rollup columns regardless of how the row was priced; `pricing_source` is a local-only column and does not cross the cloud boundary.

## Alternatives Considered

### A. Continue hand-maintaining the four pricing tables (patch `opus-4-7`, carry on)

The zero-architecture-change option. Add an `opus-4-7` arm above the `opus-4-6` arm in `claude_pricing_for_model`, ship, move on. Rejected because it fixes this one bug and leaves the shape in place. The next minor-version ship (from any vendor) re-creates the same defect. Every release cycle has to budget for "did we miss a price change?", forever. We have already paid the cost of this architecture twice in 8.x; we should pay it a third time only if the alternative is clearly worse, and the alternative here is clearly better.

### B. Scrape vendor pricing pages directly

Considered. Rejected. Vendor pricing pages are HTML, change layout without notice, are rate-limited, differ in structure per vendor (Anthropic uses a hand-crafted table, OpenAI uses a tiered layout with footnote-style caveats, Cursor publishes a blog post), and would require Budi to ship and maintain four-plus bespoke scrapers. The community (LiteLLM) already does this work; re-doing it is pointless and strictly worse (per-release rot instead of per-week rot).

### C. Stand up a Budi-hosted pricing service

Considered. Rejected as a clean fit with no infrastructure. Running a service to redistribute a public JSON file would add an infrastructure dependency, an on-call surface, and an attack surface, in exchange for a round-trip that the client can do itself against `raw.githubusercontent.com` with equivalent availability. It conflicts directly with the single-binary, no-infra ethos of 8.x (ADR-0088 §2). If upstream LiteLLM ever becomes unreliable, a Budi-hosted mirror is a reasonable follow-up; it is not where 8.3.0 should start.

### D. Per-user override mechanism for self-hosted and proxied models

Considered. Deferred to a future ticket, not rejected outright. The argument is that a user running a self-hosted model (or a proxy that rebrands an underlying model under a new id) needs a way to teach Budi the right price. The counter-argument for now is that the shape of the override (a TOML file? a CLI subcommand? precedence over the manifest? cross-version stability?) is under-specified, and shipping it speculatively in 8.3.0 would create a surface we have to support before we know what users actually need. The 8.3.0 behavior for a self-hosted model id is the same as any unknown model — show $0, warn, surface in `budi pricing status`. A user who hits this surface and reports it is the right forcing function for designing the override.

### E. Recompute historical rows on price changes

Explicitly rejected under Rule B (§5). The user was charged the price at the time. Budi's job is to report what the user was charged, not to retroactively rewrite reality to match current prices. An hour spent specifying and testing a recompute path is an hour spent building a foot-gun. The closed-on-design decision stands: there is no `budi pricing recompute` command, and there will not be one.

### F. Keep the substring-dispatch shape but make the tables data

A middle ground: move the four hand-maintained tables out of Rust literals and into a TOML file under `crates/budi-core/`, while keeping the current dispatch. Rejected because it solves the "pricing lives in Rust, which is inconvenient to edit" problem but does not solve the "pricing lives in Budi, which cannot keep up with vendor releases" problem. The substring fallthrough is still there; the opus-4-7 bug still exists in a TOML-backed version of the same code; the per-release maintenance burden is unchanged. Data-in-TOML without a community source of truth is cosmetic.

## Promotion Criteria

This ADR is promoted from `Proposed` to `Accepted` only when all of the following are true, each demonstrated with a linkable artifact:

- [#376](https://github.com/siropkin/budi/issues/376) ships the manifest loader, `pricing::lookup` API, refresh worker, schema migration for `pricing_source`, auto-backfill path, `budi pricing status` CLI, and dashboard unknown-model surface. The PR must include: (a) unit tests proving `manifest:vNNN` and `legacy:pre-manifest` rows are never recomputed across a refresh, (b) a unit test proving `unknown → backfilled:vNNN` rewrites do happen on refresh, (c) a property test or parameterized test covering model-id UTF-8 boundary handling (no `split_at` on a non-char-boundary model id), (d) validation tests proving the <95%-of-known-models guard triggers on a wiped payload, and (e) an integration test proving `BUDI_PRICING_REFRESH=0` suppresses all network calls.
- [#377](https://github.com/siropkin/budi/issues/377) deletes the four `*_pricing_for_model` functions and their substring-branching tests; every call site routes through `pricing::lookup`; net LOC across #376+#377 is negative.
- Propagation in the same PR as #375 (this ADR): SOUL.md "Key files" narrative for `cost.rs` / `provider.rs` / `providers/*.rs` references ADR-0091; README.md pricing / cost-confidence sections reference ADR-0091; rustdoc on `pricing::lookup` cross-links this document.
- The embedded baseline refresh step is added to the release checklist ahead of the v8.3.0 tag (§10).

Until every bullet above is demonstrable, the status banner stays `Proposed`.

**Promotion evidence (2026-04-21):**

- [#376](https://github.com/siropkin/budi/issues/376) merged as [PR #461](https://github.com/siropkin/budi/pull/461). All nine Promotion-Criteria test gates green: `manifest:vNNN` never recomputed, `legacy:pre-manifest` never recomputed, `unknown → backfilled:vNNN` rewrites do happen on refresh, UTF-8 boundary safety on multi-byte model ids, <95%-retention guard rejects a wiped payload, $1,000/M sanity ceiling rejects a mispriced payload, `BUDI_PRICING_REFRESH=0` suppresses network calls, schema migration is idempotent, `budi pricing status --json` golden-key shape is stable.
- [#377](https://github.com/siropkin/budi/issues/377) merged as [PR #462](https://github.com/siropkin/budi/pull/462). Net diff **-750 LOC** across the workspace; the four `*_pricing_for_model` functions and their substring-dispatch tests are gone; every call site now routes through `pricing::lookup`.
- Propagation rode with the ADR merge in [PR #460](https://github.com/siropkin/budi/pull/460) — `SOUL.md` / `README.md` / `docs/adr/0083-cloud-ingest-identity-and-privacy-contract.md` updated in the same commit.
- `scripts/pricing/sync_baseline.sh` ships as the §10 refresh instrument. The v8.3.0 release ticket is committed to running it once as part of the pre-tag checklist per the script's own §10 prose.

## Out of scope

- Recompute of any historical rows, whether pre-migration buggy rows (`legacy:pre-manifest`) or post-migration rows whose model's price has changed upstream (`manifest:vNNN`). Rule C and Rule B of §5 govern; there is no `budi pricing recompute` command.
- A separate Budi-hosted pricing service. §Alternative C.
- Vendor-page scraping. §Alternative B.
- Per-user override for self-hosted or proxied models. §Alternative D, deferred to a follow-up.
- Any change to the cloud ingest shape. `pricing_source` is a local-only column; cloud rollups continue to carry `cost_cents` with no knowledge of the manifest version it came from.
- Any change to the `cost_confidence` taxonomy beyond the introduction of `estimated_unknown_model` as the value written for `pricing_source = 'unknown'` rows. The existing `exact` / `estimated` / `proxy_estimated` values are preserved.

## References

- [ADR-0081: Product Contract and Deprecation Policy](./0081-product-contract-and-deprecation-policy.md)
- [ADR-0083: Cloud Ingest Identity and Privacy Contract](./0083-cloud-ingest-identity-and-privacy-contract.md) (amended by this ADR, §Neutral)
- [ADR-0088: 8.x Local-Developer-First Product Contract](./0088-8x-local-developer-first-product-contract.md)
- [ADR-0089: JSONL Tailing as Sole Live Path](./0089-reverse-proxy-first-jsonl-tailing-as-sole-live-path.md)
- [#373](https://github.com/siropkin/budi/issues/373) — manual pricing refresh + Opus 4.7 fallthrough fix (superseded by this ADR)
- [#375](https://github.com/siropkin/budi/issues/375) — this ADR's tracking issue
- [#376](https://github.com/siropkin/budi/issues/376) — implementation: manifest loader, refresher, lookup API, backfill, `budi pricing status`
- [#377](https://github.com/siropkin/budi/issues/377) — cleanup: delete the four hardcoded `*_pricing_for_model` functions
- [#436](https://github.com/siropkin/budi/issues/436) — 8.3.0 epic
- [#443](https://github.com/siropkin/budi/issues/443) — model display-name normalization (carry-along of #376; see §10 of the implementation ticket)
- [BerriAI/litellm `model_prices_and_context_window.json`](https://github.com/BerriAI/litellm/blob/main/model_prices_and_context_window.json) — the upstream manifest
