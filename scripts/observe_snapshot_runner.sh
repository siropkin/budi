#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT=""
OUT_DIR=""
WINDOW_DAYS=1
BUDI_BIN="${BUDI_BIN:-$(command -v budi || true)}"

usage() {
  cat <<'EOF'
Usage: scripts/observe_snapshot_runner.sh [options]

Generate periodic observe snapshots for one repo.

Options:
  --repo-root <path>     Target repository root (required)
  --out-dir <path>       Output directory (default: ~/.local/share/budi/observe-snapshots/<repo-id>)
  --window-days <n>      Rolling window for summary snapshot (default: 1)
  --budi-bin <path>      budi binary path (default: resolved from PATH)
  -h, --help             Show this help
EOF
}

fail() {
  printf '[budi-observe-runner] ERROR: %s\n' "$*" >&2
  exit 1
}

log() {
  printf '[budi-observe-runner] %s\n' "$*"
}

parse_args() {
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --repo-root)
        [[ $# -ge 2 ]] || fail "--repo-root requires a value"
        REPO_ROOT="$2"
        shift 2
        ;;
      --out-dir)
        [[ $# -ge 2 ]] || fail "--out-dir requires a value"
        OUT_DIR="$2"
        shift 2
        ;;
      --window-days)
        [[ $# -ge 2 ]] || fail "--window-days requires a value"
        WINDOW_DAYS="$2"
        shift 2
        ;;
      --budi-bin)
        [[ $# -ge 2 ]] || fail "--budi-bin requires a value"
        BUDI_BIN="$2"
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

slugify() {
  local raw="$1"
  raw="${raw,,}"
  raw="$(echo "$raw" | tr -c 'a-z0-9_-.' '-')"
  raw="$(echo "$raw" | sed -E 's/-+/-/g; s/^-+//; s/-+$//')"
  if [[ -z "$raw" ]]; then
    raw="repo"
  fi
  printf '%s' "$raw"
}

main() {
  parse_args "$@"

  [[ -n "$REPO_ROOT" ]] || fail "--repo-root is required"
  [[ -n "$BUDI_BIN" ]] || fail "Unable to locate budi binary; pass --budi-bin"
  [[ -x "$BUDI_BIN" ]] || fail "budi binary is not executable: $BUDI_BIN"
  [[ -d "$REPO_ROOT" ]] || fail "Repo root does not exist: $REPO_ROOT"
  [[ -e "$REPO_ROOT/.git" ]] || fail "Repo root is not a git repo: $REPO_ROOT"
  [[ "$WINDOW_DAYS" =~ ^[0-9]+$ ]] || fail "--window-days must be an integer"
  (( WINDOW_DAYS >= 1 )) || fail "--window-days must be >= 1"

  REPO_ROOT="$(cd "$REPO_ROOT" && pwd)"

  if [[ -z "$OUT_DIR" ]]; then
    local repo_name repo_hash
    repo_name="$(slugify "$(basename "$REPO_ROOT")")"
    repo_hash="$(printf '%s' "$REPO_ROOT" | shasum -a 256 | awk '{print $1}' | cut -c1-12)"
    OUT_DIR="$HOME/.local/share/budi/observe-snapshots/${repo_name}-${repo_hash}"
  fi
  mkdir -p "$OUT_DIR"

  local ts all_json day_json summary_txt
  ts="$(date -u +"%Y%m%dT%H%M%SZ")"
  all_json="$OUT_DIR/all-${ts}.json"
  day_json="$OUT_DIR/days${WINDOW_DAYS}-${ts}.json"
  summary_txt="$OUT_DIR/summary-days${WINDOW_DAYS}-${ts}.txt"

  "$BUDI_BIN" observe report --all --json --repo-root "$REPO_ROOT" --out "$all_json" >/dev/null
  "$BUDI_BIN" observe report --days "$WINDOW_DAYS" --json --repo-root "$REPO_ROOT" --out "$day_json" >/dev/null
  "$BUDI_BIN" observe report --days "$WINDOW_DAYS" --repo-root "$REPO_ROOT" --out "$summary_txt" >/dev/null

  cp -f "$all_json" "$OUT_DIR/latest-all.json"
  cp -f "$day_json" "$OUT_DIR/latest-days${WINDOW_DAYS}.json"
  cp -f "$summary_txt" "$OUT_DIR/latest-summary.txt"

  log "Snapshot generated for $REPO_ROOT"
  log "Saved: $all_json"
  log "Saved: $day_json"
  log "Saved: $summary_txt"
}

main "$@"
