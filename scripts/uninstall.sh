#!/usr/bin/env bash
set -euo pipefail

PREFIX="${PREFIX:-$HOME/.local}"
BIN_DIR="${BIN_DIR:-$PREFIX/bin}"
KILL_DAEMONS=1

usage() {
  cat <<'EOF'
Usage: scripts/uninstall.sh [options]

Remove installed budi binaries, backup files, and LaunchAgents.

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

  # Run `budi uninstall --yes` first if available — removes hooks, statusline, config, and data.
  if command -v budi >/dev/null 2>&1; then
    log "Running budi uninstall to remove hooks, status line, and data..."
    budi uninstall --yes 2>/dev/null || true
  fi

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

  local bins=(budi budi-daemon budi-bench)
  for bin in "${bins[@]}"; do
    local target="$BIN_DIR/$bin"
    if [[ -e "$target" ]]; then
      rm -f "$target"
      log "Removed $target"
    else
      log "Not found (skip): $target"
    fi
    # Remove any timestamped backup files (e.g. budi.bak.20260302124939)
    local baks=("$BIN_DIR/$bin".bak.*)
    for bak in "${baks[@]}"; do
      [[ -e "$bak" ]] || continue
      rm -f "$bak"
      log "Removed $bak"
    done
  done

  # Remove budi LaunchAgents
  local launch_agents_dir="$HOME/Library/LaunchAgents"
  if [[ -d "$launch_agents_dir" ]]; then
    local plist
    for plist in "$launch_agents_dir"/com.siropkin.budi.*.plist; do
      [[ -e "$plist" ]] || continue
      launchctl unload "$plist" 2>/dev/null || true
      rm -f "$plist"
      log "Removed LaunchAgent $plist"
    done
  fi

  log "Uninstall complete."
}

main "$@"
