# Contributing to budi

Thanks for helping improve budi.

## Prerequisites

- Rust stable toolchain (`rustup`, `cargo`)
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

### Cursor extension

See [`siropkin/budi-cursor`](https://github.com/siropkin/budi-cursor).

### Cloud (Next.js ingest API + dashboard)

See [`siropkin/budi-cloud`](https://github.com/siropkin/budi-cloud).

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
- **Cursor extension status stale/offline**: run `budi doctor`, then `Budi: Refresh Status` or reload Cursor window.

## Adding support for a new agent

New agents are supported via **proxy traffic classification** — no new `Provider` implementation needed for live data (see [ADR-0081](docs/adr/0081-product-contract-and-deprecation-policy.md)).

1. Update the agent compatibility matrix in [ADR-0082](docs/adr/0082-proxy-compatibility-matrix-and-gateway-contract.md)
2. If the agent uses an existing protocol family (OpenAI Chat Completions or Anthropic Messages), add the agent's env var / config key to the `budi launch` CLI wrapper in `crates/budi-cli/src/commands/launch.rs`
3. If the agent uses a new protocol family (e.g., Gemini), implement a new protocol handler in the proxy (`crates/budi-daemon/src/routes/proxy.rs`)
4. Add pricing data for the agent's models in `crates/budi-core/src/cost.rs`
5. Add onboarding instructions to README.md and update the supported agents table
6. Add tests

The existing `Provider` trait is retained only for historical import via `budi import`. Do not create new `Provider` implementations for live data ingestion.

## Adding a new enricher

1. Create a struct implementing `pipeline::Enricher` in `crates/budi-core/src/pipeline/enrichers.rs`
2. `enrich(&mut self, msg: &mut ParsedMessage) -> Vec<Tag>` - mutate the message and/or return tags
3. Register in `Pipeline::default_pipeline()` in `crates/budi-core/src/pipeline/mod.rs`
4. Enricher order matters: Identity -> Git -> Tool -> File -> Cost -> Tag (`FileEnricher` was added in R1.4 / #292; it runs after `GitEnricher` so cwd/repo-root are resolved and before `CostEnricher` so file-path tags are available to user rules)

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
