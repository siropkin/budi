# Issue #67 review: CI, release, and packaging pipeline

## What was reviewed

- CI workflow coverage and protection gates: `.github/workflows/ci.yml`
- Release workflow correctness and packaging: `.github/workflows/release.yml`, `.github/release.yml`
- Release helper and installer scripts: `scripts/release.sh`, `scripts/install.sh`, `scripts/uninstall.sh`
- Distribution handoff files: `homebrew/budi.rb`, `homebrew/setup-tap.sh`
- Maintainer docs: `CONTRIBUTING.md`

## Findings (highest impact first)

1. **Release preflight gap:** `scripts/release.sh` allowed version bumping/tag attempts from dirty worktrees, non-`main` branches, or duplicate tags, increasing the risk of inconsistent release history.
2. **Packaging correctness blind spot:** release workflow created archives but did not validate archive contents before upload/publish, so missing binaries/docs could slip through until user install time.
3. **Script quality coverage gap in CI:** CI validated Rust and extension flows but did not enforce syntax checks for release/install shell scripts, leaving regressions possible in critical install/release paths.
4. **Process documentation drift:** contributing docs had the minimal release sequence, but lacked explicit post-release verification and Homebrew handoff guidance for maintainers.

## Changes made

- Hardened `scripts/release.sh` with preflight checks:
  - fail on dirty working tree,
  - reject unknown flags,
  - require `main` when creating tags,
  - fail fast if tag already exists locally or on `origin`.
- Added CI shell syntax validation in `.github/workflows/ci.yml`:
  - `bash -n` across `scripts/*.sh` and `homebrew/setup-tap.sh`.
- Added release archive content validation in `.github/workflows/release.yml`:
  - verifies each packaged asset includes expected binaries plus `README.md` and `LICENSE` before upload.
- Expanded `CONTRIBUTING.md` release guidance with:
  - explicit clean-release workflow,
  - post-release validation commands,
  - expected artifact list,
  - Homebrew token/fallback maintenance notes.

## Risks / trade-offs

- New release-script guards are intentionally strict and may block ad-hoc release attempts from feature branches; this is a deliberate safety trade-off.
- Archive validation introduces a small amount of workflow runtime overhead in exchange for better release integrity.

## Follow-ups not included in this PR

- Add end-to-end install smoke tests against published release artifacts on all supported platforms after `publish-release` (currently CI install tests are source-build based).
- Add digest/signing provenance (e.g., artifact attestations and optional signature verification) to strengthen supply-chain trust beyond SHA256 checksums.
