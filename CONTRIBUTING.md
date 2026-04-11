# Contributing to budi

Thanks for helping improve budi.

## Prerequisites

- Rust stable toolchain (`rustup`, `cargo`)
- Node.js 20+ and npm (for dashboard and Cursor extension work)
- `gh` CLI (required when validating release-install flows with `scripts/install.sh --from-release`)

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

Run checks only for the area you changed, plus any shared Rust code impacted by your change.

### Rust workspace

```bash
cargo fmt --all
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

To mirror CI exactly for formatting, use:

```bash
cargo fmt --all -- --check
```

### Dashboard frontend (`frontend/dashboard`)

```bash
cd frontend/dashboard
npm ci
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
cargo run -p budi-daemon -- serve

# terminal B
cd frontend/dashboard
npm ci
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

If scripts are blocked (for example by anti-virus), install to Cargo bin (`~/.cargo/bin`):

```bash
cargo install --path crates/budi-cli --bin budi --force --locked
cargo install --path crates/budi-daemon --bin budi-daemon --force --locked
budi --version
budi init
```

Or build and copy binaries manually:

```bash
cargo build --release --locked
mkdir -p ~/.local/bin
cp target/release/budi target/release/budi-daemon ~/.local/bin/
chmod +x ~/.local/bin/budi ~/.local/bin/budi-daemon
rehash
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
- [ ] Follow-up work is captured explicitly (issue or PR TODO) if not included in this PR.
- [ ] PR links the driving issue (`Closes #...` or equivalent) when applicable.

## PR review expectations

Use findings-first PR descriptions so reviewers can quickly assess risk:

1. What area you reviewed or changed
2. What you changed and why
3. Risks/compatibility notes and any deferred follow-ups
4. Validation evidence (commands run + pass/fail)

If a review issue leads to "no code changes needed", still include a small artifact (for example a docs note, checklist update, or review report in `docs/reviews/`) so the decision is auditable.

## Contributor troubleshooting quick hits

- **`budi` and `budi-daemon` mismatch**: keep one install source on `PATH`; run `budi doctor`.
- **Dashboard looks stale after frontend edits**: rebuild via `./scripts/build-dashboard.sh`, then restart daemon.
- **Cursor extension status stale/offline**: run `budi doctor`, then `Budi: Refresh Status` or reload Cursor window.

## Adding a new provider

1. Create `crates/budi-core/src/providers/<name>.rs`
2. Implement the `Provider` trait: `name()`, `display_name()`, `is_available()`, `discover_files()`, `parse_file()`
3. Optionally implement `sync_direct()` for API-based data sources (like Cursor Usage API)
4. Add a pricing function `<name>_pricing_for_model(model: &str) -> ModelPricing`
5. Register in `crate::provider::all_providers()`
6. Add proxy/onboarding integration steps in `crates/budi-cli/src/commands/init.rs` if the agent needs setup automation
7. Add tests

## Adding a new enricher

1. Create a struct implementing `pipeline::Enricher` in `crates/budi-core/src/pipeline/enrichers.rs`
2. `enrich(&mut self, msg: &mut ParsedMessage) -> Vec<Tag>` - mutate the message and/or return tags
3. Register in `Pipeline::default_pipeline()` in `crates/budi-core/src/pipeline/mod.rs`
4. Enricher order matters: Identity -> Git -> Tool -> Cost -> Tag

## Releasing

Release automation is tag-driven (`.github/workflows/release.yml`) and expects a clean `main` branch.

```bash
./scripts/release.sh 7.0.0        # bump version + update Cargo.lock (clean tree required)
git commit -am "chore: bump version to 7.0.0"
./scripts/release.sh 7.0.0 --tag  # create tag v7.0.0 (main only; refuses duplicate tags)
git push origin main v7.0.0       # CI + release workflows build and publish assets
```

Post-push release checks:

```bash
gh release view v7.0.0 --repo siropkin/budi
gh release download v7.0.0 --repo siropkin/budi --pattern SHA256SUMS -D /tmp/budi-release-check
cat /tmp/budi-release-check/SHA256SUMS
```

Expected release artifacts:

- `budi-v<version>-x86_64-unknown-linux-gnu.tar.gz`
- `budi-v<version>-aarch64-unknown-linux-gnu.tar.gz`
- `budi-v<version>-x86_64-apple-darwin.tar.gz`
- `budi-v<version>-aarch64-apple-darwin.tar.gz`
- `budi-v<version>-x86_64-pc-windows-msvc.zip`
- `SHA256SUMS`

Homebrew auto-update notes:

- The release workflow updates `siropkin/homebrew-budi` after publishing assets.
- `HOMEBREW_TAP_TOKEN` must be configured in `siropkin/budi` repo secrets.
- If the workflow cannot push the formula update, run `homebrew/setup-tap.sh <tag>` manually and open a follow-up PR/issue with the failure details.
