# Pivot Experiment Matrix (Evidence-First budi)

## Goal

Validate a pivot from broad context injection to strict evidence routing, with global quality as the primary KPI.

Primary KPI:

- `bad_fast_api_rate` = cases where `with_budi` is faster on API time but judged worse

## Baseline Snapshot (2026-03-04 holdout)

- Holdout: `react`, `flask`, `express`, `terraform`
- Runs: 2 repeats x 3 prompts = 24 judged cases
- `bad_fast_api_rate_all`: 50.0%
- `bad_fast_api_rate_when_faster`: 57.14%
- Runtime-config prompt family is dominant failure mode

## Variants to Evaluate

| Variant | Description | Expected Effect |
|---|---|---|
| V0 | `no_budi` baseline | Quality reference |
| V1 | Current `with_budi` | Current behavior reference |
| V2 | Evidence-only injection (deterministic cards, strict brevity) | Reduce anchoring on noisy snippets |
| V3 | V2 + SLM critic (inject/skip arbitration only) | Better abstain decisions |
| V4 | V2 + stronger abstain thresholds | Max quality safety |

Notes:

- SLM in V3 is not allowed to generate final answers.
- It may classify intent, critique retrieved evidence, and vote inject/skip.

## Dataset Plan

Use a split that prevents overfitting to known repos:

- Core holdout repos: `react`, `flask`, `express`, `terraform`
- Add additional unseen repos before final decision (target +4)
- Prompt families per repo:
  - architecture mapping
  - runtime config/env loading
  - flow tracing
  - symbol/path lookup
  - docs-to-code mapping

Run each variant with at least:

- 3 repeats per prompt family per repo
- same prompt fingerprint across variants

## Metrics

Quality-first:

- `bad_fast_api_rate_all`
- `bad_fast_api_rate_when_faster`
- judge winner share (`with_budi`, `no_budi`, ties)
- grounding delta

Secondary:

- API duration delta
- wall duration delta
- cost delta
- injection rate
- skip reason distribution

## Success Criteria

Minimum gates to proceed with implementation rollout:

- `bad_fast_api_rate_all <= 20%`
- `bad_fast_api_rate_when_faster <= 30%`
- grounding delta >= 0.0
- runtime-config family no worse than `no_budi` on winner share

Stretch:

- maintain >= 10% API-time gain with non-negative quality delta

## Decision Rules

- If V2 meets gates: adopt evidence-only design as default.
- If V2 misses but V3 meets gates: add SLM critic in control plane.
- If V3 also misses: increase abstain policy (V4) and narrow injection scope.
- If all fail: pause injection for high-risk intents and keep budi as repo search/status tool only.

## Execution Checklist

1. Freeze prompt set and fingerprint.
2. Run all variants on same repo set.
3. Aggregate metrics in one report JSON and markdown summary.
4. Review per-intent failures, not just global averages.
5. Decide go/no-go with gates above.

## Run the Matrix

1. Copy and edit the matrix template:

```bash
cp ./fixtures/benchmarks/pivot_matrix_v1.template.json ./tmp/pivot_matrix_v1.local.json
# edit repo_root paths in ./tmp/pivot_matrix_v1.local.json
```

2. Dry-run to verify commands:

```bash
python3 scripts/pivot_matrix_runner.py \
  --matrix-file ./tmp/pivot_matrix_v1.local.json \
  --prompts-file ./fixtures/benchmarks/public_v2.json \
  --repeats 3 \
  --out-dir ./tmp/pivot_matrix_v1_run \
  --dry-run
```

3. Execute for real:

```bash
python3 scripts/pivot_matrix_runner.py \
  --matrix-file ./tmp/pivot_matrix_v1.local.json \
  --prompts-file ./fixtures/benchmarks/public_v2.json \
  --repeats 3 \
  --out-dir ./tmp/pivot_matrix_v1_run
```

Outputs:

- `tmp/pivot_matrix_v1_run/pivot-matrix-summary.json`
- `tmp/pivot_matrix_v1_run/pivot-matrix-summary.md`
- `tmp/pivot_matrix_v1_run/pivot-matrix-run-records.json`

## Report Template

| Variant | bad_fast_all | bad_fast_when_faster | grounding_delta | API_delta | wall_delta | cost_delta | gate_pass |
|---|---:|---:|---:|---:|---:|---:|---|
| V1 |  |  |  |  |  |  |  |
| V2 |  |  |  |  |  |  |  |
| V3 |  |  |  |  |  |  |  |
| V4 |  |  |  |  |  |  |  |
