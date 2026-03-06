# budi (Buddy)

[![CI](https://github.com/siropkin/budi/actions/workflows/ci.yml/badge.svg)](https://github.com/siropkin/budi/actions/workflows/ci.yml)
[![Release](https://github.com/siropkin/budi/actions/workflows/release.yml/badge.svg)](https://github.com/siropkin/budi/actions/workflows/release.yml)

```text
 ____            _ _
| __ ) _   _  __| (_)
|  _ \| | | |/ _` | |
| |_) | |_| | (_| | |
|____/ \__,_|\__,_|_|
```

## TL;DR

budi finds the most relevant code in your repo and hands it to Claude *before* Claude starts working. **This makes Claude faster and cheaper** — it spends less time searching, and less of your money doing it.

- Without budi: Claude walks into your office, looks around, opens drawers, reads random files, wastes 3–5 tool calls, then starts helping
- With budi: a local assistant has already put the right files on Claude's desk before Claude even sat down

Everything runs locally. Nothing leaves your machine.

**Latest benchmark: 13–30% faster responses, 18% lower cost, budi wins ~75% of quality-judged tasks.**

**Install:**

```text
/plugin marketplace add siropkin/budi
/plugin install budi-hooks@budi-plugins
```

**Set up once per repo:**

```bash
cd /path/to/your/repo
budi init
budi index --hard --progress
```

Then use Claude Code normally. budi runs silently in the background.

---

## How it works

Every time you submit a prompt in Claude Code:

1. budi intercepts it via a Claude Code hook (`UserPromptSubmit`)
2. budi searches its local index in ~10ms — no AI model needed
3. It injects the most relevant code snippets directly into Claude's context
4. When you edit files, budi silently updates its index in the background

The index is built with `budi index` — like a private Google for your codebase.

### What it looks like in practice

You type:

```
why is the payment form failing validation?
```

Before Claude sees that prompt, budi searches your local index in ~10ms and finds `PaymentForm.tsx`, `validateCard.ts`, and the relevant error handler. It prepends those to your prompt automatically.

Claude now has the exact code it needs — upfront, no searching. Instead of spending its first 3 responses opening files, it answers immediately.

Without budi, Claude would grep for "validation", read `PaymentForm.tsx`, notice it imports `validateCard`, read that too, maybe read the error handler — each step a tool call, each tool call burning tokens and time. By the time Claude starts reasoning, it has already spent your money just finding the map.

## Benchmark

Across 13 runs, 216 judged tasks (React and ripgrep repos):

- **Cost**: 18.7% lower on average
- **Speed**: 13% faster on average (median API time); up to 30% on some repos
- **Quality**: budi wins 160/216 judged tasks (~75% win rate)

The quality picture improved significantly as retrieval got more conservative — earlier runs showed mixed results, newer runs show consistent wins.

- Methodology: `docs/benchmark.md`
- Full evidence (repos, prompts, injected context, responses, judge rationale): `docs/benchmark-details.md`


## Install (full options)

### Claude plugin

```text
/plugin marketplace add siropkin/budi
/plugin install budi-hooks@budi-plugins
```

### Local binary

```bash
./scripts/install.sh
budi --version
```

To remove later:

```bash
./scripts/uninstall.sh
```

---

## Optional controls

Skip context injection for one prompt:
```
@nobudi your prompt here
```

Force context injection:
```
@forcebudi your prompt here
```

Daily commands:
```bash
budi index
budi index --hard --progress
budi repo status
budi repo search "<query>"
budi repo preview "<prompt>"
budi doctor --deep
```

---

## Architecture

See `docs/architecture.md`.
