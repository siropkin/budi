#!/usr/bin/env bash
# End-to-end regression for issue #326: verify an 8.1-shaped analytics DB
# upgrades cleanly by dropping the obsolete `proxy_events` table while keeping
# proxy-sourced `messages` rows queryable for stats and visible in `budi doctor`.
#
# Contract pinned:
# - #316 R2.5 (`#326`) keeps legacy proxy-sourced history read-only in
#   `messages` while removing the dead proxy-only table on upgrade
# - `budi doctor` reports retained legacy proxy history honestly without
#   pretending the old proxy runtime is still part of the live path
set -euo pipefail

# Strip ANSI color codes from captured output so doctor / CLI
# strings can be grep\'d without escape-sequence mismatches.
# Callers can force color back on with `NO_COLOR=0 bash scripts/...`.
export NO_COLOR="${NO_COLOR:-1}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BUDI="$ROOT/target/release/budi"
BUDI_DAEMON="$ROOT/target/release/budi-daemon"

if [[ ! -x "$BUDI" || ! -x "$BUDI_DAEMON" ]]; then
  echo "error: release binaries not built. run \`cargo build --release\` first." >&2
  exit 2
fi

TMPDIR_ROOT="$(mktemp -d -t budi-e2e-326-XXXXXX)"
export HOME="$TMPDIR_ROOT"
export BUDI_HOME="$HOME/.local/share/budi"

DAEMON_PORT=17879
DB="$BUDI_HOME/analytics.db"
REPO_ROOT="$HOME/repo"

cleanup() {
  local status=$?
  { kill "${DAEMON_PID:-}" 2>/dev/null || true; } >/dev/null 2>&1
  pkill -f "budi-daemon serve --host 127.0.0.1 --port $DAEMON_PORT" >/dev/null 2>&1 || true
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

assert_not_contains() {
  local file="$1"
  local needle="$2"
  if grep -Fq "$needle" "$file"; then
    echo "[e2e] FAIL: did not expect '$needle' in $file" >&2
    cat "$file" >&2 || true
    exit 1
  fi
}

mkdir -p "$REPO_ROOT/.budi"
cat >"$REPO_ROOT/.budi/budi.toml" <<CFG
daemon_host = "127.0.0.1"
daemon_port = $DAEMON_PORT
CFG

(
  cd "$REPO_ROOT"
  git init -q 2>/dev/null || true
)

echo "[e2e] create fresh 8.2 database"
"$BUDI" init --no-daemon >/dev/null

echo "[e2e] inject 8.1-style proxy residue"
python3 - <<'PY'
import datetime
import os
import sqlite3

db_path = os.path.join(os.environ["BUDI_HOME"], "analytics.db")
conn = sqlite3.connect(db_path)
conn.execute(
    """
    CREATE TABLE proxy_events (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        timestamp TEXT NOT NULL,
        provider TEXT,
        model TEXT,
        input_tokens INTEGER NOT NULL DEFAULT 0,
        output_tokens INTEGER NOT NULL DEFAULT 0,
        cost_cents REAL
    )
    """
)
timestamp = datetime.datetime.now(datetime.timezone.utc).replace(microsecond=0)
raw_ts = timestamp.isoformat().replace("+00:00", "Z")
conn.execute(
    """
    INSERT INTO proxy_events (timestamp, provider, model, input_tokens, output_tokens, cost_cents)
    VALUES (?, 'openai', 'gpt-4o', 42, 7, 0.5)
    """,
    (raw_ts,),
)
conn.execute(
    """
    INSERT INTO messages (
        id, role, timestamp, model, provider, input_tokens, output_tokens,
        cache_creation_tokens, cache_read_tokens, cost_cents, cost_confidence
    ) VALUES (
        'legacy-proxy-message', 'assistant', ?, 'gpt-4o', 'openai', 42, 7, 0, 0, 0.5, 'proxy_estimated'
    )
    """,
    (raw_ts,),
)
conn.commit()
conn.close()
PY

if [[ "$(sqlite3 "$DB" "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='proxy_events';")" != "1" ]]; then
  echo "[e2e] FAIL: expected proxy_events table before migration" >&2
  exit 1
fi

echo "[e2e] rerun init to apply 8.2 cleanup migration"
INIT_LOG="$TMPDIR_ROOT/init-upgrade.log"
"$BUDI" init --no-daemon >"$INIT_LOG" 2>&1 || {
  cat "$INIT_LOG" >&2 || true
  echo "[e2e] FAIL: upgrade init failed" >&2
  exit 1
}

if [[ "$(sqlite3 "$DB" "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='proxy_events';")" != "0" ]]; then
  echo "[e2e] FAIL: proxy_events table should be removed after migration" >&2
  sqlite3 "$DB" "SELECT name FROM sqlite_master WHERE type='table';" >&2 || true
  exit 1
fi
if [[ "$(sqlite3 "$DB" "SELECT COUNT(*) FROM messages WHERE cost_confidence='proxy_estimated';")" != "1" ]]; then
  echo "[e2e] FAIL: expected retained proxy_estimated message after migration" >&2
  sqlite3 "$DB" "SELECT id, cost_confidence FROM messages;" >&2 || true
  exit 1
fi

echo "[e2e] start daemon on :$DAEMON_PORT"
RUST_LOG=info \
  "$BUDI_DAEMON" serve \
    --host 127.0.0.1 \
    --port $DAEMON_PORT \
    >"$TMPDIR_ROOT/daemon.log" 2>&1 &
DAEMON_PID=$!

DAEMON_UP=0
for _ in {1..50}; do
  if curl -s -o /dev/null -w "%{http_code}" --max-time 1 "http://127.0.0.1:$DAEMON_PORT/health" | grep -q "^200"; then
    DAEMON_UP=1
    break
  fi
  sleep 0.1
done
if [[ "$DAEMON_UP" != "1" ]]; then
  echo "[e2e] FAIL: daemon did not come up on :$DAEMON_PORT" >&2
  cat "$TMPDIR_ROOT/daemon.log" >&2 || true
  exit 1
fi

echo "[e2e] analytics endpoints still see retained legacy proxy history"
SUMMARY_JSON="$TMPDIR_ROOT/summary.json"
COST_JSON="$TMPDIR_ROOT/cost.json"
curl -fsS "http://127.0.0.1:$DAEMON_PORT/analytics/summary?since=$(date -u +%Y-%m-%dT00:00:00+00:00)" >"$SUMMARY_JSON"
curl -fsS "http://127.0.0.1:$DAEMON_PORT/analytics/cost?since=$(date -u +%Y-%m-%dT00:00:00+00:00)" >"$COST_JSON"
python3 - "$SUMMARY_JSON" "$COST_JSON" <<'PY'
import json
import sys

with open(sys.argv[1], "r", encoding="utf-8") as f:
    summary = json.load(f)
with open(sys.argv[2], "r", encoding="utf-8") as f:
    cost = json.load(f)

assert summary["total_messages"] == 1, summary
assert summary["total_assistant_messages"] == 1, summary
assert cost["total_cost"] > 0, cost
PY

echo "[e2e] doctor reports retained legacy proxy history without stale-table residue"
DOCTOR_LOG="$TMPDIR_ROOT/doctor.log"
(
  cd "$REPO_ROOT"
  "$BUDI" doctor --repo-root "$REPO_ROOT" >"$DOCTOR_LOG" 2>&1 || true
)
assert_contains "$DOCTOR_LOG" "PASS legacy proxy history:"
assert_contains "$DOCTOR_LOG" "retaining 1 proxy-sourced assistant row read-only"
assert_not_contains "$DOCTOR_LOG" "obsolete \`proxy_events\` table is still present"

echo "[e2e] PASS"
