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

## Product idea: hybrid local + cloud AI

- Local buddy: `budi` does private indexing/retrieval on your machine
- Cloud big brother: Claude does deeper reasoning and generation
- Together: local speed + privacy, cloud intelligence + broad reasoning

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
budi repo status
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
- embedding model cache: `~/.local/share/budi/fastembed-cache`

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
budi index --ignore-pattern "scratch/**" --include-ext proto # one-shot index-scope overrides
budi index --hard --progress # full rebuild + live per-file progress + phase
budi repo status       # daemon/index/hooks health
budi repo stats        # local index stats (SQLite catalog + Tantivy)
budi repo list         # list local repo-state storage entries
budi repo remove --repo-root "/path/to/repo" --dry-run # preview removing one repo state
budi repo wipe --confirm --dry-run # preview wiping all local repo-state dirs
budi repo preview "<prompt>" # see context that would be injected
budi repo search "<query>"   # run retrieval and list top matching snippets
budi bench --prompt "<prompt>" --iterations 30 # retrieval latency/context benchmark
budi eval retrieval --fixtures ./fixtures/retrieval_eval/golden.example.json --limit 8 --mode hybrid
budi doctor --deep     # extended consistency/route/retrieval diagnostics
budi observe enable    # start metadata-only local usage logging
budi observe report    # summarize all logged usage (default: all history)
budi observe report --days 7  # summarize rolling last 7 days from now
budi observe disable   # stop usage logging
```

## What happens under the hood (simple)

`budi` uses local RAG (Retrieval-Augmented Generation):
"retrieve useful repo context first, then let Claude generate the answer."

How indexing works:
1. `budi index --hard` scans git-listed files in your repo (`git ls-files`), respecting `.gitignore`, root `*.ignore` files (for example `.cursorignore`/`.codeiumignore`/`.contextignore`), global `~/.local/share/budi/global.budiignore`, and repo-local `.budiignore` (`!unignore` supported), then applies a code-first file-type policy (`index_extensions` + extensionless `index_basenames`) plus optional one-shot CLI overrides (`--ignore-pattern`, `--include-ext`).
2. It splits indexed files into small chunks (so it can retrieve precise snippets, not whole files).
3. It builds a local search index for those chunks:
   - keyword/symbol/path search (fast exact matching)
   - semantic search vectors (meaning-based matching)
4. It stores everything locally on your machine (`~/.local/share/budi/...`).
5. It keeps a local SQLite catalog (`index.sqlite`) with file hashes + chunks so hook-driven updates can re-index only changed or hinted files instead of rescanning the full tree each time.
6. It keeps a global embedding cache (`~/.local/share/budi/embedding-cache.sqlite`) keyed by chunk content hash, so repeated code text is not re-embedded on every run.

How prompt-time retrieval works:
1. You send a prompt in Claude Code.
2. `budi` searches the local index (keyword + semantic + symbol/path + resolver-backed call-graph signals).
3. It ranks snippets with an intent-aware policy (core channels first, lightweight re-rank rules), then adds a small deterministic graph-neighbor expansion around top hits before injecting context.
4. Claude answers with better repo grounding and fewer "where is this defined?" steps.
5. After file edits, `budi` updates the index in the background from hook hints and daemon file-watch events, plus a periodic reconcile pass to catch missed events.

Why this works:
- Search is fast because the heavy work (indexing) is precomputed.
- Answers are better grounded because Claude gets real file snippets up front.
- Privacy is preserved because indexing and retrieval stay local.

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
- `debug_io_max_chars`: max chars in excerpt mode (`0` = metadata-only, no text excerpts)

`budi repo preview` now prints retrieval diagnostics (intent, confidence, recommended injection).

`budi eval retrieval` runs an offline retrieval-quality check against JSON fixtures and reports `hit@k`, `MRR`, and `precision/recall/F1@k` (`k=1,3,5`) with per-intent breakdowns. It also writes a timestamped JSON artifact to `./.budi/eval/runs/` (or `--out-dir`) and can compare against a prior artifact (`--baseline` or auto-previous artifact) with optional regression gating via `--fail-on-regression --max-regression <drop>`. If `--fixtures` is omitted, it reads `./.budi/eval/retrieval.json` in the repo root.

## Observe real usage (day/week)

If you want to validate real daily impact (not synthetic benchmarks), use observe mode:

```bash
# 1) Enable metadata-only local logging (no prompt/context text)
budi observe enable --repo-root "/path/to/repo"

# 2) Use Claude Code normally for a day or week

# 3) Generate summary reports
budi observe report --repo-root "/path/to/repo"          # all available history
budi observe report --days 7 --repo-root "/path/to/repo"

# Optional: export machine-readable report to file
budi observe report --all --json --out "./budi-observe.json" --repo-root "/path/to/repo"

# 4) Disable when done
budi observe disable --repo-root "/path/to/repo"
```

Notes:
- `--days N` means a rolling lookback window from "now" (for example, `--days 7` = last 7 days).
- If you omit both `--days` and `--all`, `budi` reports all available history.

The report shows injection rate, skip reasons, retrieval confidence, hook latency, post-edit index update health, and a simple health verdict with tuning hypotheses.

Auto snapshots on macOS (launchd):

```bash
# Generate observe snapshots every 6 hours (and run one immediately)
./scripts/setup-observe-launchd.sh \
  --repo-root "/path/to/repo" \
  --interval-secs 21600 \
  --window-days 1

# Remove the scheduled job later (use the label printed by setup)
./scripts/remove-observe-launchd.sh --label "com.siropkin.budi.observe.<repo>.<id>"
```

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

Latest judged cross-repo A/B sweep (`scripts/ab_benchmark_runner.py`, measured on 2026-03-02):
- 16 repos, 32 prompts total (32 judged).
- Overall result: faster and cheaper with `budi`, while quality was mixed in this run.

| What changed with `budi` | Result |
| --- | --- |
| API speed | 8.84% faster |
| End-to-end speed (wall time) | 9.25% faster |
| Total cost | 32.01% lower |
| Quality winner count | 13 (`with_budi`) / 17 (`no_budi`) / 2 tie |
| Repos improved | 10/16 faster API, 11/16 faster wall time, 14/16 cheaper |

Technical note (for deeper comparison): aggregate quality delta `-0.27`, grounding delta `-0.21`; median per-repo deltas were API `-2.89%`, wall `-2.80%`, and cost `-25.39%`.

How this snapshot was produced (simple):
- We ran the same small prompt set across a mix of local repos (frontend-style, backend-style, infra, and tools).
- For each prompt, we executed two runs:
  - `no_budi`: Claude with hooks disabled
  - `with_budi`: Claude with hooks enabled (`budi` injection path on)
- Speed/cost numbers come from Claude CLI JSON output (`duration_ms`, wall time, tokens, USD cost).
- Quality/grounding winners were judged by a separate Claude pass that compares both answers side-by-side using a fixed JSON schema (`winner`, quality, grounding, actionability).
- Repo names and internal paths are intentionally not shown in this public summary.

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
./scripts/bump-plugin-version.sh 1.0.12

# 2) Ensure Cargo workspace version is also 1.0.12
#    (release workflow enforces tag == Cargo version)

# 3) Tag and publish
git tag v1.0.12
git push origin v1.0.12
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
- `scripts/install.sh`: installer
- `scripts/uninstall.sh`: uninstaller
- `scripts/setup-observe-launchd.sh`: schedule periodic observe snapshots
- `scripts/remove-observe-launchd.sh`: remove scheduled observe snapshots
- `scripts/observe_snapshot_runner.sh`: snapshot generation worker used by launchd
- `scripts/ab_benchmark_runner.py`: A/B benchmark runner
- `scripts/bump-plugin-version.sh`: sync `budi-hooks` plugin + marketplace versions

More details:
- `docs/architecture.md`
- `docs/benchmark.md`
