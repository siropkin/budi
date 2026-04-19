#!/usr/bin/env bash
# End-to-end regression for issue #303: verify branch attribution surfaces via
# tailer-ingested transcripts (8.2 live path).
#
# - Ingest three transcript turns in one session with a concrete git_branch.
# - Assert all assistant rows carry that branch.
# - Assert /analytics/branches surfaces that branch (what `budi stats --branches` uses).
# - Assert `budi doctor` branch-attribution check is not red.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BUDI="$ROOT/target/release/budi"
BUDI_DAEMON="$ROOT/target/release/budi-daemon"

if [[ ! -x "$BUDI" || ! -x "$BUDI_DAEMON" ]]; then
  echo "error: release binaries not built. run \`cargo build --release\` first." >&2
  exit 2
fi

TMPDIR_ROOT="$(mktemp -d -t budi-e2e-303-XXXXXX)"
export HOME="$TMPDIR_ROOT"
mkdir -p "$HOME/.config/budi"

DAEMON_PORT=17879

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
TRANSCRIPTS_DIR="$CLAUDE_PROJECTS/e2e-303"
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
  local req_id="$4"
  local uuid_prefix="$5"
  local prompt="$6"

  python3 - "$file" "$session" "$branch" "$REPO_ROOT" "$req_id" "$uuid_prefix" "$prompt" <<'PY'
import datetime
import json
import sys

file_path, session, branch, cwd, req_id, prefix, prompt = sys.argv[1:]
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

SESSION_ID="e2e-sess-303-$(date +%s)"
BRANCH="PROJ-303-branch-attribution"

append_turn "$TRANSCRIPTS_DIR/session.jsonl" "$SESSION_ID" "$BRANCH" "req-303-1-$SESSION_ID" "303-1-$SESSION_ID" "first turn"
append_turn "$TRANSCRIPTS_DIR/session.jsonl" "$SESSION_ID" "$BRANCH" "req-303-2-$SESSION_ID" "303-2-$SESSION_ID" "second turn"
append_turn "$TRANSCRIPTS_DIR/session.jsonl" "$SESSION_ID" "$BRANCH" "req-303-3-$SESSION_ID" "303-3-$SESSION_ID" "third turn"

wait_sql_eq "3" "SELECT COUNT(*) FROM messages WHERE session_id = '$SESSION_ID' AND role='assistant';" "three assistant rows in session"

echo "[e2e] DB rows for session:"
sqlite3 "$DB" "SELECT id, session_id, git_branch, repo_id FROM messages WHERE session_id = '$SESSION_ID' AND role='assistant';"

WITH_BRANCH="$(sqlite3 "$DB" "SELECT COUNT(*) FROM messages WHERE session_id = '$SESSION_ID' AND role='assistant' AND git_branch = '$BRANCH';")"
if [[ "$WITH_BRANCH" != "3" ]]; then
  echo "[e2e] FAIL: expected all 3 assistant rows to carry branch '$BRANCH', got $WITH_BRANCH" >&2
  sqlite3 "$DB" "SELECT id, git_branch FROM messages WHERE session_id = '$SESSION_ID' AND role='assistant';" >&2
  exit 1
fi
echo "[e2e] OK: all assistant rows carry branch '$BRANCH'"

SINCE_TS="$(date -u -v0H -v0M -v0S +%Y-%m-%dT%H:%M:%SZ 2>/dev/null \
           || date -u --date="today 00:00:00" +%Y-%m-%dT%H:%M:%SZ)"
BRANCHES_JSON="$(curl -s --max-time 5 \
  "http://127.0.0.1:$DAEMON_PORT/analytics/branches?limit=20&since=$SINCE_TS")"
echo "[e2e] /analytics/branches -> $BRANCHES_JSON"
if ! echo "$BRANCHES_JSON" | grep -Fq "\"git_branch\":\"$BRANCH\""; then
  echo "[e2e] FAIL: /analytics/branches did not surface branch '$BRANCH'" >&2
  exit 1
fi
echo "[e2e] OK: branch '$BRANCH' visible via /analytics/branches"

echo "[e2e] budi doctor (tailer-contract output, post-#325):"
DOCTOR_OUT="$(cd "$REPO_ROOT" && "$BUDI" doctor --repo-root "$REPO_ROOT" 2>&1 || true)"
# After R2.4 (#325) the doctor is reorganized around the tailer contract:
# transcript visibility / tailer health replace the standalone "branch
# attribution" section. The branch surfacing for #303 is now proven by the
# /analytics/branches assertion above; here we just guard that doctor still
# reports tailer health as PASS for the Claude provider on the seeded
# transcript so a future regression that breaks attribution-by-tailer is
# caught loudly.
echo "$DOCTOR_OUT" | grep -E "tailer health / Claude Code" || {
  echo "[e2e] FAIL: budi doctor did not print a tailer-health section for Claude Code" >&2
  echo "$DOCTOR_OUT" | tail -n 60 >&2
  exit 1
}
if echo "$DOCTOR_OUT" | grep -q "FAIL tailer health / Claude Code"; then
  echo "[e2e] FAIL: budi doctor reported a red tailer-health result for Claude Code" >&2
  echo "$DOCTOR_OUT" | tail -n 60 >&2
  exit 1
fi
echo "[e2e] OK: budi doctor tailer-health for Claude Code is not red"

echo "[e2e] PASS"
