# Contributing to budi

## Build

```bash
cargo build
cargo build --release   # production binary
```

## Test

```bash
cargo test              # all 153 tests (budi-core + budi-cli + budi-daemon)
cargo test -p budi-core # core tests only
```

## Install locally

```bash
./scripts/install.sh    # builds release + installs to ~/.local/bin/budi
budi --version
```

## Run benchmarks

Requires a target repo indexed with budi and `ANTHROPIC_API_KEY` set.

```bash
# React repo (default)
python3 scripts/dev/ab_benchmark_runner.py \
  --repo /path/to/react \
  --prompts-file fixtures/benchmarks/react-structural-v1.prompts.json \
  --judge-passes 3

# Cross-repo (ripgrep)
python3 scripts/dev/ab_benchmark_runner.py \
  --repo /path/to/ripgrep \
  --prompts-file fixtures/benchmarks/ripgrep-v1.prompts.json \
  --judge-passes 3
```

Results land in `ab-bench-<timestamp>/`.

## Run stress tests

Requires a running budi daemon (`budi daemon start`) and an indexed repo.

```bash
python3 scripts/dev/stress_test.py --repo /path/to/repo
```

## Adding a new intent kind

1. Add the variant to `QueryIntentKind` in `crates/budi-core/src/retrieval.rs`
2. Add trigger keywords to `classify_intent()`
3. Add channel weights to `weights_for_intent()`
4. Add retrieval limit to `intent_retrieval_limit()`
5. Add score floor to `min_selection_score()` if needed
6. Add call graph budget to the `match intent.kind` block in `daemon.rs`
7. Add tests in `retrieval.rs` `#[cfg(test)]` block

## Tuning score floors

Score floors in `min_selection_score()` control how aggressive injection is per intent. The general approach:

1. Run the AB benchmark before and after: `python3 scripts/dev/ab_benchmark_runner.py --judge-passes 3`
2. Check per-prompt context sizes with `debug_io = true` in config
3. A floor that's too low → noisy context, Claude distracted by irrelevant snippets
4. A floor that's too high → under-injection, budi skips relevant context

The current floors were tuned against 18 React prompts and 18 ripgrep prompts. See `MEMORY.md` for benchmark history.

## Chunking changes

After any change to `chunking.rs` (especially `dominant_symbol_hint`, `symbol_from_line`, or `append_node_chunks`), **all repos must be re-indexed with `--hard`**:

```bash
budi index --hard --progress
```

Symbol hints are computed at index time, not query time. Stale chunks won't benefit from improved hint logic.

## Releasing

```bash
./scripts/dev/prepare_release.sh   # bumps version, updates Cargo.toml
./scripts/dev/create_release.sh    # builds, packages, creates GitHub release
```
