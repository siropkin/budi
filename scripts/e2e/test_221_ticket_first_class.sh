#!/usr/bin/env bash
# End-to-end regression for issue #221 (R1.3): verify ticket extraction and
# ticket_source semantics via the 8.2 live path (filesystem tailer), not proxy
# ingest.
#
# What this script guards (tailer-first):
# - Alphanumeric branch tickets (e.g. PROJ-221-foo) land as
#   ticket_id=PROJ-221 + ticket_prefix=PROJ + ticket_source=branch.
# - Numeric-only branch tickets (e.g. feature/1234) land as
#   ticket_id=1234 + ticket_source=branch_numeric + NO ticket_prefix.
# - Integration branches (main) emit no ticket tags.
# - No legacy "Unassigned" ticket_id sentinel appears.
# - /analytics/tickets and /analytics/tickets/{id} expose dominant source.
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

TMPDIR_ROOT="$(mktemp -d -t budi-e2e-221-XXXXXX)"
export HOME="$TMPDIR_ROOT"
mkdir -p "$HOME/.config/budi"

DAEMON_PORT=17881

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

# Tailer setup: create watch root before daemon start.
CLAUDE_PROJECTS="$HOME/.claude/projects"
TRANSCRIPTS_DIR="$CLAUDE_PROJECTS/e2e-221"
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
  local branch="$3"
  local prompt="$4"
  local req_id="$5"
  local uuid_prefix="$6"

  python3 - "$file" "$session" "$branch" "$REPO_ROOT" "$prompt" "$req_id" "$uuid_prefix" <<'PY'
import datetime
import json
import sys

file_path, session, branch, cwd, prompt, req_id, prefix = sys.argv[1:]
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
    "gitBranch": branch,
    "message": {
        "role": "user",
        "content": prompt,
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
    "gitBranch": branch,
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

TS="$(date +%s)"
ALPHA_SESSION="e2e-221-alpha-$TS"
NUM_SESSION="e2e-221-numeric-$TS"
MAIN_SESSION="e2e-221-main-$TS"

append_turn "$TRANSCRIPTS_DIR/alpha.jsonl"   "$ALPHA_SESSION" "PROJ-221-ticket-attribution" "fix login ticket" "req-221-alpha-$TS" "alpha-$TS"
append_turn "$TRANSCRIPTS_DIR/numeric.jsonl" "$NUM_SESSION"   "feature/1234"               "fix numeric ticket" "req-221-num-$TS"   "num-$TS"
append_turn "$TRANSCRIPTS_DIR/main.jsonl"    "$MAIN_SESSION"  "main"                       "update docs" "req-221-main-$TS"  "main-$TS"

wait_sql_eq "1" "SELECT COUNT(*) FROM messages WHERE session_id = '$ALPHA_SESSION' AND role='assistant';" "alpha assistant row"
wait_sql_eq "1" "SELECT COUNT(*) FROM messages WHERE session_id = '$NUM_SESSION' AND role='assistant';" "numeric assistant row"
wait_sql_eq "1" "SELECT COUNT(*) FROM messages WHERE session_id = '$MAIN_SESSION' AND role='assistant';" "main assistant row"

tag_value() {
  local mid="$1"; local key="$2"
  sqlite3 "$DB" "SELECT value FROM tags WHERE message_id = '$mid' AND key = '$key' LIMIT 1;"
}

tag_count() {
  local mid="$1"; local key="$2"
  sqlite3 "$DB" "SELECT COUNT(*) FROM tags WHERE message_id = '$mid' AND key = '$key';"
}

MSG_ALPHA="$(sqlite3 "$DB" "SELECT id FROM messages WHERE session_id = '$ALPHA_SESSION' AND role='assistant' ORDER BY timestamp DESC LIMIT 1;")"
if [[ -z "$MSG_ALPHA" ]]; then
  echo "[e2e] FAIL: no assistant row for alpha session" >&2
  exit 1
fi

TID_A="$(tag_value "$MSG_ALPHA" ticket_id)"
TPREF_A="$(tag_value "$MSG_ALPHA" ticket_prefix)"
TSRC_A="$(tag_value "$MSG_ALPHA" ticket_source)"
echo "[e2e] alpha row tags: ticket_id=$TID_A ticket_prefix=$TPREF_A ticket_source=$TSRC_A"

if [[ "$TID_A" != "PROJ-221" ]]; then
  echo "[e2e] FAIL: expected ticket_id=PROJ-221 on alpha row, got '$TID_A'" >&2
  sqlite3 "$DB" "SELECT key, value FROM tags WHERE message_id = '$MSG_ALPHA';" >&2
  exit 1
fi
if [[ "$TPREF_A" != "PROJ" ]]; then
  echo "[e2e] FAIL: expected ticket_prefix=PROJ on alpha row, got '$TPREF_A'" >&2
  exit 1
fi
if [[ "$TSRC_A" != "branch" ]]; then
  echo "[e2e] FAIL: expected ticket_source=branch on alpha row, got '$TSRC_A'" >&2
  exit 1
fi
echo "[e2e] OK: alpha branch -> ticket_id/prefix/source all present and correct"

MSG_NUM="$(sqlite3 "$DB" "SELECT id FROM messages WHERE session_id = '$NUM_SESSION' AND role='assistant' ORDER BY timestamp DESC LIMIT 1;")"
if [[ -z "$MSG_NUM" ]]; then
  echo "[e2e] FAIL: no assistant row for numeric session" >&2
  exit 1
fi

TID_N="$(tag_value "$MSG_NUM" ticket_id)"
TSRC_N="$(tag_value "$MSG_NUM" ticket_source)"
TPREF_N_COUNT="$(tag_count "$MSG_NUM" ticket_prefix)"
echo "[e2e] numeric row tags: ticket_id=$TID_N ticket_source=$TSRC_N ticket_prefix_count=$TPREF_N_COUNT"

if [[ "$TID_N" != "1234" ]]; then
  echo "[e2e] FAIL: expected ticket_id=1234 on numeric row, got '$TID_N'" >&2
  sqlite3 "$DB" "SELECT key, value FROM tags WHERE message_id = '$MSG_NUM';" >&2
  exit 1
fi
if [[ "$TSRC_N" != "branch_numeric" ]]; then
  echo "[e2e] FAIL: expected ticket_source=branch_numeric on numeric row, got '$TSRC_N'" >&2
  exit 1
fi
if [[ "$TPREF_N_COUNT" != "0" ]]; then
  echo "[e2e] FAIL: expected no ticket_prefix on numeric-only ticket, got $TPREF_N_COUNT" >&2
  exit 1
fi
echo "[e2e] OK: numeric branch -> ticket_id=1234 source=branch_numeric, no prefix"

MSG_MAIN="$(sqlite3 "$DB" "SELECT id FROM messages WHERE session_id = '$MAIN_SESSION' AND role='assistant' ORDER BY timestamp DESC LIMIT 1;")"
TID_M_COUNT="$(tag_count "$MSG_MAIN" ticket_id)"
TSRC_M_COUNT="$(tag_count "$MSG_MAIN" ticket_source)"
echo "[e2e] main row tag counts: ticket_id=$TID_M_COUNT ticket_source=$TSRC_M_COUNT"
if [[ "$TID_M_COUNT" != "0" ]]; then
  echo "[e2e] FAIL: 'main' should never emit a ticket_id tag, got $TID_M_COUNT" >&2
  sqlite3 "$DB" "SELECT key, value FROM tags WHERE message_id = '$MSG_MAIN';" >&2
  exit 1
fi
if [[ "$TSRC_M_COUNT" != "0" ]]; then
  echo "[e2e] FAIL: 'main' should never emit a ticket_source tag, got '$TSRC_M_COUNT'" >&2
  exit 1
fi
echo "[e2e] OK: integration branch 'main' emits no ticket_* tags"

UNASSIGNED="$(sqlite3 "$DB" "SELECT COUNT(*) FROM tags WHERE key = 'ticket_id' AND value = 'Unassigned';")"
if [[ "$UNASSIGNED" != "0" ]]; then
  echo "[e2e] FAIL: 'Unassigned' ticket_id sentinel leaked into DB ($UNASSIGNED rows)" >&2
  sqlite3 "$DB" "SELECT message_id, key, value FROM tags WHERE key = 'ticket_id' AND value = 'Unassigned';" >&2
  exit 1
fi
echo "[e2e] OK: no 'Unassigned' ticket_id rows (legacy sentinel retired)"

SINCE_TS="$(date -u -v0H -v0M -v0S +%Y-%m-%dT%H:%M:%SZ 2>/dev/null \
           || date -u --date="today 00:00:00" +%Y-%m-%dT%H:%M:%SZ)"
TICKETS_JSON="$(curl -s --max-time 5 \
  "http://127.0.0.1:$DAEMON_PORT/analytics/tickets?limit=20&since=$SINCE_TS")"
echo "[e2e] /analytics/tickets -> $TICKETS_JSON"

python3 - "$TICKETS_JSON" <<'PY'
import json, sys

data = json.loads(sys.argv[1])
by_id = {row["ticket_id"]: row for row in data}

missing = [t for t in ("PROJ-221", "1234") if t not in by_id]
if missing:
    print(f"[e2e] FAIL: /analytics/tickets missing rows for: {missing}")
    print(f"       saw: {sorted(by_id.keys())}")
    sys.exit(1)

alpha = by_id["PROJ-221"]
num = by_id["1234"]

if alpha.get("source") != "branch":
    print(f"[e2e] FAIL: PROJ-221 source should be 'branch', got {alpha.get('source')!r}")
    sys.exit(1)
if num.get("source") != "branch_numeric":
    print(f"[e2e] FAIL: 1234 source should be 'branch_numeric', got {num.get('source')!r}")
    sys.exit(1)

if "Unassigned" in by_id:
    print("[e2e] FAIL: /analytics/tickets surfaced a legacy 'Unassigned' bucket")
    sys.exit(1)
if "(untagged)" not in by_id:
    print(f"[e2e] FAIL: /analytics/tickets missing '(untagged)' bucket; saw {sorted(by_id.keys())}")
    sys.exit(1)
if by_id["(untagged)"].get("source"):
    print(f"[e2e] FAIL: (untagged) row should have empty source, got {by_id['(untagged)'].get('source')!r}")
    sys.exit(1)

print(f"[e2e] OK: /analytics/tickets carries sources "
      f"(PROJ-221={alpha['source']}, 1234={num['source']}, "
      f"(untagged)={by_id['(untagged)'].get('source')!r})")
PY

DETAIL_ALPHA="$(curl -s --max-time 5 \
  "http://127.0.0.1:$DAEMON_PORT/analytics/tickets/PROJ-221?since=$SINCE_TS")"
DETAIL_NUM="$(curl -s --max-time 5 \
  "http://127.0.0.1:$DAEMON_PORT/analytics/tickets/1234?since=$SINCE_TS")"
echo "[e2e] /analytics/tickets/PROJ-221 -> $DETAIL_ALPHA"
echo "[e2e] /analytics/tickets/1234 -> $DETAIL_NUM"

python3 - "$DETAIL_ALPHA" "$DETAIL_NUM" <<'PY'
import json, sys

alpha = json.loads(sys.argv[1])
num = json.loads(sys.argv[2])

if alpha.get("source") != "branch":
    print(f"[e2e] FAIL: PROJ-221 detail source should be 'branch', got {alpha.get('source')!r}")
    sys.exit(1)
if num.get("source") != "branch_numeric":
    print(f"[e2e] FAIL: 1234 detail source should be 'branch_numeric', got {num.get('source')!r}")
    sys.exit(1)
print("[e2e] OK: /analytics/tickets/{id} returns matching sources for both tickets")
PY

echo "[e2e] PASS"
