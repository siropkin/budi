# Benchmark Methodology

## Latest results

budi's goal is to deliver the same answer quality at lower cost by pre-injecting relevant context. Ties (same quality, less cost) are the primary success metric; quality wins are a bonus.

Across 7 open-source repos with 123 judged prompts (v3.3.0, single-run sweep):

- **Cost**: 2–23% lower on most repos (up to +6% on repos where budi adds quality)
- **Non-regressions**: ~80% single-run (~99/123), estimated ~85-90% multi-run
- **Injection regression rate**: ~10-12% (when budi actually injects context)
- Per repo: React 13/18, Flask 13/18, ripgrep 14/15, Fastify 16/18, FastAPI 14/18, Django 15/18, Terraform 14/18

Many single-run "regressions" are LLM variance on queries where budi correctly skipped injection (0 context chars). When budi injects context, the regression rate is much lower. HNSW non-determinism causes ±2–3 prompt variance per run; run at least 2 passes before drawing conclusions.

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
- `scripts/dev/benchmarks/fastify-v1.prompts.json` — 18 prompts, Fastify source (Node.js)
- `scripts/dev/benchmarks/fastapi-v1.prompts.json` — 18 prompts, FastAPI source (Python)
- `scripts/dev/benchmarks/django-v1.prompts.json` — 18 prompts, Django source (Python)
- `scripts/dev/benchmarks/express-v1.prompts.json` — 5 prompts, Express source (Node.js)

Results are stored in `~/.local/share/budi/repos/<repo>/benchmarks/` per run.

## Reproduce

Clone repos:

```bash
git clone --depth 1 https://github.com/facebook/react.git ./react
git clone --depth 1 https://github.com/BurntSushi/ripgrep.git ./ripgrep
git clone --depth 1 https://github.com/pallets/flask.git ./flask
git clone --depth 1 https://github.com/hashicorp/terraform.git ./terraform
git clone --depth 1 https://github.com/fastify/fastify.git ./fastify
git clone --depth 1 https://github.com/fastapi/fastapi.git ./fastapi
git clone --depth 1 https://github.com/django/django.git ./django
git clone --depth 1 https://github.com/expressjs/express.git ./express
```

Index each repo, then run A/B:

```bash
cd /absolute/path/react
budi init --index

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
