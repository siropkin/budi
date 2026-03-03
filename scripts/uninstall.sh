#!/usr/bin/env bash
set -euo pipefail

PREFIX="${PREFIX:-$HOME/.local}"
BIN_DIR="${BIN_DIR:-$PREFIX/bin}"
KILL_DAEMONS=1

usage() {
  cat <<'EOF'
Usage: scripts/uninstall.sh [options]

Remove installed budi binaries.

Options:
  --prefix <dir>       Install prefix used during install (default: ~/.local)
  --bin-dir <dir>      Binary install directory (default: <prefix>/bin)
  --keep-daemons       Do not stop running budi-daemon processes
  -h, --help           Show this help
EOF
}

log() {
  printf '[budi-uninstall] %s\n' "$*"
}

parse_args() {
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --prefix)
        [[ $# -ge 2 ]] || { echo "Missing value for --prefix" >&2; exit 1; }
        PREFIX="$2"
        shift 2
        ;;
      --bin-dir)
        [[ $# -ge 2 ]] || { echo "Missing value for --bin-dir" >&2; exit 1; }
        BIN_DIR="$2"
        shift 2
        ;;
      --keep-daemons)
        KILL_DAEMONS=0
        shift
        ;;
      -h|--help)
        usage
        exit 0
        ;;
      *)
        echo "Unknown argument: $1" >&2
        exit 1
        ;;
    esac
  done
}

main() {
  parse_args "$@"

  if [[ "$KILL_DAEMONS" -eq 1 ]] && command -v pgrep >/dev/null 2>&1; then
    local pids
    pids="$(pgrep -f 'budi-daemon serve' || true)"
    if [[ -n "$pids" ]]; then
      while read -r pid; do
        [[ -n "$pid" ]] || continue
        kill "$pid" || true
        log "Stopped budi-daemon pid=$pid"
      done <<< "$pids"
    fi
  fi

  local bins=(budi budi-daemon)
  for bin in "${bins[@]}"; do
    local target="$BIN_DIR/$bin"
    if [[ -e "$target" ]]; then
      rm -f "$target"
      log "Removed $target"
    else
      log "Not found (skip): $target"
    fi
  done

  log "Uninstall complete."
}

main "$@"
