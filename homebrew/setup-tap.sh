#!/usr/bin/env bash
# Bootstrap script to create the siropkin/homebrew-budi GitHub repo.
#
# Prerequisites:
#   - gh CLI authenticated (`gh auth login`)
#   - SHA256SUMS from the latest release
#
# Usage:
#   ./homebrew/setup-tap.sh
#
# After running, add a HOMEBREW_TAP_TOKEN secret to the siropkin/budi repo:
#   1. Create a fine-grained PAT at https://github.com/settings/tokens
#      - Scope: siropkin/homebrew-budi, permissions: Contents (read/write)
#   2. Add it as a secret: gh secret set HOMEBREW_TAP_TOKEN -R siropkin/budi
set -euo pipefail

REPO="siropkin/homebrew-budi"
TAG="${1:-$(gh release view --repo siropkin/budi --json tagName -q .tagName)}"
VER="${TAG#v}"

log() { printf '[homebrew-setup] %s\n' "$*"; }

log "Setting up tap for budi ${TAG}..."

# Download checksums
TEMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TEMP_DIR"' EXIT
curl -fsSL "https://github.com/siropkin/budi/releases/download/${TAG}/SHA256SUMS" -o "$TEMP_DIR/sums"

AARCH64_DARWIN=$(awk '/aarch64-apple-darwin\.tar\.gz$/ {print $1}' "$TEMP_DIR/sums")
X86_64_DARWIN=$(awk '/x86_64-apple-darwin\.tar\.gz$/ {print $1}' "$TEMP_DIR/sums")
X86_64_LINUX=$(awk '/x86_64-unknown-linux-gnu\.tar\.gz$/ {print $1}' "$TEMP_DIR/sums")
AARCH64_LINUX=$(awk '/aarch64-unknown-linux-gnu\.tar\.gz$/ {print $1}' "$TEMP_DIR/sums")

# Render formula
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
sed \
  -e "s/{{VERSION}}/${VER}/g" \
  -e "s/{{SHA256_AARCH64_DARWIN}}/${AARCH64_DARWIN}/g" \
  -e "s/{{SHA256_X86_64_DARWIN}}/${X86_64_DARWIN}/g" \
  -e "s/{{SHA256_X86_64_LINUX}}/${X86_64_LINUX}/g" \
  -e "s/{{SHA256_AARCH64_LINUX}}/${AARCH64_LINUX}/g" \
  "${SCRIPT_DIR}/budi.rb" > "$TEMP_DIR/budi.rb"

log "Rendered formula:"
cat "$TEMP_DIR/budi.rb"
echo

# Create repo if it doesn't exist
if gh repo view "$REPO" &>/dev/null; then
  log "Repo $REPO already exists"
else
  log "Creating $REPO..."
  gh repo create "$REPO" --public --description "Homebrew tap for budi — AI cost analytics for coding agents"
fi

# Clone, add formula, push
git clone "https://github.com/${REPO}.git" "$TEMP_DIR/tap"
mkdir -p "$TEMP_DIR/tap/Formula"
cp "$TEMP_DIR/budi.rb" "$TEMP_DIR/tap/Formula/budi.rb"

# Create a minimal README
cat > "$TEMP_DIR/tap/README.md" << 'EOF'
# homebrew-budi

Homebrew tap for [budi](https://github.com/siropkin/budi) — local-first cost analytics for AI coding agents.

## Install

```bash
brew install siropkin/budi/budi
```

## Update

```bash
brew upgrade budi
```
EOF

cd "$TEMP_DIR/tap"
git add -A
git commit -m "budi ${TAG}" || { log "Nothing to commit"; exit 0; }
git push

log ""
log "Done! Users can now install with:"
log "  brew install siropkin/budi/budi"
log ""
log "Next step: add a HOMEBREW_TAP_TOKEN secret to siropkin/budi for auto-updates."
log "  1. Create a fine-grained PAT: https://github.com/settings/tokens"
log "     Scope: siropkin/homebrew-budi, Permissions: Contents (read/write)"
log "  2. gh secret set HOMEBREW_TAP_TOKEN -R siropkin/budi"
