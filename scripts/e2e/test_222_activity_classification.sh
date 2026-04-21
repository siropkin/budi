#!/usr/bin/env bash
# End-to-end regression for issue #222: verify activity classification and
# (activity, source, confidence) tags via the 8.2 live path (filesystem
# tailer), plus ADR-0083 privacy guarantees.
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

TMPDIR_ROOT="$(mktemp -d -t budi-e2e-222-XXXXXX)"
export HOME="$TMPDIR_ROOT"
mkdir -p "$HOME/.config/budi"

DAEMON_PORT=17880

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

CLAUDE_PROJECTS="$HOME/.claude/projects"
TRANSCRIPTS_DIR="$CLAUDE_PROJECTS/e2e-222"
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
  local prompt="$3"
  local req_id="$4"
  local uuid_prefix="$5"

  python3 - "$file" "$session" "$REPO_ROOT" "$prompt" "$req_id" "$uuid_prefix" <<'PY'
import datetime
import json
import sys

file_path, session, cwd, prompt, req_id, prefix = sys.argv[1:]
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
    "gitBranch": "v8/222-activity-classification",
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
    "gitBranch": "v8/222-activity-classification",
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
BUGFIX_SESSION="e2e-222-bugfix-$TS"
QUESTION_SESSION="e2e-222-question-$TS"
HIGH_SESSION="e2e-222-high-$TS"

append_turn "$TRANSCRIPTS_DIR/bugfix.jsonl"   "$BUGFIX_SESSION"   "fix the login bug please" "req-222-bugfix-$TS" "bugfix-$TS"
append_turn "$TRANSCRIPTS_DIR/question.jsonl" "$QUESTION_SESSION" "explain the error in the login flow" "req-222-question-$TS" "question-$TS"
append_turn "$TRANSCRIPTS_DIR/high.jsonl"     "$HIGH_SESSION"     "fix the crash and patch the regression in the login flow" "req-222-high-$TS" "high-$TS"

wait_sql_eq "1" "SELECT COUNT(*) FROM messages WHERE session_id = '$BUGFIX_SESSION' AND role='assistant';" "bugfix assistant row"
wait_sql_eq "1" "SELECT COUNT(*) FROM messages WHERE session_id = '$QUESTION_SESSION' AND role='assistant';" "question assistant row"
wait_sql_eq "1" "SELECT COUNT(*) FROM messages WHERE session_id = '$HIGH_SESSION' AND role='assistant';" "high assistant row"

tag_value() {
  local mid="$1"; local key="$2"
  sqlite3 "$DB" "SELECT value FROM tags WHERE message_id = '$mid' AND key = '$key' LIMIT 1;"
}

MSG_ID_BUGFIX="$(sqlite3 "$DB" "SELECT id FROM messages WHERE session_id = '$BUGFIX_SESSION' AND role='assistant' ORDER BY timestamp DESC LIMIT 1;")"
if [[ -z "$MSG_ID_BUGFIX" ]]; then
  echo "[e2e] FAIL: no assistant row for bugfix session" >&2
  exit 1
fi

ACTIVITY="$(tag_value "$MSG_ID_BUGFIX" activity)"
SOURCE="$(tag_value "$MSG_ID_BUGFIX" activity_source)"
CONFIDENCE="$(tag_value "$MSG_ID_BUGFIX" activity_confidence)"

echo "[e2e] bugfix row tags: activity=$ACTIVITY source=$SOURCE confidence=$CONFIDENCE"

if [[ "$ACTIVITY" != "bugfix" ]]; then
  echo "[e2e] FAIL: expected activity=bugfix, got '$ACTIVITY'" >&2
  sqlite3 "$DB" "SELECT key, value FROM tags WHERE message_id = '$MSG_ID_BUGFIX';" >&2
  exit 1
fi
if [[ "$SOURCE" != "rule" ]]; then
  echo "[e2e] FAIL: expected activity_source=rule, got '$SOURCE'" >&2
  exit 1
fi
if [[ -z "$CONFIDENCE" ]]; then
  echo "[e2e] FAIL: activity_confidence tag missing" >&2
  exit 1
fi
echo "[e2e] OK: bugfix session carries activity=bugfix source=rule confidence=$CONFIDENCE"

MSG_ID_Q="$(sqlite3 "$DB" "SELECT id FROM messages WHERE session_id = '$QUESTION_SESSION' AND role='assistant' ORDER BY timestamp DESC LIMIT 1;")"
ACTIVITY_Q="$(tag_value "$MSG_ID_Q" activity)"
SOURCE_Q="$(tag_value "$MSG_ID_Q" activity_source)"
echo "[e2e] question row: activity=$ACTIVITY_Q source=$SOURCE_Q"
if [[ "$ACTIVITY_Q" != "question" ]]; then
  echo "[e2e] FAIL: expected question-anchor prompt to classify as 'question', got '$ACTIVITY_Q'" >&2
  exit 1
fi
if [[ "$SOURCE_Q" != "rule" ]]; then
  echo "[e2e] FAIL: expected activity_source=rule on question row, got '$SOURCE_Q'" >&2
  exit 1
fi
echo "[e2e] OK: question anchor wins over bugfix keyword (activity=question)"

MSG_ID_H="$(sqlite3 "$DB" "SELECT id FROM messages WHERE session_id = '$HIGH_SESSION' AND role='assistant' ORDER BY timestamp DESC LIMIT 1;")"
CONFIDENCE_H="$(tag_value "$MSG_ID_H" activity_confidence)"
echo "[e2e] high-confidence row: confidence=$CONFIDENCE_H"
if [[ "$CONFIDENCE_H" != "high" ]]; then
  echo "[e2e] FAIL: expected confidence=high on multi-keyword prompt, got '$CONFIDENCE_H'" >&2
  exit 1
fi
echo "[e2e] OK: multi-signal prompt reached confidence=high"

SINCE_TS="$(date -u -v0H -v0M -v0S +%Y-%m-%dT%H:%M:%SZ 2>/dev/null \
           || date -u --date="today 00:00:00" +%Y-%m-%dT%H:%M:%SZ)"
ACTIVITIES_JSON="$(curl -s --max-time 5 \
  "http://127.0.0.1:$DAEMON_PORT/analytics/activities?limit=20&since=$SINCE_TS")"
echo "[e2e] /analytics/activities -> $ACTIVITIES_JSON"

python3 - "$ACTIVITIES_JSON" <<'PY'
import json, sys

data = json.loads(sys.argv[1])
by_name = {row["activity"]: row for row in data}

missing = [n for n in ("bugfix", "question") if n not in by_name]
if missing:
    print(f"[e2e] FAIL: /analytics/activities missing rows for: {missing}")
    sys.exit(1)

bad = []
for name, row in by_name.items():
    if name == "(untagged)":
        continue
    if row.get("source") != "rule":
        bad.append((name, "source", row.get("source")))
    if not row.get("confidence"):
        bad.append((name, "confidence", row.get("confidence")))
if bad:
    print(f"[e2e] FAIL: /analytics/activities has rows without source/confidence: {bad}")
    sys.exit(1)

if by_name["bugfix"].get("confidence") != "high":
    print(f"[e2e] FAIL: bugfix confidence should be 'high', got {by_name['bugfix'].get('confidence')}")
    sys.exit(1)

print(f"[e2e] OK: /analytics/activities exposes real source/confidence "
      f"(bugfix={by_name['bugfix']['confidence']}, "
      f"question={by_name['question']['confidence']})")
PY

echo "[e2e] sweeping DB for leaked prompt text (ADR-0083)..."
LEAK="$(sqlite3 "$DB" <<SQL
SELECT 'messages' AS tbl, id FROM messages
  WHERE COALESCE(session_id,'') || ' ' || COALESCE(repo_id,'') || ' ' || COALESCE(git_branch,'')
        LIKE '%login bug%'
UNION ALL
SELECT 'tags', message_id FROM tags
  WHERE value LIKE '%login bug%'
     OR value LIKE '%explain the error%'
     OR value LIKE '%patch the regression%';
SQL
)"
if [[ -n "$LEAK" ]]; then
  echo "[e2e] FAIL: prompt text leaked into analytics DB (ADR-0083 violation):" >&2
  echo "$LEAK" >&2
  exit 1
fi
echo "[e2e] OK: no prompt text in analytics DB (ADR-0083 preserved)"

echo "[e2e] PASS"
