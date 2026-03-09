# budi

[![CI](https://github.com/siropkin/budi/actions/workflows/ci.yml/badge.svg)](https://github.com/siropkin/budi/actions/workflows/ci.yml)
[![Release](https://github.com/siropkin/budi/actions/workflows/release.yml/badge.svg)](https://github.com/siropkin/budi/actions/workflows/release.yml)
[![License](https://img.shields.io/github/license/siropkin/budi)](https://github.com/siropkin/budi/blob/main/LICENSE)
[![GitHub stars](https://img.shields.io/github/stars/siropkin/budi?style=social)](https://github.com/siropkin/budi)

**The context buster for Claude Code.**

`budi` finds the code Claude is about to look for and injects it before Claude starts searching.

That means faster first useful answers, fewer wasted tool calls, lower token burn, and better grounding on medium and large repos.

Stop paying Claude to rediscover your codebase on every prompt.

- Local-first: your code stays on your machine
- Fast: retrieval runs in about 10ms
- Automatic: Claude Code hooks work in the background
- Practical: skip once with `@nobudi`, force once with `@forcebudi`

## Why it feels better

Without `budi`, Claude often spends its first few turns doing repo discovery: searching for files, opening imports, tracing the obvious path, and only then starting to reason.

With `budi`, the likely files are already in context when Claude sees your prompt.

```mermaid
flowchart LR
    A[You ask Claude Code] --> B[budi detects intent]
    B --> C[budi searches the local index]
    C --> D[budi injects the best snippets]
    D --> E[Claude starts with the right code]
```

## Latest A/B numbers

Across the latest aggregate benchmark runs:

- 13 runs
- 216 judged tasks
- 13% faster median API time
- 18.7% lower average cost
- 160/216 judged wins (about 75%)
- Up to 30% faster on some repos

Latest fully public reproducible snapshot across 3 open-source repos and 9 cases:

- 32.16% faster API time
- 31.48% faster end-to-end time
- 18.42% lower total cost

```mermaid
xychart-beta
    title "Latest public A/B snapshot"
    x-axis ["API speedup", "Wall speedup", "Cost reduction"]
    y-axis "Percent" 0 --> 35
    bar [32.16, 31.48, 18.42]
```

```mermaid
pie showData
    title "Latest aggregate judged outcomes"
    "budi wins" : 160
    "other outcomes" : 56
```

The README keeps benchmark repo names out of the headline copy. Full methodology, prompts, raw evidence, and judge rationale live in `docs/benchmark.md` and `docs/benchmark-details.md`.

## Install in 60 seconds

1. Install the local binary:

```bash
./scripts/install.sh --from-release
# or build locally:
./scripts/install.sh
```

2. Install the Claude Code plugin:

```text
/plugin marketplace add siropkin/budi
/plugin install budi-hooks@budi-plugins
```

3. Enable `budi` in your repo:

```bash
cd /path/to/your/repo
budi init
budi index --hard --progress
```

Then use Claude Code normally. `budi` runs silently in the background.

## What happens on each prompt

1. `budi` intercepts your prompt through a Claude Code hook.
2. It figures out intent: symbol lookup, architecture question, call tracing, config hunt, and more.
3. It searches a local index using lexical, semantic, symbol, and graph signals.
4. It injects the best snippets into Claude's context.
5. Claude starts answering with the likely code already in view.

## Useful commands

```bash
budi index
budi index --hard --progress
budi repo status
budi repo search "payment validation"
budi repo preview "why is the payment form failing validation?"
```

For troubleshooting:

```bash
budi doctor
# deeper watcher/index diagnostics:
budi doctor --deep
```

## Prompt controls

Skip context injection for one prompt:

```text
@nobudi your prompt here
```

Force context injection for one prompt:

```text
@forcebudi your prompt here
```

## Docs

- Benchmark methodology: `docs/benchmark.md`
- Public evidence: `docs/benchmark-details.md`
- Configuration: `docs/configuration.md`
- Architecture: `docs/architecture.md`
- Installer details: `docs/installer.md`

## Privacy

Everything runs locally. No cloud index. No repo upload. No external retrieval service needed to do the core job.
