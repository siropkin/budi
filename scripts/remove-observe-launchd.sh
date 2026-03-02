#!/usr/bin/env bash
set -euo pipefail

LABEL=""
PLIST_PATH=""

usage() {
  cat <<'EOF'
Usage: scripts/remove-observe-launchd.sh [options]

Remove a previously configured budi observe LaunchAgent.

Options:
  --label <name>      Launchd label (required unless --plist is provided)
  --plist <path>      Explicit plist path
  -h, --help          Show this help
EOF
}

fail() {
  printf '[budi-observe-launchd-remove] ERROR: %s\n' "$*" >&2
  exit 1
}

log() {
  printf '[budi-observe-launchd-remove] %s\n' "$*"
}

parse_args() {
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --label)
        [[ $# -ge 2 ]] || fail "--label requires a value"
        LABEL="$2"
        shift 2
        ;;
      --plist)
        [[ $# -ge 2 ]] || fail "--plist requires a value"
        PLIST_PATH="$2"
        shift 2
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

  if [[ -z "$PLIST_PATH" ]]; then
    [[ -n "$LABEL" ]] || fail "Provide --label or --plist"
    PLIST_PATH="$HOME/Library/LaunchAgents/${LABEL}.plist"
  fi

  local uid
  uid="$(id -u)"
  if [[ -f "$PLIST_PATH" ]]; then
    launchctl bootout "gui/${uid}" "$PLIST_PATH" >/dev/null 2>&1 || true
    rm -f "$PLIST_PATH"
    log "Removed: $PLIST_PATH"
  else
    log "Not found (skip): $PLIST_PATH"
  fi
}

main "$@"
