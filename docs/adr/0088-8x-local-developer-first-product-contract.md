# ADR-0088: 8.x Local-Developer-First Product Contract

- **Date**: 2026-04-17
- **Status**: Accepted (amended — see banner)
- **Issue**: [#216](https://github.com/siropkin/budi/issues/216)
- **Milestone**: 8.1.0
- **Depends on**: [ADR-0081](./0081-product-contract-and-deprecation-policy.md), [ADR-0082](./0082-proxy-compatibility-matrix-and-gateway-contract.md), [ADR-0083](./0083-cloud-ingest-identity-and-privacy-contract.md), [ADR-0086](./0086-extraction-boundaries.md), [ADR-0087](./0087-cloud-infrastructure-and-deployment.md)

> **Amended by [ADR-0089](./0089-reverse-proxy-first-jsonl-tailing-as-sole-live-path.md) (2026-04-17).** §2's table row naming the proxy as "Sole live ingestion path (ADR-0082)" is replaced: in 8.2+, the tailer is the sole live ingestion path (ADR-0089); it filesystem-watches agent transcripts and there is no proxy. §5's language about "rule-based activity/ticket/branch/file/outcome signals inside the proxy + pipeline" is replaced with "inside the pipeline, over JSONL tailed from agent transcripts." The rest of this ADR — persona priority (§1), local vs cloud boundary (§2 remainder), round order (§3), statusline contract (§4), classification intent (§5 intent), cloud scope for 8.1 (§6), and deprecation policy (§7) — stands as written. The 8.1 surfaces referenced here all shipped against the proxy path because 8.1 predates the pivot; 8.2 R1/R2 execute the reversal.

## Context

ADR-0081 locked the 8.0 product contract — proxy-first ingestion, a clear split between `budi-core` / `budi-cursor` / `budi-cloud`, and the first stable release. ADR-0086 locked the repo extraction boundaries, and ADR-0083 / ADR-0087 locked cloud identity, privacy, and deployment.

Those decisions answered the question _what does 8.0 ship?_. They do not answer the question _who is 8.x for, round by round, and what kind of product are we actually trying to become?_.

Budi 8.0 exposed new failure modes that only show up once real enterprise developers use local Budi every day:

- Session visibility broke silently for long windows (#302).
- Live branch attribution collapsed into a single `(untagged)` bucket (#303).
- Ticket and activity were CLI-invisible despite being the dimensions enterprise developers reach for first (#304, #305).
- The statusline grew a handful of modes and options without an anchor around _what does a developer actually want to glance at every minute?_.
- Onboarding, `budi init`, and `budi doctor` worked, but only if you already knew the happy path. First-run success was not the default.

At the same time, the backlog started suggesting a much larger 8.1: broader AI coding tool coverage (Gemini CLI, Windsurf, Cline, Aider, Roo Code, Continue), budgets and limits, pipeline integrations, richer team / manager workflows, MCP server reintroduction, richer ticket metadata. Taking all of that on at once would re-create exactly the kind of "everything, slightly" release that 8.0 worked hard to avoid.

The 8.x split therefore needs its own product contract: **local Budi as the thing enterprise developers love using every day, first**. Cloud, Cursor, and broader agent coverage align with that story instead of competing with it. This ADR locks that contract and is the governing document for the rest of 8.1, with explicit deferrals into 8.2 (#294) and 9.0 (#159).

## Decision

### 1. Persona priority for 8.1

Locked in this order:

1. **Enterprise developers** — the primary persona. When there is a design trade-off, pick the path that makes a professional developer understand their AI usage locally, faster.
2. **Freelancers / solo users** — important, but secondary. They benefit from the same local-first product; they are not the group we optimize defaults around.
3. **Managers** — cloud visibility still matters, but manager-facing workflows (budgets, limits, pipeline, team rollups) are **not** 8.1 scope.

This priority applies to defaults, onboarding copy, default statusline layout, default CLI verbosity, classification surfaces, and docs tone.

### 2. Local vs cloud product boundary for 8.x

Budi 8.x is organized around one **local product** with optional **cloud visibility**, not two peer products:

| Surface | Owner repo | Role in 8.x |
|---------|------------|-------------|
| CLI (`budi`) | `siropkin/budi` | Primary developer surface. Daily-use commands (`stats`, `sessions`, `status`, `doctor`, `statusline`). |
| Daemon (`budi-daemon`) | `siropkin/budi` | Owns SQLite, serves analytics APIs, runs proxy, runs cloud sync worker. |
| Proxy | `siropkin/budi` | Sole live ingestion path (ADR-0082). |
| Statusline | `siropkin/budi` | Quiet, provider-scoped ambient signal (see §4). |
| Classification | `siropkin/budi` | Rule-based activity/ticket/branch/file/outcome signals inside the proxy + pipeline (see §5). |
| Cursor extension | `siropkin/budi-cursor` | Provider-scoped Cursor surface. Consumes the shared status contract from §4. |
| Cloud dashboard | `siropkin/budi-cloud` | Shared visibility + manager-facing view. Uses the **same** windows (`1d`/`7d`/`30d`) and classification vocabulary as local. Owns local→cloud linking UX. |

Design rules for 8.x:

- Cloud and extension surfaces in 8.1 **align with** the local story. They do not outgrow it, fork it, or introduce separate data contracts.
- Every user-visible surface change in 8.1 that lands locally (statusline visuals, CLI examples, dashboard windows, onboarding copy, classification narrative) must thread a follow-up into [#296](https://github.com/siropkin/budi/issues/296) before the release can promote.
- No new local LLM dependencies are introduced for the primary 8.1 classification path.
- No upload of prompts, code, or responses to the cloud in any round. ADR-0083 remains the privacy contract; nothing in 8.1 relaxes it.

### 3. Round order and gating for 8.1

The execution queue in [#201](https://github.com/siropkin/budi/issues/201) is the source of truth. This ADR locks two non-obvious properties of that queue:

**R1.0 is a hard gate**, not a nice-to-have. Richer signals in R1.4 (file attribution, #292) and R1.5 (tool / session outcomes, #293) assume that sessions, branches, tickets, and activities are _already_ correctly attributed and queryable on a real user's machine. Therefore:

- R1.0 (#302, #303, #304, #305) must be merged and demonstrably working on a real user's machine before R1.1..R1.7 work begins.
- R1.0 deliverables are: root-cause evidence where applicable, regression tests (prefer shell-driven end-to-end scripts in `scripts/e2e/` that fail when the fix is reverted), a `budi doctor` health check for the dimension where relevant, and documentation of the attribution contract in `SOUL.md`.

**Review passes are part of the round, not a separate queue.** After each round's implementation issues are merged, the next work is that round's code review pass, then its docs review pass. Blocking defects are fixed during the review pass; non-blocking follow-ups become new issues rather than growing the current round.

**Release gates (R4):**

- #230 (R4.1) is release readiness and release notes drafting.
- #297 (R4.2) is the smoke test plan and is a **hard release gate**. It must be closed with a full PASS record covering 8.0 regression tests, the R1.0 attribution bugs and gaps, and every new 8.1 surface (R1.4 files, R1.5 outcomes, simplified statusline, CLI normalization, onboarding, dashboard windows + linking flow, Cursor extension alignment).
- #202 (R4.3) tags and promotes. It must not tag v8.1.0 until R4.1 (#230), R4.2 (#297), and the public-site sync (#296) are all closed.

### 4. Simplified statusline and the shared provider-scoped status contract

The statusline is the first surface most developers see every minute, so it sets the product tone. In 8.1:

- **Default statusline is quiet, simple, stable, and provider-scoped.** The Claude Code statusline shows Claude Code usage only — no blended multi-provider totals in provider-scoped surfaces.
- **Core windows are `1d`, `7d`, and `30d`.** Other windows are allowed in advanced modes but are not the default.
- **Advanced variants and tweaks live in README or advanced-install docs**, not in the default path. If a knob is not needed for the default quiet daily-use experience, it should not be advertised in the default help output.
- **A single shared provider-scoped status schema / API contract is defined and shipped from the statusline work (#224).** This contract — the JSON shape emitted by `budi statusline --format json` and the matching daemon response — is consumed by:
  - the CLI statusline itself,
  - the Cursor extension (#232), scoped to Cursor usage,
  - the cloud dashboard (#235), scoped per-provider for provider-scoped views.
  Provider is an explicit filter in the contract rather than a family of ad-hoc per-surface shapes.

Stability of this contract is the point. Once 8.1 ships it, new agents in 8.2 (Gemini CLI, #161; and the rest, #295) slot into the same shape — they do not each invent their own statusline schema.

### 5. Classification and signal principles for 8.1

Classification should improve through **simple, explainable, local-first methods before any heavier ML direction is considered**.

**R1.0 — reliability of existing attribution (bugs and gaps):**

- R1.0.1 (#302), R1.0.2 (#303), R1.0.3 (#304), R1.0.4 (#305) are about making existing attribution trustworthy and queryable, not about inventing new signals.
- Prefer end-to-end regression tests in `scripts/e2e/` that fail when the fix is reverted.
- Expose a corresponding `budi doctor` check when the dimension has a realistic silent-failure mode.

**R1.1–R1.3 — classification contract and pipeline cleanup:**

- Prefer the simplest trustworthy path first: better deterministic heuristics, wider provider and ingest coverage, shared pipeline logic, and explicit `source` and `confidence` on classifications.
- Do not introduce new local LLM dependencies on the primary 8.1 classification path.
- Ticket attribution and branch classification ride on top of the attribution rules already documented in `SOUL.md` (the attribution contract §). They become first-class CLI and API dimensions, not best-effort side effects.
- The activity taxonomy stays explainable and maintainable. Richer ticket enrichment (Linear / Jira metadata, sprint / epic rollups) is 9.0 scope under #241 / #240.

**R1.4 — file-level attribution (#292):**

- Stays inside ADR-0083 privacy limits: **repo-relative paths only**, no absolute paths, no outside-of-repo paths, no file contents, no diffs.
- Signal is derived from tool-call arguments already visible to the proxy and pipeline.
- Writes are small, bounded, and deduplicated — file attribution must not inflate row size or row count unboundedly.
- A file attribution row without a valid repo-relative path is dropped, not stored with a placeholder.

**R1.5 — tool outcome and session→work outcome signals (#293):**

- Rule-based and debuggable. No remote git or PR API calls in 8.1. No content capture.
- Tool outcomes are derived from proxy-observable signals (denials, retries, follow-up success) and optional local git state the daemon can already see, not from external systems.
- Session→work outcome ("did this session produce a merged change, a reverted change, or no change?") is inferred from local git transitions only.
- Every outcome signal is labeled with an explicit `source` / `confidence` so surfaces can honestly say "this is a heuristic".

### 6. Local UX rules for R2

- **CLI normalization (#225):** Normalize subcommand names, flags, defaults, and help text for common-pattern developer expectations. Reduce surprising differences between related commands. Any breaking CLI change is called out in release notes (#230).
- **Onboarding (#228):** Scope is strictly local: install / init / doctor / first-run success. The cross-surface local→cloud linking UX is **not** part of #228 — it is owned by #235 in R3. This split is intentional: keep R2 about "does Budi feel good on one machine?" and keep R3 about "does Budi feel good across machine and cloud?".
- **Statusline (#224):** See §4. Default stays quiet, provider-scoped, `1d` / `7d` / `30d`.

### 7. Surface alignment rules for R3

- **Cloud dashboard (#235):** Adopts the same `1d` / `7d` / `30d` window contract as local. Owns the local→cloud linking flow end-to-end:
  - Discoverable linking after signup (not a multi-step hunt through settings).
  - Auto initial sync on successful link (a freshly linked account is never indistinguishable from a broken one).
  - Sync freshness indicator in the dashboard UI.
  - Empty-vs-stalled differentiation (initial sync in progress vs no data yet vs sync error).
- **Cursor extension (#232):** Consumes the shared provider-scoped status contract from §4. Cursor surfaces show Cursor usage only — they do not blend multi-provider totals.
- **Broader AI coding tool coverage is NOT 8.1 scope.** Do not add Gemini CLI, Windsurf, Cline, Aider, Roo Code, or Continue in 8.1. These are 8.2 scope under #294 (#161, #295).

### 8. Explicit deferrals

Locked as out-of-scope for 8.1:

| Deferred item | Target | Reason |
|---------------|--------|--------|
| Gemini CLI proxy support | 8.2 (#161 under #294) | 8.2 is the breadth release; 8.1 stays tight. |
| Other agents (Windsurf, Cline, Aider, Roo Code, Continue) | 8.2 (#295 under #294) | Same as above. |
| MCP server reintroduction | 9.0 (#162 under #159) | MCP is a 9.0 amplifier on top of the stable 8.x surfaces. |
| Budget engine, limits | 9.0 (#106, #107 under #159) | Not a developer-daily-use signal; needs the 8.1 classification foundation first. |
| Slack / team notifications | 9.0 (#164 under #159) | Team workflows belong to the 9.0 manager story. |
| GitHub Action / PR cost summaries | 9.0 (#163 under #159) | Pipeline integrations require a stable exported cost artifact first. |
| Stable CI/CD cost export | 9.0 (#238 under #159) | Tied to the pipeline integration path. |
| Linear / Jira ticket metadata enrichment | 9.0 (#241 under #159) | Richer ticket enrichment is a 9.0 workflow story, not 8.1 classification. |
| Ticket / sprint / epic rollups | 9.0 (#240 under #159) | Needs #241 first. |
| Persona-aware budget / limit policy surfaces | 9.0 (#237 under #159) | Depends on 9.0 budget engine. |

### 9. Public marketing site sync rule

Every 8.1 change that affects user-visible surfaces — statusline visuals, dashboard windows / screenshots, CLI examples, onboarding copy, classification narrative — can invalidate public content on https://github.com/siropkin/getbudi.dev.

Those updates are threaded into #296 as follow-ups regardless of which round they originate in. #296 must be merged in `siropkin/getbudi.dev` before the v8.1.0 promotion in #202.

### 10. ADR propagation rule (applies to every ADR ticket in this milestone)

An ADR ticket is **not done** when the ADR doc lands. It is done when every affected open ticket has been updated to reflect the ADR's decisions.

Before closing this ticket (#216), or any future ADR ticket in 8.1:

1. Audit open issues in `8.1.0` plus any downstream implications in `8.2.0`, `9.0.0`, and `getbudi.dev`. Identify tickets whose scope, acceptance criteria, deferrals, or approach are changed by this ADR.
2. Update each affected ticket: link back to this ADR, adjust scope / acceptance / non-goals / deferrals, and move milestone / labels if scope shifted between releases.
3. Refresh the parent epic body and agent prompt if the ADR changes round order, deferrals, or working rules.
4. Update `SOUL.md`, `README.md`, and the ADR index in `docs/adr/` where relevant.
5. Record the propagation audit as a comment on the ADR ticket: tickets touched with one-line reasons, tickets deliberately untouched with one-line reasons, and any #296 (getbudi.dev) impact.

A merged ADR PR with no downstream ticket updates is not considered complete.

## Consequences

### Expected

- R1.0 is sequenced before R1.1..R1.7 so richer signals land on top of trustworthy attribution, not on top of silent bugs.
- 8.1 stays tight around local developer daily use. Cloud and Cursor align with the local story instead of competing with it.
- A single provider-scoped status contract is defined once (in #224) and reused by Cursor and the cloud dashboard. New agents in 8.2 plug into it without reinventing per-surface shapes.
- Classification remains explainable and cheap. No new local model runs on the primary 8.1 path.
- Privacy stays inside ADR-0083 limits even as file and outcome signals are added.
- The 8.2 and 9.0 backlogs absorb the scope that does not belong to a developer-first 8.1, keeping 8.1 shippable.

### Trade-offs

- The Claude Code statusline deliberately does not show blended multi-provider totals in its default mode. Users who want cross-provider totals get them from `budi stats`, the cloud dashboard, or advanced statusline modes in docs — not from the quiet default.
- 8.1 does not add any new AI coding tool to the supported set. Users asking for Gemini CLI / Windsurf / Cline / Aider / Roo Code / Continue will see those in 8.2, not 8.1.
- Budgets, limits, pipeline integrations, Slack alerts, and richer ticket metadata slip to 9.0. Users who want those today will have to wait.
- Onboarding (#228) deliberately stops at first-run local success; the cross-surface linking polish lives in #235. This requires both tickets to ship before the onboarding story feels end-to-end.

### What this ADR does NOT decide

- Specific statusline JSON field names, CLI flag naming, or default help text — those are decided in their respective round tickets (#224, #225, #228) and must conform to the principles above.
- Internal data representation of file-level attribution and outcome signals — decided in #292 and #293, within the privacy rules above.
- Cloud dashboard visual design or copy — decided in #235 and surfaced through #296.
- Proxy protocol, cloud ingest identity, and repo extraction contracts — already governed by ADR-0082, ADR-0083, and ADR-0086 respectively. This ADR does not relax or override them.
