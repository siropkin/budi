#!/usr/bin/env bash
# Standalone installer for budi — works without cloning the repo.
# Usage: curl -fsSL https://raw.githubusercontent.com/siropkin/budi/main/scripts/install-standalone.sh | sh
set -euo pipefail

REPO="siropkin/budi"
BIN_DIR="${BIN_DIR:-$HOME/.local/bin}"
VERSION="${VERSION:-}"

log() { printf '[budi-install] %s\n' "$*"; }
fail() { printf '[budi-install] ERROR: %s\n' "$*" >&2; exit 1; }

sha256_of_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

detect_target() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$arch" in
    x86_64|amd64) arch="x86_64" ;;
    arm64|aarch64) arch="aarch64" ;;
    *) fail "Unsupported architecture: $arch" ;;
  esac
  case "$os" in
    Linux)  echo "${arch}-unknown-linux-gnu" ;;
    Darwin) echo "${arch}-apple-darwin" ;;
    *)      fail "Unsupported OS: $os" ;;
  esac
}

TEMP_DIR=""
cleanup() { [ -n "$TEMP_DIR" ] && rm -rf "$TEMP_DIR"; }

main() {
  command -v curl >/dev/null 2>&1 || fail "curl is required"
  command -v tar >/dev/null 2>&1 || fail "tar is required"

  local target tag asset_url
  target="$(detect_target)"

  # Resolve version tag.
  if [ -n "$VERSION" ]; then
    tag="$VERSION"
  else
    log "Fetching latest release tag..."
    tag="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
      | grep '"tag_name"' | head -1 | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')" \
      || fail "Could not determine latest release"
  fi

  local asset_name="budi-${tag}-${target}.tar.gz"
  local base_url="https://github.com/$REPO/releases/download/$tag"
  TEMP_DIR="$(mktemp -d)"
  trap cleanup EXIT

  log "Downloading $asset_name ($tag)..."
  curl -fSL "$base_url/$asset_name" -o "$TEMP_DIR/$asset_name" \
    || fail "Download failed — check that a release asset exists for $target"

  # Verify checksum if available.
  if curl -fsSL "$base_url/SHA256SUMS" -o "$TEMP_DIR/SHA256SUMS" 2>/dev/null; then
    local expected actual
    expected="$(awk -v f="$asset_name" '$2 == f {print $1}' "$TEMP_DIR/SHA256SUMS")"
    if [ -n "$expected" ]; then
      actual="$(sha256_of_file "$TEMP_DIR/$asset_name")"
      [ "$expected" = "$actual" ] || fail "Checksum mismatch for $asset_name"
      log "Checksum verified."
    fi
  fi

  tar -xzf "$TEMP_DIR/$asset_name" -C "$TEMP_DIR"
  local pkg_dir="$TEMP_DIR/budi-${tag}-${target}"
  [ -d "$pkg_dir" ] || fail "Unexpected archive layout"

  mkdir -p "$BIN_DIR"
  for bin in budi budi-daemon; do
    if [ -x "$pkg_dir/$bin" ]; then
      install -m 0755 "$pkg_dir/$bin" "$BIN_DIR/$bin"
      log "Installed $bin -> $BIN_DIR/$bin"
    fi
  done

  # Verify.
  "$BIN_DIR/budi" --version || fail "Installed binary failed to run"

  # Auto-add BIN_DIR to PATH in shell profile if missing.
  if ! echo ":$PATH:" | grep -q ":$BIN_DIR:"; then
    local shell_profile=""
    local current_shell="${SHELL:-}"
    case "$current_shell" in
      */zsh)  shell_profile="$HOME/.zshrc" ;;
      */bash)
        if [ -f "$HOME/.bashrc" ]; then
          shell_profile="$HOME/.bashrc"
        else
          shell_profile="$HOME/.profile"
        fi
        ;;
      *)
        if [ -f "$HOME/.profile" ]; then
          shell_profile="$HOME/.profile"
        fi
        ;;
    esac

    local path_line="export PATH=\"$BIN_DIR:\$PATH\""
    if [ -n "$shell_profile" ]; then
      # Only append if the line isn't already there.
      if ! grep -qF "$BIN_DIR" "$shell_profile" 2>/dev/null; then
        printf '\n# Added by budi installer\n%s\n' "$path_line" >> "$shell_profile"
        log "Added $BIN_DIR to PATH in $shell_profile"
        log "Restart your terminal or run: source $shell_profile"
      fi
    else
      log ""
      log "NOTE: $BIN_DIR is not in your PATH."
      log "Add this to your shell profile:"
      log "  $path_line"
    fi
  fi

  log ""
  log "Installed budi $tag ($target)"
  log ""
  log "Get started:"
  log "  budi init --global  # set up hooks globally (all repos and worktrees)"
  log "  budi doctor      # verify everything is working"
  log "  budi stats       # view usage analytics"
}

main
