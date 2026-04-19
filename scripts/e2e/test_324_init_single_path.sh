#!/usr/bin/env bash
# End-to-end regression for issue #324: verify `budi init` now runs as a
# single-path, zero-prompt setup flow and reports detected transcript roots.
#
# Contract pinned:
# - #316 R2.3 (`#324`) simplified init flow
# - Detection comes from existing `Provider::watch_roots()` locations only
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BUDI="$ROOT/target/release/budi"

if [[ ! -x "$BUDI" ]]; then
  echo "error: release binary not built. run \`cargo build --release\` first." >&2
  exit 2
fi

TMPDIR_ROOT="$(mktemp -d -t budi-e2e-324-XXXXXX)"
export HOME="$TMPDIR_ROOT"
export BUDI_HOME="$HOME/.local/share/budi"

cleanup() {
  local status=$?
  if [[ "${KEEP_TMP:-0}" == "1" ]]; then
    echo "[e2e] leaving tmp: $TMPDIR_ROOT"
  else
    rm -rf "$TMPDIR_ROOT"
  fi
  exit $status
}
trap cleanup EXIT INT TERM

assert_contains() {
  local file="$1"
  local needle="$2"
  if ! grep -Fq "$needle" "$file"; then
    echo "[e2e] FAIL: expected '$needle' in $file" >&2
    cat "$file" >&2 || true
    exit 1
  fi
}

mkdir -p "$HOME/.claude/projects"
mkdir -p "$HOME/.codex/sessions"

echo "[e2e] first init run"
LOG1="$TMPDIR_ROOT/init-1.log"
"$BUDI" init --no-daemon >"$LOG1" 2>&1 || {
  cat "$LOG1" >&2 || true
  echo "[e2e] FAIL: first init run failed" >&2
  exit 1
}
assert_contains "$LOG1" "Detected agents:"
assert_contains "$LOG1" "Claude Code"
assert_contains "$LOG1" "Codex"
echo "[e2e] OK: detected-agent output reflects existing watch roots"

echo "[e2e] second init run (idempotence)"
LOG2="$TMPDIR_ROOT/init-2.log"
"$BUDI" init --no-daemon >"$LOG2" 2>&1 || {
  cat "$LOG2" >&2 || true
  echo "[e2e] FAIL: second init run failed" >&2
  exit 1
}
assert_contains "$LOG2" "Detected agents:"
assert_contains "$LOG2" "Claude Code"
assert_contains "$LOG2" "Codex"
echo "[e2e] OK: second init run stays no-prompt and stable"

echo "[e2e] PASS"
