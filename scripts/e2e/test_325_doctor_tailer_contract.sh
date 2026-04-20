#!/usr/bin/env bash
# End-to-end regression for issue #325: verify `budi doctor` reports the
# tailer-first 8.2 contract with actionable FAIL/WARN hints.
#
# Contract pinned:
# - #316 R2.4 (`#325`) doctor is organized around daemon health, schema drift,
#   transcript visibility, and leftover proxy residue
# - Legacy proxy-reachability checks do not come back
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BUDI="$ROOT/target/release/budi"

if [[ ! -x "$BUDI" ]]; then
  echo "error: release binary not built. run \`cargo build --release\` first." >&2
  exit 2
fi

TMPDIR_ROOT="$(mktemp -d -t budi-e2e-325-XXXXXX)"
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

echo "[e2e] seed schema without starting daemon"
"$BUDI" init --no-daemon >/dev/null

mkdir -p "$HOME/.claude/projects/demo"
printf '{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}\n' \
  >"$HOME/.claude/projects/demo/session.jsonl"

python3 - <<'PY'
import os
import sqlite3

db_path = os.path.join(os.environ["BUDI_HOME"], "analytics.db")
conn = sqlite3.connect(db_path)
conn.execute("PRAGMA user_version = 0")
conn.commit()
conn.close()
PY

echo "[e2e] run doctor with fail modes injected"
LOG="$TMPDIR_ROOT/doctor.log"
set +e
ANTHROPIC_BASE_URL="http://127.0.0.1:9878" \
BUDI_DAEMON_BIN="/definitely/missing/budi-daemon" \
"$BUDI" doctor >"$LOG" 2>&1
STATUS=$?
set -e

if [[ $STATUS -eq 0 ]]; then
  cat "$LOG" >&2 || true
  echo "[e2e] FAIL: doctor was expected to fail" >&2
  exit 1
fi

assert_contains "$LOG" "FAIL daemon health:"
assert_contains "$LOG" "BUDI_DAEMON_BIN"
assert_contains "$LOG" "FAIL schema drift:"
assert_contains "$LOG" "Run \`budi init\` or \`budi update\`"
assert_contains "$LOG" "WARN leftover proxy config:"
assert_contains "$LOG" "budi init --cleanup"
assert_contains "$LOG" "FAIL transcript visibility / Claude Code:"
assert_contains "$LOG" "Run \`budi db import\` if you also need older history backfilled."
assert_contains "$LOG" "doctor found"

echo "[e2e] PASS"
