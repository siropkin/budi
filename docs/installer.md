# Installer Guide

## Scripts

- `scripts/install.sh`
- `scripts/uninstall.sh`

## Install

```bash
./scripts/install.sh
```

By default this:

1. ensures Rust toolchain is available (installs rustup if needed),
2. builds in `release` profile with `--locked` when `Cargo.lock` exists,
3. installs binaries into `~/.local/bin`.

Installed binaries:

- `budi`
- `budi-daemon`
- `budi-mcp`

## Install from prebuilt GitHub release

If you want the fastest setup (no local Rust build), install from release assets:

```bash
./scripts/install.sh --from-release
```

Install a specific release tag:

```bash
./scripts/install.sh --from-release --version v2.0.0
```

Custom repository (fork):

```bash
./scripts/install.sh --from-release --repo your-org/budi
```

Release-mode install uses `gh release download` and verifies checksums from `SHA256SUMS` when available.
If the repository is private, authenticate first with `gh auth login`.

## Useful options

```bash
./scripts/install.sh --profile dev
./scripts/install.sh --bin-dir "$HOME/.local/bin" --force
./scripts/install.sh --cargo-install
./scripts/install.sh --skip-build --bin-dir "$HOME/.local/bin"
./scripts/install.sh --from-release --version v2.0.0
```

## PATH troubleshooting

If installer warns that your bin dir is not in `PATH`, add the following to your shell profile:

```bash
export PATH="$HOME/.local/bin:$PATH"
```

Then restart the shell.

## Uninstall

```bash
./scripts/uninstall.sh
```

Default uninstall removes binaries from `~/.local/bin` and stops running `budi-daemon` processes.
