# Benchmark Methodology

## Latest results

Across 4 open-source repos (React, ripgrep, Flask, Terraform) with 18 prompts each:

- **Cost**: 5–24% lower on average
- **Speed**: faster median API time depending on repo
- **Quality**: budi wins or ties on most prompts; React 8W/3L/7T, Flask 3W/1L/14T, ripgrep 1W/2L/12T, Terraform 5W/5L/8T (v2.49.0–v2.50.0)

These numbers come from the latest validated full-suite A/B runs. HNSW non-determinism causes ±2–3 prompt variance per run; run at least 2 passes before drawing conclusions. Baseline Claude quality has improved significantly — many prompts now score 9→9 as ties.

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

Results are stored in `~/.local/share/budi/repos/<repo>/benchmarks/` per run.

## Reproduce

Clone repos:

```bash
git clone --depth 1 https://github.com/facebook/react.git ./react
git clone --depth 1 https://github.com/BurntSushi/ripgrep.git ./ripgrep
git clone --depth 1 https://github.com/pallets/flask.git ./flask
git clone --depth 1 https://github.com/hashicorp/terraform.git ./terraform
```

Index each repo, then run A/B:

```bash
budi init --repo-root /absolute/path/react
budi index --hard --repo-root /absolute/path/react

python3 scripts/dev/ab_benchmark_runner.py \
  --repo-root "/absolute/path/react" \
  --prompts-file "./scripts/dev/benchmarks/react-structural-v1.prompts.json"
```

Use `--validation-tier fast` to skip the judge pass, or `--validation-tier focused --prompt-indices 3,7,12` to judge specific prompts.

## Notes about validity

- A row is considered an injected `with_budi` run when hook output has `success=true` and `reason=ok`.
- The runner captures `with_budi_hook` per session and retries once if hook retrieval fails with transient reasons.
- Always compare runs with the same prompt-set fingerprint.
- HNSW vector search is non-deterministic; run at least 2 passes before drawing conclusions.
