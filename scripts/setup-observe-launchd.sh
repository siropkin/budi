#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUNNER_SCRIPT="$SCRIPT_DIR/observe_snapshot_runner.sh"

REPO_ROOT=""
OUT_DIR=""
INTERVAL_SECS=21600
WINDOW_DAYS=1
LABEL=""
BUDI_BIN="${BUDI_BIN:-$(command -v budi || true)}"

usage() {
  cat <<'EOF'
Usage: scripts/setup-observe-launchd.sh [options]

Set up launchd to generate periodic budi observe snapshots for a repo.

Options:
  --repo-root <path>       Target repository root (required)
  --interval-secs <n>      Snapshot cadence in seconds (default: 21600 = 6h)
  --window-days <n>        Rolling window for daily snapshot (default: 1)
  --out-dir <path>         Snapshot output directory (default under ~/.local/share/budi/observe-snapshots)
  --label <name>           Custom launchd label (default: derived from repo)
  --budi-bin <path>        budi binary path (default: resolved from PATH)
  -h, --help               Show this help

Notes:
  - This script enables observe logging for the repo automatically.
  - It creates/updates a LaunchAgent plist under ~/Library/LaunchAgents.
EOF
}

fail() {
  printf '[budi-observe-launchd] ERROR: %s\n' "$*" >&2
  exit 1
}

log() {
  printf '[budi-observe-launchd] %s\n' "$*"
}

parse_args() {
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --repo-root)
        [[ $# -ge 2 ]] || fail "--repo-root requires a value"
        REPO_ROOT="$2"
        shift 2
        ;;
      --interval-secs)
        [[ $# -ge 2 ]] || fail "--interval-secs requires a value"
        INTERVAL_SECS="$2"
        shift 2
        ;;
      --window-days)
        [[ $# -ge 2 ]] || fail "--window-days requires a value"
        WINDOW_DAYS="$2"
        shift 2
        ;;
      --out-dir)
        [[ $# -ge 2 ]] || fail "--out-dir requires a value"
        OUT_DIR="$2"
        shift 2
        ;;
      --label)
        [[ $# -ge 2 ]] || fail "--label requires a value"
        LABEL="$2"
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
  raw="$(printf '%s' "$raw" | tr '[:upper:]' '[:lower:]')"
  raw="$(echo "$raw" | tr -c 'a-z0-9_-' '-')"
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
  [[ -f "$RUNNER_SCRIPT" ]] || fail "Runner script missing: $RUNNER_SCRIPT"
  [[ -d "$REPO_ROOT" ]] || fail "Repo root does not exist: $REPO_ROOT"
  [[ -e "$REPO_ROOT/.git" ]] || fail "Repo root is not a git repo: $REPO_ROOT"

  [[ "$INTERVAL_SECS" =~ ^[0-9]+$ ]] || fail "--interval-secs must be an integer"
  [[ "$WINDOW_DAYS" =~ ^[0-9]+$ ]] || fail "--window-days must be an integer"
  (( INTERVAL_SECS >= 300 )) || fail "--interval-secs must be >= 300"
  (( WINDOW_DAYS >= 1 )) || fail "--window-days must be >= 1"

  REPO_ROOT="$(cd "$REPO_ROOT" && pwd)"
  local repo_name repo_hash
  repo_name="$(slugify "$(basename "$REPO_ROOT")")"
  repo_hash="$(printf '%s' "$REPO_ROOT" | shasum -a 256 | awk '{print $1}' | cut -c1-12)"

  if [[ -z "$OUT_DIR" ]]; then
    OUT_DIR="$HOME/.local/share/budi/observe-snapshots/${repo_name}-${repo_hash}"
  fi
  mkdir -p "$OUT_DIR"

  if [[ -z "$LABEL" ]]; then
    LABEL="com.siropkin.budi.observe.${repo_name}.${repo_hash}"
  fi

  local launch_agents_dir plist_path uid
  launch_agents_dir="$HOME/Library/LaunchAgents"
  plist_path="$launch_agents_dir/${LABEL}.plist"
  uid="$(id -u)"
  mkdir -p "$launch_agents_dir"

  "$BUDI_BIN" observe enable --repo-root "$REPO_ROOT" >/dev/null
  /bin/bash "$RUNNER_SCRIPT" \
    --repo-root "$REPO_ROOT" \
    --out-dir "$OUT_DIR" \
    --window-days "$WINDOW_DAYS" \
    --budi-bin "$BUDI_BIN" >/dev/null

  cat > "$plist_path" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
  <dict>
    <key>Label</key>
    <string>${LABEL}</string>
    <key>ProgramArguments</key>
    <array>
      <string>/bin/bash</string>
      <string>${RUNNER_SCRIPT}</string>
      <string>--repo-root</string>
      <string>${REPO_ROOT}</string>
      <string>--out-dir</string>
      <string>${OUT_DIR}</string>
      <string>--window-days</string>
      <string>${WINDOW_DAYS}</string>
      <string>--budi-bin</string>
      <string>${BUDI_BIN}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>StartInterval</key>
    <integer>${INTERVAL_SECS}</integer>
    <key>StandardOutPath</key>
    <string>${OUT_DIR}/launchd.stdout.log</string>
    <key>StandardErrorPath</key>
    <string>${OUT_DIR}/launchd.stderr.log</string>
  </dict>
</plist>
EOF

  launchctl bootout "gui/${uid}" "$plist_path" >/dev/null 2>&1 || true
  launchctl bootstrap "gui/${uid}" "$plist_path"
  launchctl kickstart -k "gui/${uid}/${LABEL}"

  log "LaunchAgent installed: $plist_path"
  log "Label: $LABEL"
  log "Interval: ${INTERVAL_SECS}s"
  log "Output dir: $OUT_DIR"
  log "To remove: /bin/bash \"$SCRIPT_DIR/remove-observe-launchd.sh\" --label \"$LABEL\""
}

main "$@"
