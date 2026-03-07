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

**Latest benchmark: 13–30% faster responses, 18% lower cost, budi wins ~83% of quality-judged tasks (React) and ~72% on ripgrep.**

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
- **Quality**: 15/18 wins (83%) on React, 13/18 wins (72%) on ripgrep

The quality picture improved significantly as retrieval got smarter — intent routing, score floors, and symbol-definition accuracy tuning drove consistent gains over early phases.

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

## Hooks

`budi init` installs four Claude Code hooks automatically:

| Hook | Command | What it does |
|------|---------|--------------|
| `SessionStart` | `budi hook session-start` | Injects a project map and recently-relevant files into the system prompt |
| `UserPromptSubmit` | `budi hook user-prompt-submit` | Main retrieval hook — searches local index and injects context before each prompt |
| `PostToolUse` | `budi hook post-tool-use` | Fires after `Write`, `Edit`, `Read`, `Glob` — prefetches graph neighbors for open files |
| `Stop` | `budi hook session-end` | Writes a session summary to the hook log |

All hook output uses `additionalContext` (for `UserPromptSubmit`/`SessionStart`) or `AsyncSystemMessageOutput` (for `PostToolUse`). Nothing is sent to any external service.

---

## Configuration

Config lives at `~/.local/share/budi/repos/<repo-id>/config.toml` (created by `budi init`). All fields are optional — defaults work well for most repos.

Key fields:

| Field | Default | Description |
|-------|---------|-------------|
| `retrieval_limit` | 8 | Max snippets per query (per-intent limits of 5–8 apply automatically) |
| `context_char_budget` | 12000 | Max total characters of injected context |
| `min_inject_score` | 0.05 | Minimum score to inject any context; raise for less noise |
| `skip_non_code_prompts` | true | Skip injection for clearly non-code questions |
| `debug_io` | false | Log all hook I/O to `logs/hook-io.jsonl` |
| `debug_io_full_text` | false | Include full context text in the log |

See `docs/configuration.md` for all 22 fields with descriptions and defaults.

---

## Architecture

See `docs/architecture.md`.
