#!/usr/bin/env bash
# End-to-end regression for issue #602: pin the vitals freshness contract.
#
# Two contracts live here:
#
#   1. Freshness — once an assistant turn lands in the JSONL transcript,
#      `GET /analytics/session-health` reflects the new `message_count`
#      within tail latency (≈ notify debounce + one SQLite write).
#      `session_health` reads `messages` directly, no rollup, so a real
#      lag would point at a tailer or pipeline regression.
#
#   2. Minimum-messages contract — the four vitals only score once a session
#      crosses per-vital sample thresholds (see SOUL.md "Vitals freshness
#      contract"). This script appends turns incrementally and asserts:
#        - 3 assistant messages → state=insufficient_data, tip surfaces the
#          message count (#602: a user wondering "why is N/A stuck?" must
#          see proof that data IS flowing during warm-up).
#        - 8 assistant messages → state=green, vitals scored.
#
# Failure of contract 1 means the live tail/query path stopped being fresh.
# Failure of contract 2 means the warm-up tip drifted away from the
# user-facing wording the issue asked for.

set -euo pipefail

# Strip ANSI color codes so any CLI output we grep stays predictable.
export NO_COLOR="${NO_COLOR:-1}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BUDI="$ROOT/target/release/budi"
BUDI_DAEMON="$ROOT/target/release/budi-daemon"

if [[ ! -x "$BUDI" || ! -x "$BUDI_DAEMON" ]]; then
  echo "error: release binaries not built. run \`cargo build --release\` first." >&2
  exit 2
fi

TMPDIR_ROOT="$(mktemp -d -t budi-e2e-602-XXXXXX)"
export HOME="$TMPDIR_ROOT"
mkdir -p "$HOME/.config/budi"

DAEMON_PORT=17602

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

echo "[e2e] HOME=$HOME"

CLAUDE_PROJECTS="$HOME/.claude/projects"
TRANSCRIPTS_DIR="$CLAUDE_PROJECTS/e2e-602"
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
SESSION_ID="e2e-602-$(date +%s)"
TRANSCRIPT="$TRANSCRIPTS_DIR/session-$SESSION_ID.jsonl"

# Seed N user/assistant turn pairs into the transcript file. Each call
# appends to the file the tailer is already watching, so it produces real
# notify events (matching how a live agent streams turns to disk).
append_turns() {
  local file="$1"
  local session="$2"
  local start_idx="$3"
  local count="$4"
  python3 - "$file" "$session" "$start_idx" "$count" <<'PY'
import datetime
import json
import sys

file_path, session, start_idx_s, count_s = sys.argv[1:]
start_idx = int(start_idx_s)
count = int(count_s)

now = datetime.datetime.now(datetime.timezone.utc)
with open(file_path, "a", encoding="utf-8") as f:
    for offset in range(count):
        idx = start_idx + offset
        # Each turn is ~1 minute apart so timestamps don't collide.
        user_ts = (now + datetime.timedelta(seconds=idx * 60)).isoformat(
            timespec="milliseconds"
        ).replace("+00:00", "Z")
        assistant_ts = (
            now + datetime.timedelta(seconds=idx * 60, milliseconds=200)
        ).isoformat(timespec="milliseconds").replace("+00:00", "Z")

        user = {
            "type": "user",
            "uuid": f"602-u-{idx}",
            "parentUuid": None,
            "isSidechain": False,
            "sessionId": session,
            "timestamp": user_ts,
            "cwd": "/tmp/e2e-602",
            "gitBranch": "v8/602-vitals-freshness",
            "message": {"role": "user", "content": f"turn {idx}"},
        }
        assistant = {
            "type": "assistant",
            "uuid": f"602-a-{idx}",
            "parentUuid": f"602-u-{idx}",
            "isSidechain": False,
            "sessionId": session,
            "timestamp": assistant_ts,
            "cwd": "/tmp/e2e-602",
            "gitBranch": "v8/602-vitals-freshness",
            "message": {
                "type": "message",
                "role": "assistant",
                "id": f"req-602-{idx}",
                "model": "claude-sonnet-4-6",
                "content": [{"type": "text", "text": "ok"}],
                "stop_reason": "end_turn",
                "usage": {
                    "input_tokens": 4000,
                    "output_tokens": 100,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 8000,
                },
            },
        }
        f.write(json.dumps(user, separators=(",", ":")) + "\n")
        f.write(json.dumps(assistant, separators=(",", ":")) + "\n")
PY
}

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

fetch_health() {
  curl -s --max-time 5 "http://127.0.0.1:$DAEMON_PORT/analytics/session-health?session_id=$SESSION_ID"
}

# ---------------------------------------------------------------------------
# Phase 1: 3 assistant turns → insufficient_data, tip surfaces the count.
# ---------------------------------------------------------------------------

echo "[e2e] phase 1: appending 3 assistant turns"
append_turns "$TRANSCRIPT" "$SESSION_ID" 0 3
wait_sql_eq "3" \
  "SELECT COUNT(*) FROM messages WHERE session_id='$SESSION_ID' AND role='assistant';" \
  "3 assistant rows after first append"

# Re-query session-health until it reflects the 3 new rows. This is the
# freshness assertion: the read path must see the writes.
HEALTH=""
for _ in {1..60}; do
  HEALTH="$(fetch_health)"
  COUNT="$(echo "$HEALTH" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("message_count",0))' 2>/dev/null || echo 0)"
  if [[ "$COUNT" == "3" ]]; then
    break
  fi
  sleep 0.1
done

echo "[e2e] phase 1 health envelope:"
echo "$HEALTH" | python3 -m json.tool

STATE="$(echo "$HEALTH" | python3 -c 'import json,sys;print(json.load(sys.stdin)["state"])')"
TIP="$(echo "$HEALTH" | python3 -c 'import json,sys;print(json.load(sys.stdin)["tip"])')"
COUNT="$(echo "$HEALTH" | python3 -c 'import json,sys;print(json.load(sys.stdin)["message_count"])')"

if [[ "$COUNT" != "3" ]]; then
  echo "[e2e] FAIL: expected message_count=3 from session-health, got $COUNT" >&2
  exit 1
fi
if [[ "$STATE" != "insufficient_data" ]]; then
  echo "[e2e] FAIL: expected state=insufficient_data with 3 messages, got '$STATE'" >&2
  exit 1
fi
# #602: the tip must include the assistant message count so the user
# sees data IS flowing during warm-up. The exact wording is asserted in
# the unit test; here we pin only the load-bearing bits so a copy-tweak
# doesn't double-cost a maintainer.
case "$TIP" in
  *"3 assistant messages"*"warm up"*) ;;
  *)
    echo "[e2e] FAIL: phase 1 tip did not surface the message count + warm-up hint" >&2
    echo "       got: $TIP" >&2
    exit 1
    ;;
esac
echo "[e2e] OK phase 1: state=insufficient_data, tip surfaces 3-message warm-up"

# ---------------------------------------------------------------------------
# Phase 2: append 5 more turns (total 8) → vitals score, state goes green.
# ---------------------------------------------------------------------------

echo "[e2e] phase 2: appending 5 more assistant turns (total 8)"
append_turns "$TRANSCRIPT" "$SESSION_ID" 3 5
wait_sql_eq "8" \
  "SELECT COUNT(*) FROM messages WHERE session_id='$SESSION_ID' AND role='assistant';" \
  "8 assistant rows after second append"

HEALTH=""
for _ in {1..60}; do
  HEALTH="$(fetch_health)"
  COUNT="$(echo "$HEALTH" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("message_count",0))' 2>/dev/null || echo 0)"
  if [[ "$COUNT" == "8" ]]; then
    break
  fi
  sleep 0.1
done

echo "[e2e] phase 2 health envelope:"
echo "$HEALTH" | python3 -m json.tool

STATE="$(echo "$HEALTH" | python3 -c 'import json,sys;print(json.load(sys.stdin)["state"])')"
COUNT="$(echo "$HEALTH" | python3 -c 'import json,sys;print(json.load(sys.stdin)["message_count"])')"
CONTEXT_DRAG_SCORED="$(echo "$HEALTH" | python3 -c '
import json, sys
d = json.load(sys.stdin)
v = d["vitals"]["context_drag"]
print("1" if v else "0")
')"
CACHE_EFF_SCORED="$(echo "$HEALTH" | python3 -c '
import json, sys
d = json.load(sys.stdin)
v = d["vitals"]["cache_efficiency"]
print("1" if v else "0")
')"

if [[ "$COUNT" != "8" ]]; then
  echo "[e2e] FAIL: expected message_count=8 from session-health, got $COUNT" >&2
  exit 1
fi
if [[ "$STATE" != "green" ]]; then
  echo "[e2e] FAIL: expected state=green with 8 stable messages, got '$STATE'" >&2
  echo "$HEALTH" | python3 -m json.tool >&2
  exit 1
fi
if [[ "$CONTEXT_DRAG_SCORED" != "1" ]]; then
  echo "[e2e] FAIL: expected context_drag to score with 8 messages" >&2
  exit 1
fi
if [[ "$CACHE_EFF_SCORED" != "1" ]]; then
  echo "[e2e] FAIL: expected cache_efficiency to score with 8 messages" >&2
  exit 1
fi
echo "[e2e] OK phase 2: state=green, context_drag and cache_efficiency scored"

echo "[e2e] PASS"
