# Benchmark Methodology

This page explains how to benchmark `budi` in a way that is reproducible and transparent for regular Claude Code users.

## What we measure

For each prompt, we run two modes:

- `no_budi`: Claude with hooks disabled
- `with_budi`: Claude with hooks enabled

We compare:

- API duration (`duration_ms`)
- End-to-end wall duration
- Total cost (`total_cost_usd`)
- Judge winner + quality + grounding + actionability
- Hook injection health (`success`, `reason`, `context_chars`)

## Public benchmark dataset

- Prompt set: `fixtures/benchmarks/public_v2.json`
- Public benchmark details: `docs/benchmark-details.md`
- Public benchmark output artifacts:
  - `tmp/public_bench_react_v2b/ab-results.json`
  - `tmp/public_bench_flask_v2b/ab-results.json`
  - `tmp/public_bench_express_v2b/ab-results.json`

## Reproduce exactly

Clone repos:

```bash
git clone --depth 1 https://github.com/facebook/react.git ./react
git clone --depth 1 https://github.com/pallets/flask.git ./flask
git clone --depth 1 https://github.com/expressjs/express.git ./express
```

Run A/B script on each:

```bash
python3 scripts/ab_benchmark_runner.py \
  --repo-root "/absolute/path/react" \
  --prompts-file "./fixtures/benchmarks/public_v2.json" \
  --out-dir "./tmp/public_bench_react_v2b" \
  --run-label "public-bench-react-v2b"

python3 scripts/ab_benchmark_runner.py \
  --repo-root "/absolute/path/flask" \
  --prompts-file "./fixtures/benchmarks/public_v2.json" \
  --out-dir "./tmp/public_bench_flask_v2b" \
  --run-label "public-bench-flask-v2b"

python3 scripts/ab_benchmark_runner.py \
  --repo-root "/absolute/path/express" \
  --prompts-file "./fixtures/benchmarks/public_v2.json" \
  --out-dir "./tmp/public_bench_express_v2b" \
  --run-label "public-bench-express-v2b"
```

Generate the human-readable report with full evidence:

```bash
python3 scripts/generate_public_benchmark_details.py
```

## Notes about validity

- A row is considered an injected `with_budi` run when hook output has `success=true` and `reason=ok`.
- The runner now captures `with_budi_hook` per session and retries one time if hook retrieval fails with transient reasons.
- Always compare runs with the same prompt-set fingerprint.

## Optional retrieval-only regression checks

Use fixture-driven retrieval eval when you want ranking metrics independent of full model behavior:

```bash
budi eval retrieval --fixtures ./fixtures/retrieval_eval/golden.example.json --limit 8 --mode hybrid
budi eval retrieval --fixtures ./fixtures/retrieval_eval/golden.example.json --limit 8 --mode hybrid --fail-on-regression --max-regression 0.01
```
