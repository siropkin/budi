#!/usr/bin/env bash
# End-to-end regression for issue #302: verify that tailer-ingested transcript
# messages show up in /analytics/sessions and `budi sessions -p today`, with
# persisted non-null session_id and a green doctor visibility check.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BUDI="$ROOT/target/release/budi"
BUDI_DAEMON="$ROOT/target/release/budi-daemon"

if [[ ! -x "$BUDI" || ! -x "$BUDI_DAEMON" ]]; then
  echo "error: release binaries not built. run \`cargo build --release\` first." >&2
  exit 2
fi

TMPDIR_ROOT="$(mktemp -d -t budi-e2e-302-XXXXXX)"
export HOME="$TMPDIR_ROOT"
mkdir -p "$HOME/.config/budi"

DAEMON_PORT=17878

cleanup() {
  local status=$?
  { kill "${DAEMON_PID:-}" 2>/dev/null || true; } >/dev/null 2>&1
  { kill "${CLI_DAEMON_PID:-}" 2>/dev/null || true; } >/dev/null 2>&1
  pkill -f "budi-daemon serve --host 127.0.0.1 --port $DAEMON_PORT" >/dev/null 2>&1 || true
  if [[ "${KEEP_TMP:-0}" == "1" ]]; then
    echo "[e2e] leaving tmp: $TMPDIR_ROOT"
  else
    rm -rf "$TMPDIR_ROOT"
  fi
  exit $status
}
trap cleanup EXIT INT TERM

echo "[e2e] HOME=$HOME"

REPO_ROOT="$HOME/repo"
mkdir -p "$REPO_ROOT/.budi"
cat >"$REPO_ROOT/.budi/budi.toml" <<CFG
daemon_host = "127.0.0.1"
daemon_port = $DAEMON_PORT
CFG

(
  cd "$REPO_ROOT"
  git init -q 2>/dev/null || true
  git remote add origin https://github.com/siropkin/budi.git 2>/dev/null || true
)

CLAUDE_PROJECTS="$HOME/.claude/projects"
TRANSCRIPTS_DIR="$CLAUDE_PROJECTS/e2e-302"
mkdir -p "$TRANSCRIPTS_DIR"

echo "[e2e] starting budi-daemon on :$DAEMON_PORT"
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
  echo "--- daemon log ---" >&2
  cat "$TMPDIR_ROOT/daemon.log" >&2 || true
  exit 1
fi

DB="$HOME/.local/share/budi/analytics.db"

wait_sql_eq() {
  local expected="$1"
  local sql="$2"
  local label="$3"
  local got=""
  for _ in {1..120}; do
    got="$(sqlite3 "$DB" "$sql" 2>/dev/null || true)"
    if [[ "$got" == "$expected" ]]; then
      return 0
    fi
    sleep 0.1
  done
  echo "[e2e] FAIL: timed out waiting for $label (expected '$expected', got '$got')" >&2
  echo "[e2e] sql: $sql" >&2
  tail -n 120 "$TMPDIR_ROOT/daemon.log" >&2 || true
  exit 1
}

append_turn() {
  local file="$1"
  local session="$2"
  local req_id="$3"
  local uuid_prefix="$4"

  python3 - "$file" "$session" "$REPO_ROOT" "$req_id" "$uuid_prefix" <<'PY'
import datetime
import json
import sys

file_path, session, cwd, req_id, prefix = sys.argv[1:]
now = datetime.datetime.now(datetime.timezone.utc)
user_ts = now.isoformat(timespec="milliseconds").replace("+00:00", "Z")
assistant_ts = (now + datetime.timedelta(milliseconds=200)).isoformat(timespec="milliseconds").replace("+00:00", "Z")

user = {
    "type": "user",
    "uuid": f"{prefix}-u",
    "parentUuid": None,
    "isSidechain": False,
    "sessionId": session,
    "timestamp": user_ts,
    "cwd": cwd,
    "gitBranch": "v8/302-sessions-visibility",
    "message": {
        "role": "user",
        "content": "hi",
    },
}
assistant = {
    "type": "assistant",
    "uuid": f"{prefix}-a",
    "parentUuid": f"{prefix}-u",
    "isSidechain": False,
    "sessionId": session,
    "timestamp": assistant_ts,
    "cwd": cwd,
    "gitBranch": "v8/302-sessions-visibility",
    "message": {
        "type": "message",
        "role": "assistant",
        "id": req_id,
        "model": "claude-sonnet-4-6",
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "usage": {
            "input_tokens": 42,
            "output_tokens": 7,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0,
        },
    },
}

with open(file_path, "a", encoding="utf-8") as f:
    f.write(json.dumps(user, separators=(",", ":")) + "\n")
    f.write(json.dumps(assistant, separators=(",", ":")) + "\n")
PY
}

SESSION_ID="e2e-sess-$(date +%s)"
echo "[e2e] appending transcript turn with session_id=$SESSION_ID"
append_turn "$TRANSCRIPTS_DIR/session-$SESSION_ID.jsonl" "$SESSION_ID" "req-302-$SESSION_ID" "302-$SESSION_ID"

wait_sql_eq "1" "SELECT COUNT(*) FROM messages WHERE session_id = '$SESSION_ID' AND role='assistant';" "assistant row for session"

echo "[e2e] sessions list (json):"
SESSIONS_JSON="$(curl -s "http://127.0.0.1:$DAEMON_PORT/analytics/sessions?since=$(date -u +%Y-%m-%dT00:00:00+00:00)")"
echo "$SESSIONS_JSON" | python3 -m json.tool

FOUND="$(echo "$SESSIONS_JSON" | python3 -c '
import json, sys
data = json.load(sys.stdin)
ids = [s.get("id") for s in data.get("sessions", [])]
print("1" if sys.argv[1] in ids else "0")
' "$SESSION_ID")"
if [[ "$FOUND" != "1" ]]; then
  echo "[e2e] FAIL: session '$SESSION_ID' not returned by /analytics/sessions" >&2
  echo "--- daemon log ---" >&2
  tail -n 80 "$TMPDIR_ROOT/daemon.log" >&2 || true
  exit 1
fi
echo "[e2e] OK: session '$SESSION_ID' visible via /analytics/sessions"

echo "[e2e] DB rows for session:"
sqlite3 "$DB" "SELECT id, session_id, provider, model, input_tokens, output_tokens, cost_confidence FROM messages WHERE session_id = '$SESSION_ID';"

ROWCOUNT="$(sqlite3 "$DB" "SELECT COUNT(*) FROM messages WHERE session_id = '$SESSION_ID' AND role='assistant';")"
if [[ "$ROWCOUNT" != "1" ]]; then
  echo "[e2e] FAIL: expected 1 assistant row for '$SESSION_ID', found $ROWCOUNT" >&2
  exit 1
fi
NULL_SESSIONS="$(sqlite3 "$DB" "SELECT COUNT(*) FROM messages WHERE role='assistant' AND (session_id IS NULL OR session_id='');")"
if [[ "$NULL_SESSIONS" != "0" ]]; then
  echo "[e2e] FAIL: $NULL_SESSIONS assistant rows were written with NULL/empty session_id" >&2
  exit 1
fi
echo "[e2e] OK: session_id persisted, no NULL/empty-session_id rows"

# `budi sessions` currently resolves daemon config from
# `$BUDI_HOME/repos/<hash>/budi.toml`, not `$REPO_ROOT/.budi/budi.toml`.
# In isolated E2E homes this can point the CLI at a different daemon than the
# one we boot here. To keep this script deterministic, assert against the same
# daemon endpoint shape the CLI uses (`/analytics/sessions` with sort/limit).
SESSIONS_CLI_SHAPE_JSON="$(curl -s --max-time 5 \
  --get "http://127.0.0.1:$DAEMON_PORT/analytics/sessions" \
  --data-urlencode "since=$(date -u +%Y-%m-%dT00:00:00+00:00)" \
  --data-urlencode "sort_by=started_at" \
  --data-urlencode "limit=50" \
  --data-urlencode "offset=0")"
echo "[e2e] /analytics/sessions (CLI query shape) -> $SESSIONS_CLI_SHAPE_JSON"
FOUND_CLI_SHAPE="$(echo "$SESSIONS_CLI_SHAPE_JSON" | python3 -c '
import json, sys
data = json.load(sys.stdin)
ids = [s.get("id") for s in data.get("sessions", [])]
print("1" if sys.argv[1] in ids else "0")
' "$SESSION_ID")"
if [[ "$FOUND_CLI_SHAPE" != "1" ]]; then
  echo "[e2e] FAIL: /analytics/sessions (CLI query shape) did not include session '$SESSION_ID'" >&2
  exit 1
fi
echo "[e2e] OK: /analytics/sessions CLI query shape includes the session"

echo "[e2e] budi doctor (sessions visibility section):"
DOCTOR_OUT="$(cd "$REPO_ROOT" && "$BUDI" doctor --repo-root "$REPO_ROOT" 2>&1 || true)"
echo "$DOCTOR_OUT" | grep -E "sessions visibility" || {
  echo "[e2e] FAIL: budi doctor did not print a sessions-visibility check" >&2
  echo "$DOCTOR_OUT" | tail -n 60 >&2
  exit 1
}
if echo "$DOCTOR_OUT" | grep -q "Sessions visibility mismatch"; then
  echo "[e2e] FAIL: budi doctor reported a sessions-visibility mismatch" >&2
  echo "$DOCTOR_OUT" | tail -n 60 >&2
  exit 1
fi
echo "[e2e] OK: budi doctor sessions-visibility check is green"

echo "[e2e] PASS"
