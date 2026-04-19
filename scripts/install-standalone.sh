#!/usr/bin/env bash
# Standalone installer for budi — works without cloning the repo.
# Usage: curl -fsSL https://raw.githubusercontent.com/siropkin/budi/main/scripts/install-standalone.sh | bash
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
    Linux)
      # Detect musl-based systems (Alpine, Void, etc.) — prebuilt binaries require glibc.
      if [ -f /etc/alpine-release ] || (command -v ldd >/dev/null 2>&1 && ldd --version 2>&1 | grep -qi musl); then
        fail "musl libc detected. Prebuilt binaries require glibc. Install from source instead: https://github.com/siropkin/budi#install"
      fi
      echo "${arch}-unknown-linux-gnu"
      ;;
    Darwin) echo "${arch}-apple-darwin" ;;
    *)      fail "Unsupported OS: $os" ;;
  esac
}

TEMP_DIR=""
cleanup() { [ -n "$TEMP_DIR" ] && rm -rf "$TEMP_DIR"; }

restore_binaries() {
  local backup_dir="$1"
  local bin_dir="$2"
  shift 2
  for bin in "$@"; do
    rm -f "$bin_dir/$bin"
    if [ -f "$backup_dir/$bin.bak" ]; then
      cp "$backup_dir/$bin.bak" "$bin_dir/$bin"
      chmod 0755 "$bin_dir/$bin" || true
    fi
  done
}

main() {
  command -v curl >/dev/null 2>&1 || fail "curl is required"
  command -v tar >/dev/null 2>&1 || fail "tar is required"

  # Warn if budi is already installed via Homebrew (check binary path, fast).
  # Skip when called from `budi update --version` (BUDI_SKIP_INIT is set).
  local existing_budi
  existing_budi="$(command -v budi 2>/dev/null || true)"
  if [ -z "${BUDI_SKIP_INIT:-}" ] && [ -n "$existing_budi" ] && case "$existing_budi" in */Cellar/*|*/homebrew/*|*/Homebrew/*) true;; *) false;; esac; then
    log "WARNING: budi is already installed via Homebrew at $existing_budi."
    log "This will install a second copy in $BIN_DIR."
    log "Consider using 'budi update' instead."
    log ""
  fi

  local target tag
  target="$(detect_target)"

  # Resolve version tag.
  if [ -n "$VERSION" ]; then
    tag="$VERSION"
  else
    log "Fetching latest release tag..."
    local api_response
    api_response="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest")" \
      || fail "Could not fetch release info from GitHub (check network or try again later)"
    # Try jq first, fall back to python3, then grep.
    if command -v jq >/dev/null 2>&1; then
      tag="$(echo "$api_response" | jq -r '.tag_name')"
    elif command -v python3 >/dev/null 2>&1; then
      tag="$(echo "$api_response" | python3 -c "import json,sys; print(json.load(sys.stdin)['tag_name'])")"
    else
      tag="$(echo "$api_response" | grep '"tag_name"' | head -1 | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')"
    fi
    [ -n "$tag" ] || fail "Could not determine latest release tag"
  fi

  local asset_name="budi-${tag}-${target}.tar.gz"
  local base_url="https://github.com/$REPO/releases/download/$tag"
  TEMP_DIR="$(mktemp -d)"
  trap cleanup EXIT

  log "Downloading $asset_name ($tag)..."
  curl -fSL "$base_url/$asset_name" -o "$TEMP_DIR/$asset_name" \
    || fail "Download failed — check that a release asset exists for $target"

  # Verify checksum (required by default).
  if ! curl -fsSL "$base_url/SHA256SUMS" -o "$TEMP_DIR/SHA256SUMS" 2>/dev/null; then
    if [ "${BUDI_ALLOW_INSECURE_NO_CHECKSUM:-}" = "1" ]; then
      log "WARNING: checksum file unavailable — continuing due to BUDI_ALLOW_INSECURE_NO_CHECKSUM=1."
    else
      fail "Checksum file unavailable. Refusing insecure install. Set BUDI_ALLOW_INSECURE_NO_CHECKSUM=1 to override."
    fi
  fi
  if [ -f "$TEMP_DIR/SHA256SUMS" ]; then
    local expected actual
    expected="$(awk -v f="$asset_name" '$2 == f || $2 == "*" f {print $1}' "$TEMP_DIR/SHA256SUMS")"
    [ -n "$expected" ] || fail "Checksum for $asset_name not found in SHA256SUMS"
    actual="$(sha256_of_file "$TEMP_DIR/$asset_name")"
    [ "$expected" = "$actual" ] || fail "Checksum mismatch for $asset_name"
    log "Checksum verified."
  fi

  tar -xzf "$TEMP_DIR/$asset_name" -C "$TEMP_DIR"
  local pkg_dir="$TEMP_DIR/budi-${tag}-${target}"
  [ -d "$pkg_dir" ] || fail "Unexpected archive layout"

  mkdir -p "$BIN_DIR"
  local backup_dir="$TEMP_DIR/backup"
  mkdir -p "$backup_dir"

  for bin in budi budi-daemon; do
    [ -x "$pkg_dir/$bin" ] || fail "Missing binary in release archive: $bin"
    if [ -e "$BIN_DIR/$bin" ]; then
      cp "$BIN_DIR/$bin" "$backup_dir/$bin.bak" || fail "Failed to backup existing $bin"
    fi
  done

  for bin in budi budi-daemon; do
    local staged="$BIN_DIR/.${bin}.new.$$"
    cp "$pkg_dir/$bin" "$staged" || {
      restore_binaries "$backup_dir" "$BIN_DIR" budi budi-daemon
      fail "Failed to stage $bin for install"
    }
    chmod 0755 "$staged" || {
      rm -f "$staged"
      restore_binaries "$backup_dir" "$BIN_DIR" budi budi-daemon
      fail "Failed to set execute permission on staged $bin"
    }
    rm -f "$BIN_DIR/$bin" || {
      rm -f "$staged"
      restore_binaries "$backup_dir" "$BIN_DIR" budi budi-daemon
      fail "Failed to replace existing $bin"
    }
    mv "$staged" "$BIN_DIR/$bin" || {
      rm -f "$staged"
      restore_binaries "$backup_dir" "$BIN_DIR" budi budi-daemon
      fail "Failed to install $bin"
    }
    log "Installed $bin -> $BIN_DIR/$bin"
  done

  # Verify.
  "$BIN_DIR/budi" --version || {
    restore_binaries "$backup_dir" "$BIN_DIR" budi budi-daemon
    fail "Installed budi failed to run"
  }
  "$BIN_DIR/budi-daemon" --version || {
    restore_binaries "$backup_dir" "$BIN_DIR" budi budi-daemon
    fail "Installed budi-daemon failed to run"
  }

  # Auto-add BIN_DIR to PATH in shell profile if missing.
  if ! echo ":$PATH:" | grep -q ":$BIN_DIR:"; then
    local shell_profile=""
    local current_shell="${SHELL:-}"
    case "$current_shell" in
      */zsh)  shell_profile="$HOME/.zshrc" ;;
      */fish)
        # Fish uses a different syntax — handle separately.
        local fish_config="$HOME/.config/fish/config.fish"
        if ! grep -qF "$BIN_DIR" "$fish_config" 2>/dev/null; then
          mkdir -p "$(dirname "$fish_config")"
          printf '\n# Added by budi installer\nfish_add_path %s\n' "$BIN_DIR" >> "$fish_config"
          log "Added $BIN_DIR to PATH in $fish_config"
          log "Restart your terminal or run: source $fish_config"
        fi
        ;;
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
    elif [ "${current_shell##*/}" = "fish" ]; then
      : # Fish PATH was configured in the case block above — nothing to do here.
    else
      log ""
      log "NOTE: $BIN_DIR is not in your PATH."
      log "Add this to your shell profile:"
      log "  $path_line"
    fi
  fi

  # Ensure BIN_DIR is in PATH for the current shell so budi init works.
  export PATH="$BIN_DIR:$PATH"

  log ""
  log "Installed budi $tag ($target)"
  log ""

  # Skip init if called from `budi update` (update handles its own post-install sequence).
  if [ "${BUDI_SKIP_INIT:-}" = "1" ]; then
    log "Skipping init (update mode)."
  else
    # Auto-run budi init for a seamless setup experience.
    log "Running budi init..."
    log ""
    local init_rc=0
    "$BIN_DIR/budi" init || init_rc=$?
    if [ "$init_rc" -eq 0 ]; then
      log ""
      log "Setup complete! Run 'budi doctor' to verify end-to-end, then open your agent and send a prompt."
    elif [ "$init_rc" -eq 2 ]; then
      log ""
      log "Setup complete with warnings. Run 'budi doctor' to see what needs fixing."
    else
      fail "budi init failed (exit code $init_rc). Run 'budi doctor' to diagnose."
    fi
  fi
}

main
