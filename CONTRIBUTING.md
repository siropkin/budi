# Contributing to budi

Thanks for helping improve budi.

## Quick start

```bash
cargo build
cargo test
```

## Local development workflow

1. Create a branch from `main`.
2. Implement your change in the relevant crate (`budi-core`, `budi-cli`, `budi-daemon`) or extension.
3. Run the local quality checks.
4. Open a pull request with test evidence and a short risk note.

## Local quality checks

### Rust workspace

```bash
cargo fmt --all
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

### Dashboard frontend (`frontend/dashboard`)

```bash
cd frontend/dashboard
npm install
npm run build
```

`npm run build` compiles the React app and writes static assets to `crates/budi-daemon/static/dashboard-dist`, which are embedded/served by `budi-daemon`.

From repo root you can also run:

```bash
./scripts/build-dashboard.sh
```

One-liner to rebuild dashboard assets and run daemon locally (foreground):

```bash
(cd frontend/dashboard && npm ci && npm run build) && CARGO_INCREMENTAL=0 cargo run -p budi-daemon -- serve
```

`CARGO_INCREMENTAL=0` avoids noisy incremental-cache warnings on some machines.

For local dashboard UI development (hot reload + API proxy):

```bash
# terminal A (repo root)
cargo run -p budi-daemon

# terminal B
cd frontend/dashboard
npm install
npm run dev
```

### Cursor extension

```bash
cd extensions/cursor-budi
npm ci
npm run lint
npm run format:check
npm run test
npm run build
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
pkill -f "budi-daemon serve"   # graceful stop (avoid -9 unless stuck)
budi init               # restarts daemon + re-syncs data
```

## Filing issues and feature requests

Use GitHub Issues:

- **Bug report**: include expected behavior, actual behavior, reproduction steps, and environment details.
- **Feature request**: include problem statement, proposed change, alternatives considered, and success criteria.

Issue templates are available in the repository to keep reports actionable.

## Pull request checklist

- [ ] Change is scoped and described clearly.
- [ ] `cargo fmt`, `clippy`, and tests pass locally.
- [ ] Dashboard frontend build passes (`cd frontend/dashboard && npm run build`) if dashboard code changed.
- [ ] Extension lint/format/test/build checks pass if extension code changed.
- [ ] Docs were updated for user-visible behavior changes.
- [ ] Migration or compatibility impact is noted (if relevant).

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
2. `enrich(&mut self, msg: &mut ParsedMessage) -> Vec<Tag>` - mutate the message and/or return tags
3. Register in `Pipeline::new()` in `crates/budi-core/src/pipeline/mod.rs`
4. Enricher order matters: Hook -> Identity -> Git -> Cost -> Tag

## Testing MCP server

```bash
# Send initialize + tools/list via stdin:
printf '{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}\n{"jsonrpc":"2.0","method":"notifications/initialized"}\n{"jsonrpc":"2.0","method":"tools/list","id":2}\n' | cargo run --bin budi -- mcp-serve 2>/dev/null
```

The MCP server uses stdio (stdout = JSON-RPC, stderr = logging). It's a thin HTTP client to the daemon - make sure `budi-daemon` is running first.

## Releasing

```bash
./scripts/release.sh 7.0.0        # bump version + update Cargo.lock
./scripts/release.sh 7.0.0 --tag  # also create git tag

# Then:
git commit -am "chore: bump version to 7.0.0"
git push origin main v7.0.0       # CI builds release artifacts
```
