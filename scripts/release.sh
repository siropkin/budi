#!/usr/bin/env bash
# Release script: bumps Cargo workspace version, updates Cargo.lock,
# and optionally creates the git tag.
#
# Usage:
#   ./scripts/release.sh 4.1.0          # bump & validate only
#   ./scripts/release.sh 4.1.0 --tag    # also create git tag v4.1.0
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

log()  { printf '[budi-release] %s\n' "$*"; }
fail() { printf '[budi-release] ERROR: %s\n' "$*" >&2; exit 1; }

VERSION="${1:-}"
CREATE_TAG="${2:-}"

if [[ -z "$VERSION" ]]; then
  cat <<'EOF'
Usage: scripts/release.sh <version> [--tag]

Bumps version across:
  - Cargo.toml (workspace)
  - Cargo.lock (via cargo check)

Pass --tag to also create git tag v<version>.
EOF
  exit 1
fi

if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  fail "Version must use semantic version format X.Y.Z (got: $VERSION)"
fi

if [[ -n "$CREATE_TAG" && "$CREATE_TAG" != "--tag" ]]; then
  fail "Unknown flag: $CREATE_TAG (expected --tag)"
fi

cd "$REPO_ROOT"

if [[ -n "$(git status --porcelain)" ]]; then
  fail "Working tree is not clean. Commit/stash changes before running release."
fi

current_branch="$(git rev-parse --abbrev-ref HEAD)"
if [[ "$CREATE_TAG" == "--tag" && "$current_branch" != "main" ]]; then
  fail "Refusing to create release tag from branch '$current_branch' (expected main)"
fi

# 1. Bump workspace Cargo.toml version
log "Bumping Cargo.toml workspace version to $VERSION"
sed -i.bak -E "s/^version = \"[0-9]+\.[0-9]+\.[0-9]+\"/version = \"$VERSION\"/" Cargo.toml
rm -f Cargo.toml.bak

# 2. Update Cargo.lock
log "Updating Cargo.lock"
cargo check --workspace --quiet 2>/dev/null || cargo check --workspace

# 3. Summary
log "Version bump complete: $VERSION"
log "Files modified:"
git diff --name-only

# 4. Optionally tag
if [[ "$CREATE_TAG" == "--tag" ]]; then
  if git rev-parse -q --verify "refs/tags/v$VERSION" >/dev/null; then
    fail "Tag v$VERSION already exists locally"
  fi
  if git ls-remote --exit-code --tags origin "refs/tags/v$VERSION" >/dev/null 2>&1; then
    fail "Tag v$VERSION already exists on origin"
  fi
  log "Creating git tag v$VERSION"
  git tag "v$VERSION"
  log "Tag v$VERSION created. Push with: git push origin v$VERSION"
else
  cat <<EOF

[budi-release] Next steps:
  1) Review changes: git diff
  2) Commit: git commit -am "chore: bump version to $VERSION"
  3) Tag: git tag v$VERSION
  4) Push: git push origin main v$VERSION
EOF
fi
