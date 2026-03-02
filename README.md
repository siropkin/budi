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

`budi` helps Claude Code in large repos.
It adds useful local code context to your prompt **before** Claude answers.

Think of it as: "better context, less back-and-forth."
Built in Rust, so the hook path stays fast and lightweight.
Because every AI assistant deserves a buddy too.

## Why use budi

- Faster responses in big codebases
- Lower token usage in many cases
- Better repo grounding for answers
- Fully local index (no cloud index needed)
- Git-aware (branch, HEAD, uncommitted changes)
- Rust-powered speed for low-latency prompt injection
- Smart skip for non-code / low-confidence prompts

## Install

From this repo (build from source):

```bash
./scripts/install.sh
budi --version
```

Fast path (download prebuilt release binaries):

```bash
./scripts/install.sh --from-release
budi --version
```

Tip: `--from-release` uses GitHub Releases and the `gh` CLI.
If the repo is private, run `gh auth login` first.

Remove later if needed:

```bash
./scripts/uninstall.sh
```

## Quick start in your repo

```bash
cd /path/to/your/repo
budi init
budi index --hard --progress
budi status
```

After that, use Claude Code normally.
`budi` runs through Claude hooks and injects context automatically.

## Publishable Claude plugin

This repo also ships a Claude Code plugin marketplace entry for `budi-hooks`.
If you want to install the hook automation as a plugin instead of running
`budi init`, use:

```bash
/plugin marketplace add siropkin/budi
/plugin install budi-hooks@budi-plugins
```

`budi-hooks` installs two hooks:
- `UserPromptSubmit` -> `budi hook user-prompt-submit`
- `PostToolUse` (`Write|Edit`) -> `budi hook post-tool-use`

By default, `budi` keeps index/config/log data outside your repo at:

- `~/.local/share/budi/repos/<repo-id>/...`

`budi init` prints the exact local data path for each repo.

## Why hooks, not MCP? (quick)

For this specific problem (injecting repo context before every answer), hooks are a better fit:

- Hook runs automatically on each prompt (no extra model decision step)
- Context is attached before Claude answers, so grounding is more consistent
- Setup is one-time per repo (`budi init`), then it "just works"
- More predictable behavior for A/B benchmark comparisons

MCP is still great for interactive tools (APIs, actions, custom commands).  
In practice, many teams can use both: `budi` for automatic context + MCP for extra capabilities.

## Simple example: prompt in, context added

What you type in Claude Code:

```text
Where is team configuration loaded, and which function resolves team members?
```

What `budi` adds automatically (simplified):

```text
[budi deterministic context]
branch: main
head: 23e124a...
dirty_files:
- src/config/runtime-core.ts

snippets:
### src/config/runtime-core.ts:218-260
function loadTeamsFromJson(...) { ... }

### src/services/github-team-resolver.ts:201-280
async function initializeTeamMembers(...) { ... }
```

Result: Claude starts with relevant files/functions already in context, so it can answer faster with fewer discovery steps.

## Day-to-day commands

```bash
budi init              # install/update hooks in current repo
budi index             # incremental re-index
budi index --hard      # full rebuild
budi index --hard --progress # full rebuild + live per-file progress + phase
budi status            # daemon/index/hooks health
budi preview "<prompt>"# see context that would be injected
budi ignore <path>     # add file to local budi ignore list
```

## What happens under the hood (simple)

1. You send a prompt in Claude Code.
2. `budi` retrieves relevant local snippets.
3. `budi` adds that context to the request.
4. Claude answers with better repo context.
5. After file edits, `budi` updates index in the background.

Prompt controls:
- `@nobudi`: skip context injection for this prompt
- `@forcebudi`: force context injection for this prompt (overrides smart skip)

## Optional debug logging (off by default)

Debug hook I/O logging is a special mode for troubleshooting and benchmarks.
For normal usage and best speed, keep it off.

In local `budi` config (`~/.local/share/budi/repos/<repo-id>/config.toml`):

```toml
smart_skip_enabled = true
skip_non_code_prompts = true
min_confidence_to_inject = 0.45

debug_io = true
debug_io_full_text = false
debug_io_max_chars = 1200
```

- `smart_skip_enabled = true` (default): allows budi to skip low-value injection
- `skip_non_code_prompts = true` (default): skips obvious non-code prompts
- `min_confidence_to_inject`: confidence threshold used by smart skip
- `debug_io = false` (default): no hook JSONL logging
- `debug_io = true`: writes hook events to `~/.local/share/budi/repos/<repo-id>/logs/hook-io.jsonl`
- `debug_io_full_text = true`: logs full prompt/context text (use carefully)
- `debug_io_max_chars`: max chars in excerpt mode

`budi preview` now prints retrieval diagnostics (intent, confidence, recommended injection).

## Benchmark your own repo (A/B)

Run no-budi vs with-budi and compare speed, cost, and quality:

```bash
python3 scripts/ab_benchmark_runner.py \
  --repo-root "/path/to/repo" \
  --prompts-file "./prompts.txt" \
  --run-label "my-repo-baseline"
```

Prompt inputs supported:
- one prompt per line (`.txt`)
- JSON array of prompts (`.json`)
- repeated `--prompt` flags

Every benchmark output includes a prompt-set fingerprint (SHA256), so different runs are easy to compare fairly.

## Real run snapshot

Latest judged A/B runs (`scripts/ab_benchmark_runner.py`, measured on 2026-03-02):
- Big frontend repo: ~24% faster average API time, ~22% faster average wall time, ~51% lower total cost.
- Big backend repo: ~71% slower average API time, ~62% slower average wall time, ~46% higher total cost.
- Combined across both runs: ~24% slower average API time, ~20% slower average wall time, ~9% higher total cost.

| Repo profile | Prompts | Avg API time delta (with vs no) | Avg wall time delta (with vs no) | Total cost delta (with vs no) | Judge winners (with/no/tie) | Quality delta (with-no) | Grounding delta (with-no) |
| --- | ---: | ---: | ---: | ---: | --- | ---: | ---: |
| Big frontend repo | 2 | -24.08% | -21.89% | -51.20% | 0 / 2 / 0 | -1.00 | -1.00 |
| Big backend repo | 2 | +71.07% | +62.29% | +45.74% | 2 / 0 / 0 | +1.25 | +1.50 |
| Combined | 4 | +24.02% | +19.79% | +9.01% | 2 / 2 / 0 | +0.12 | +0.25 |

## GitHub Actions goodies

- Every push/PR runs CI (`fmt`, `clippy`, `test`, release build check)
- Plugin/marketplace updates are strictly validated via `validate-plugins.yml`
- Every `v*` tag builds prebuilt tarballs for:
  - Linux x86_64
  - macOS arm64
- Release uploads include `SHA256SUMS` for verification

To publish a new prebuilt release:

```bash
# 1) Keep plugin + marketplace versions in sync
./scripts/bump-plugin-version.sh 1.0.6

# 2) Ensure Cargo workspace version is also 1.0.6
#    (release workflow enforces tag == Cargo version)

# 3) Tag and publish
git tag v1.0.6
git push origin v1.0.6
```

To publish only the Claude plugin marketplace entry through CI:

1. Open GitHub Actions -> `Publish Marketplace`
2. Run with input `version=X.Y.Z`
3. CI bumps plugin+marketplace versions and opens a publish PR automatically

Optional secret for custom push credentials:
- `MARKETPLACE_PUSH_TOKEN` (falls back to `GITHUB_TOKEN` if not set)

## Workspace layout (for contributors)

- `crates/budi-cli`: `budi` CLI and hook handlers
- `crates/budi-daemon`: background daemon
- `crates/budi-core`: indexing, retrieval, config, hook schemas
- `crates/budi-bench`: benchmark harness
- `scripts/install.sh`: installer
- `scripts/uninstall.sh`: uninstaller
- `scripts/ab_benchmark_runner.py`: A/B benchmark runner
- `scripts/bump-plugin-version.sh`: sync `budi-hooks` plugin + marketplace versions

More details:
- `docs/architecture.md`
- `docs/benchmark.md`
