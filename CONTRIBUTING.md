# Contributing to budi

## Build

```bash
cargo build
cargo build --release   # production binary
```

## Test

```bash
cargo test              # all tests (budi-core + budi-cli + budi-daemon)
cargo test -p budi-core # core tests only
```

## Install locally

```bash
./scripts/install.sh    # builds release + installs to ~/.local/bin/
budi --version
```

Or manually:

```bash
cargo build --release
cp target/release/budi target/release/budi-daemon ~/.local/bin/
pkill -9 -f budi-daemon
budi sync               # restarts daemon + syncs data
```

## Validate cost accuracy

```bash
python3 scripts/dev/validate_costs.py              # all time
python3 scripts/dev/validate_costs.py --since 2026-03-18  # last 7 days
```

Compares Budi's cost calculations against raw JSONL transcript data. Reports per-model breakdown, rounding error, and 1-hour cache token detection.

## Adding a new provider

1. Create `crates/budi-core/src/providers/<name>.rs`
2. Implement the `Provider` trait: `name()`, `display_name()`, `is_available()`, `discover_files()`, `parse_file()`
3. Optionally implement `sync_direct()` for API-based data sources (like Cursor Usage API)
4. Add a pricing function `<name>_pricing_for_model(model: &str) -> ModelPricing`
5. Register in `crate::provider::all_providers()`
6. Add hook installation in `crates/budi-cli/src/commands/init.rs` if the agent supports hooks
7. Add tests

## Adding a new enricher

1. Create a struct implementing `pipeline::Enricher` in `crates/budi-core/src/pipeline/enrichers.rs`
2. `enrich(&mut self, msg: &mut ParsedMessage) -> Vec<Tag>` — mutate the message and/or return tags
3. Register in `Pipeline::new()` in `crates/budi-core/src/pipeline/mod.rs`
4. Enricher order matters: Hook → Identity → Git → Cost → Tag

## Releasing

```bash
./scripts/release.sh 7.0.0        # bump version + update Cargo.lock
./scripts/release.sh 7.0.0 --tag  # also create git tag

# Then:
git commit -am "chore: bump version to 7.0.0"
git push origin main v7.0.0       # CI builds release artifacts
```
