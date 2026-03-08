# Benchmark Methodology

## Latest results

Across 13 runs, 216 judged tasks (React and ripgrep repos):

- **Cost**: 18.7% lower on average
- **Speed**: 13% faster on average (median API time); up to 30% on some repos
- **Quality**: budi wins 160/216 judged tasks (~75% win rate)

Full evidence: `docs/benchmark-details.md`

## What we measure

For each prompt, we run two modes:

- `no_budi`: Claude with hooks disabled
- `with_budi`: Claude with hooks enabled

We compare:

- API duration (`duration_ms`)
- End-to-end wall duration
- Total cost (`total_cost_usd`)
- Judge winner + quality + grounding scores
- Hook injection health (`success`, `reason`, `context_chars`)

## Benchmark datasets

Current prompt sets:

- `scripts/dev/benchmarks/react-structural-v1.prompts.json` — 18 prompts, React source (architecture, symbol lookup, call tracing)
- `scripts/dev/benchmarks/ripgrep-v1.prompts.json` — 18 prompts, ripgrep source (same categories)
- `scripts/dev/benchmarks/flask-structural-v1.prompts.json` — 18 prompts, Flask source (Python)
- `scripts/dev/benchmarks/terraform-v1.prompts.json` — 18 prompts, Terraform source (Go)

Results live in root-level `ab-bench-*` directories, one folder per run.

## Reproduce

Clone repos:

```bash
git clone --depth 1 https://github.com/facebook/react.git ./react
git clone --depth 1 https://github.com/BurntSushi/ripgrep.git ./ripgrep
```

Run A/B on each:

```bash
python3 scripts/ab_benchmark_runner.py \
  --repo-root "/absolute/path/react" \
  --prompts-file "./scripts/dev/benchmarks/react-structural-v1.prompts.json" \
  --out-dir "./tmp/bench_react" \
  --run-label "react-v1"

python3 scripts/ab_benchmark_runner.py \
  --repo-root "/absolute/path/ripgrep" \
  --prompts-file "./scripts/dev/benchmarks/ripgrep-v1.prompts.json" \
  --out-dir "./tmp/bench_ripgrep" \
  --run-label "ripgrep-v1"
```

## Notes about validity

- A row is considered an injected `with_budi` run when hook output has `success=true` and `reason=ok`.
- The runner captures `with_budi_hook` per session and retries once if hook retrieval fails with transient reasons.
- Always compare runs with the same prompt-set fingerprint.

## Retrieval-only regression checks

Use fixture-driven retrieval eval for ranking metrics independent of full model behavior:

```bash
budi eval retrieval --fixtures ./scripts/dev/retrieval_eval/golden.example.json --limit 8 --mode hybrid
budi eval retrieval --fixtures ./scripts/dev/retrieval_eval/golden.example.json --limit 8 --mode hybrid --fail-on-regression --max-regression 0.01
```
