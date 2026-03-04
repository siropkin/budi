# budi (Buddy)

[![CI](https://github.com/siropkin/budi/actions/workflows/ci.yml/badge.svg)](https://github.com/siropkin/budi/actions/workflows/ci.yml)
[![Release](https://github.com/siropkin/budi/actions/workflows/release.yml/badge.svg)](https://github.com/siropkin/budi/actions/workflows/release.yml)

`budi` is a local context layer for Claude Code.
It injects relevant repo snippets before Claude answers, so you spend less on discovery and get to decisions faster.

## Why use budi

Latest public benchmark (React + Flask + Express, 9 judged tasks):

- **32.16% faster** API-time responses with `budi`
- **31.48% faster** end-to-end wall time
- **18.42% lower** total cost
- **9/9** hook injections succeeded (`reason=ok`)
- Judge quality was mixed in this pass (see full evidence below)

Full evidence (exact repos, prompts, injected context excerpts, final responses, judge rationale):
- `docs/benchmark-details.md`

Methodology and reproduction:
- `docs/benchmark.md`

## Install in 60 seconds

### Easiest: Claude plugin install

```text
/plugin marketplace add siropkin/budi
/plugin install budi-hooks@budi-plugins
```

### Local binary install

```bash
./scripts/install.sh
budi --version
```

Remove later:

```bash
./scripts/uninstall.sh
```

## Use once per repo

```bash
cd /path/to/your/repo
budi init
budi index --hard --progress
```

Then use Claude Code normally.

## What budi does automatically

- `UserPromptSubmit`: runs retrieval and injects deterministic context
- `PostToolUse` (`Write|Edit`): updates index in background
- Smart skip: avoids injection for low-value/non-code prompts

Prompt controls:
- `@nobudi` to skip context injection
- `@forcebudi` to force context injection

## Daily commands

```bash
budi index
budi index --hard --progress
budi repo status
budi repo search "<query>"
budi repo preview "<prompt>"
budi doctor --deep
```

## Reproduce the public benchmark

Public prompt set:
- `fixtures/benchmarks/public_v2.json`

Run:

```bash
python3 scripts/ab_benchmark_runner.py \
  --repo-root "/path/to/repo" \
  --prompts-file "./fixtures/benchmarks/public_v2.json" \
  --out-dir "./tmp/ab_my_repo" \
  --run-label "my-repo-v1"
```

## Advanced docs

- Architecture: `docs/architecture.md`
- Benchmark methodology: `docs/benchmark.md`
- Full benchmark evidence: `docs/benchmark-details.md`
- Installer notes: `docs/installer.md`
