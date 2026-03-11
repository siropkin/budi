#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

PREFIX="${PREFIX:-$HOME/.local}"
BIN_DIR="${BIN_DIR:-$PREFIX/bin}"
PROFILE="${PROFILE:-release}"
RELEASE_REPO="${RELEASE_REPO:-siropkin/budi}"
RELEASE_TAG="${RELEASE_TAG:-}"
USE_CARGO_INSTALL=0
SKIP_BUILD=0
FORCE=0
NO_PATH_WARN=0
FROM_RELEASE=0

usage() {
  cat <<'EOF'
Usage: scripts/install.sh [options]

Install budi binaries from source or from GitHub releases.

Options:
  --prefix <dir>          Install prefix (default: ~/.local)
  --bin-dir <dir>         Binary install directory (default: <prefix>/bin)
  --profile <name>        Cargo profile: release or dev (default: release)
  --from-release          Download prebuilt binaries from GitHub release assets
  --repo <owner/repo>     Release repository (default: siropkin/budi)
  --version <tag>         Release tag to install (default: latest release)
  --cargo-install         Install via cargo install (into cargo bin dir)
  --skip-build            Skip build step and only copy existing binaries
  --force                 Overwrite existing binaries without backup
  --no-path-warn          Suppress PATH guidance output
  -h, --help              Show this help

Environment overrides:
  PREFIX, BIN_DIR, PROFILE, RELEASE_REPO, RELEASE_TAG
EOF
}

log() {
  printf '[budi-install] %s\n' "$*"
}

fail() {
  printf '[budi-install] ERROR: %s\n' "$*" >&2
  exit 1
}

ensure_cmd() {
  command -v "$1" >/dev/null 2>&1 || fail "Missing required command: $1"
}

sha256_of_file() {
  local file="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | awk '{print $1}'
  else
    shasum -a 256 "$file" | awk '{print $1}'
  fi
}

detect_target_triple() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"

  case "$arch" in
    x86_64|amd64) arch="x86_64" ;;
    arm64|aarch64) arch="aarch64" ;;
    *) fail "Unsupported architecture for release installs: $arch" ;;
  esac

  case "$os" in
    Linux) echo "${arch}-unknown-linux-gnu" ;;
    Darwin) echo "${arch}-apple-darwin" ;;
    *) fail "Unsupported OS for release installs: $os" ;;
  esac
}

backup_if_exists() {
  local target="$1"
  if [[ -e "$target" ]]; then
    if [[ "$FORCE" -eq 1 ]]; then
      return
    fi
    local backup="${target}.bak.$(date +%Y%m%d%H%M%S)"
    cp "$target" "$backup"
    log "Backed up existing binary: $target -> $backup"
  fi
}

ensure_cargo() {
  if command -v cargo >/dev/null 2>&1; then
    return
  fi

  ensure_cmd curl
  log "cargo not found; installing Rust toolchain via rustup"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y

  # shellcheck source=/dev/null
  . "$HOME/.cargo/env"
  command -v cargo >/dev/null 2>&1 || fail "cargo still unavailable after rustup install"
}


install_binaries_from_dir() {
  local src_dir="$1"
  mkdir -p "$BIN_DIR"

  local bins=(budi budi-daemon budi-mcp)
  for bin in "${bins[@]}"; do
    local src="$src_dir/$bin"
    local dst="$BIN_DIR/$bin"
    [[ -x "$src" ]] || fail "Missing binary: $src"
    backup_if_exists "$dst"
    install -m 0755 "$src" "$dst"
    log "Installed $bin -> $dst"
  done
}

verify_binaries() {
  "$BIN_DIR/budi" --version >/dev/null
  "$BIN_DIR/budi-daemon" --version >/dev/null
  [[ -x "$BIN_DIR/budi-mcp" ]] || log "Note: budi-mcp not found (optional MCP server)"
}

install_from_release() {
  ensure_cmd gh
  ensure_cmd tar

  local target asset_name temp_dir package_dir expected actual
  target="$(detect_target_triple)"

  if [[ -z "$RELEASE_TAG" ]]; then
    RELEASE_TAG="$(gh release view --repo "$RELEASE_REPO" --json tagName --jq '.tagName')" || \
      fail "Unable to determine latest release tag from $RELEASE_REPO"
  fi

  asset_name="budi-${RELEASE_TAG}-${target}.tar.gz"
  temp_dir="$(mktemp -d)"
  trap "rm -rf \"$temp_dir\"" EXIT

  log "Downloading release $RELEASE_TAG asset: $asset_name"
  gh release download "$RELEASE_TAG" \
    --repo "$RELEASE_REPO" \
    --pattern "$asset_name" \
    --dir "$temp_dir" || fail "Failed to download release asset via gh (try source install without --from-release)"
  gh release download "$RELEASE_TAG" \
    --repo "$RELEASE_REPO" \
    --pattern "SHA256SUMS" \
    --dir "$temp_dir" >/dev/null 2>&1 || true

  if [[ -f "$temp_dir/SHA256SUMS" ]]; then
    expected="$(awk -v file="$asset_name" '$2 == file {print $1}' "$temp_dir/SHA256SUMS")"
    [[ -n "$expected" ]] || fail "Checksum for $asset_name not found in SHA256SUMS"
    actual="$(sha256_of_file "$temp_dir/$asset_name")"
    [[ "$expected" == "$actual" ]] || fail "Checksum mismatch for $asset_name"
    log "Checksum verified for $asset_name"
  else
    log "SHA256SUMS not found for release; skipping checksum verification"
  fi

  tar -xzf "$temp_dir/$asset_name" -C "$temp_dir"
  package_dir="$temp_dir/budi-${RELEASE_TAG}-${target}"
  [[ -d "$package_dir" ]] || fail "Unexpected package layout in $asset_name"

  install_binaries_from_dir "$package_dir"
}

parse_args() {
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --prefix)
        [[ $# -ge 2 ]] || fail "--prefix requires a value"
        PREFIX="$2"
        shift 2
        ;;
      --bin-dir)
        [[ $# -ge 2 ]] || fail "--bin-dir requires a value"
        BIN_DIR="$2"
        shift 2
        ;;
      --profile)
        [[ $# -ge 2 ]] || fail "--profile requires a value"
        PROFILE="$2"
        shift 2
        ;;
      --from-release)
        FROM_RELEASE=1
        shift
        ;;
      --repo)
        [[ $# -ge 2 ]] || fail "--repo requires a value"
        RELEASE_REPO="$2"
        shift 2
        ;;
      --version)
        [[ $# -ge 2 ]] || fail "--version requires a value"
        RELEASE_TAG="$2"
        shift 2
        ;;
      --cargo-install)
        USE_CARGO_INSTALL=1
        shift
        ;;
      --skip-build)
        SKIP_BUILD=1
        shift
        ;;
      --force)
        FORCE=1
        shift
        ;;
      --no-path-warn)
        NO_PATH_WARN=1
        shift
        ;;
      -h|--help)
        usage
        exit 0
        ;;
      *)
        fail "Unknown argument: $1"
        ;;
    esac
  done
}

main() {
  parse_args "$@"

  [[ "$PROFILE" == "release" || "$PROFILE" == "dev" ]] || fail "--profile must be release or dev"

  if [[ "$FROM_RELEASE" -eq 1 ]]; then
    [[ "$USE_CARGO_INSTALL" -eq 0 ]] || fail "--from-release cannot be used with --cargo-install"
    [[ "$SKIP_BUILD" -eq 0 ]] || fail "--from-release cannot be used with --skip-build"
    install_from_release
  else
    ensure_cargo
    if [[ "$USE_CARGO_INSTALL" -eq 1 ]]; then
      log "Installing via cargo install"
      local lock_args=()
      if [[ -f "$REPO_ROOT/Cargo.lock" ]]; then
        lock_args+=(--locked)
      fi
      cargo install --path "$REPO_ROOT/crates/budi-cli" --bin budi --force "${lock_args[@]}"
      cargo install --path "$REPO_ROOT/crates/budi-daemon" --bin budi-daemon --force "${lock_args[@]}"
      cargo install --path "$REPO_ROOT/crates/budi-mcp" --bin budi-mcp --force "${lock_args[@]}"
      BIN_DIR="${CARGO_HOME:-$HOME/.cargo}/bin"
    else
      if [[ "$SKIP_BUILD" -eq 0 ]]; then
        log "Building workspace with cargo ($PROFILE profile)"
        local lock_args=()
        if [[ -f "$REPO_ROOT/Cargo.lock" ]]; then
          lock_args+=(--locked)
        fi
        cargo build --manifest-path "$REPO_ROOT/Cargo.toml" --profile "$PROFILE" "${lock_args[@]}"
      fi
      install_binaries_from_dir "$REPO_ROOT/target/$PROFILE"
    fi
  fi

  verify_binaries

  if [[ ":$PATH:" != *":$BIN_DIR:"* && "$NO_PATH_WARN" -eq 0 ]]; then
    cat <<EOF
[budi-install] NOTE: $BIN_DIR is not in your PATH.
[budi-install] Add this to your shell profile:
  export PATH="$BIN_DIR:\$PATH"
EOF
  fi

  cat <<EOF
[budi-install] Installation complete.
[budi-install] Binary directory: $BIN_DIR
[budi-install] Next step:
  cd /path/to/your/repo && budi init --index
EOF
}

main "$@"
