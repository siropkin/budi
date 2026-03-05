# ADR: Evidence-First Pivot for budi

- Date: 2026-03-04
- Status: Proposed
- Owner: budi core

## Context

`budi` exists to be a local buddy for a global AI assistant: it should surface repo-grounded context only when useful, and avoid adding noise.

Recent holdout evaluation across four public repos (`react`, `flask`, `express`, `terraform`) with two repeats (24 judged cases) shows that current behavior is too often "faster but worse":

- bad-fast (API): 12/24 = 50.0%
- bad-fast among API-faster cases: 12/21 = 57.14%
- worst intent family: runtime-config prompts (bad-fast among API-faster cases: 87.5%)
- injected runs currently underperform skipped runs in quality consistency

Benchmark artifacts:

- `tmp/ab_holdout_r1/*/ab-results.json`
- `tmp/ab_holdout_r2/*/ab-results.json`
- `tmp/ab_holdout_bad_fast_summary.json`

## Decision

Pivot `budi` from "snippet injector" toward a strict "evidence router and verifier."

1. Default to abstain
   - If evidence quality is not clearly high, skip injection.
   - Prefer false-negative injection (skip) over false-positive injection (noisy context).

2. Evidence-card output contract
   - Inject small, structured evidence cards only:
     - exact path
     - symbol/function anchor
     - short proof excerpt
     - confidence and reason code
   - Avoid broad, free-form snippet dumps for sensitive intents.

3. Deterministic extraction for high-risk intents
   - Runtime-config prompts use deterministic detectors first (env reads, loaders, validators, schema hints).
   - Retrieval ranking is secondary to deterministic evidence when available.

4. Local SLM as control-plane critic only
   - Allowed roles: intent normalization, query rewrite, retrieval critique, inject/skip arbitration.
   - Not allowed role: writing final user answer.

5. Repository-agnostic heuristics only
   - No repo-name or project-specific anchors in retrieval logic.
   - Favor generic code-shape and path-family signals.

## Non-Goals

- Build a local model that replaces the global model for final answers.
- Optimize for speed alone.
- Maximize injection rate.

## Consequences

Expected:

- Lower bad-fast rate.
- Higher grounding consistency.
- More skipped injections in ambiguous prompts.

Trade-offs:

- Some queries become slightly slower because global AI does more tool calls.
- Lower raw "with_budi faster" percentage may be acceptable if quality stabilizes.

## Success Gates for this Pivot

Across holdout benchmark runs:

- bad-fast (API) <= 20%
- bad-fast among API-faster cases <= 30%
- average grounding delta >= 0.0 vs `no_budi`
- no regression in deterministic exact-path precision on runtime-config prompts

If gates are not met, escalate to stronger abstain policies and reduce injection surface further.

