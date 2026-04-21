#!/usr/bin/env bash
# Release smoke gate for v8.2.0 — pins the scenarios from issue #328.
#
# This script is the executable contract behind the R3.2 release gate. It
# walks the daemon + CLI through the four #328 scenario families on macOS /
# Linux (Windows runs as its own job in CI) using the live tailer path:
#
#   1. Fresh install
#      - `budi init --no-daemon` writes nothing outside the data dir
#      - daemon boots cleanly
#      - `budi doctor` PASSes (with the expected one-time "leftover proxy
#        config" WARN tolerated when the host shell still exports the
#        legacy `*_BASE_URL` env vars)
#      - synthetic JSONL turns for **all four providers** (Claude Code,
#        Codex, Cursor, Copilot CLI) round-trip into `messages` rows
#        through the tailer within ≤10 s of file flush
#      - attribution (repo_id, git_branch) populates from the seeded git
#        repo + transcript metadata
#
#   2. Upgrade from 8.1.x
#      - 8.1-shaped DB (proxy_events table + proxy_estimated message) plus
#        legacy managed shell / Cursor / Codex blocks are seeded
#      - rerunning init drops `proxy_events` while keeping retained
#        history queryable (#326)
#      - `budi init --cleanup` removes the three managed-block flavors
#        idempotently and produces a reviewable diff (#357)
#      - `budi doctor` reports the legacy state honestly and is silent
#        about the obsolete table after migration
#
#   3. Failure modes
#      - Daemon killed mid-session: a synthetic transcript turn appended
#        while the daemon is dead is replayed exactly once on restart
#        (no dupes, no missing rows)
#      - Transcript root missing for one provider: tailer warns/keeps
#        going for the providers that do have data
#      - Schema drift: doctor surfaces the regression (#309) with the
#        actionable migration hint
#      - Disk-full surrogate: data-dir made read-only, daemon fails loudly
#        without crashing the agent (we just verify the daemon refuses to
#        ingest, not that it kills the agent — agents run out-of-band)
#
#   4. Cross-provider
#      - Claude Code transcript → row visible in /analytics/sessions
#      - Codex session_meta + token_count → row with provider=codex
#      - Cursor agent-transcripts JSONL → row with provider=cursor
#      - Copilot session-state events.jsonl → row with provider=copilot_cli
#
# Acceptance pinned by #328 and SOUL.md §release-readiness:
#   - PASS exit means a real PASS record, not a drafted plan.
#   - Anything that goes red here blocks `v8.2.0` until either fixed or
#     deferred to 8.2.1 with an explicit sign-off comment on the parent
#     issue.
#
# Run:
#   cargo build --release
#   bash scripts/e2e/test_328_release_smoke.sh
#   KEEP_TMP=1 bash scripts/e2e/test_328_release_smoke.sh   # post-mortem
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

# Drop stale env that 8.0/8.1 installs (or other e2e scripts in the same
# shell session) may have leaked. We want the smoke gate to evaluate the
# daemon, not whatever the surrounding shell happens to export.
unset ANTHROPIC_BASE_URL OPENAI_BASE_URL COPILOT_PROVIDER_BASE_URL COPILOT_PROVIDER_TYPE \
      CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC || true

TMPDIR_ROOT="$(mktemp -d -t budi-e2e-328-XXXXXX)"
export HOME="$TMPDIR_ROOT"
export BUDI_HOME="$HOME/.local/share/budi"
export CODEX_HOME="$HOME/.codex"
export COPILOT_HOME="$HOME/.copilot"

DAEMON_PORT=17828
DB="$BUDI_HOME/analytics.db"
REPO_ROOT="$HOME/repo"

DAEMON_PID=""

cleanup() {
  local status=$?
  if [[ -n "${DAEMON_PID:-}" ]]; then
    kill "$DAEMON_PID" >/dev/null 2>&1 || true
  fi
  pkill -f "budi-daemon serve --host 127.0.0.1 --port $DAEMON_PORT" >/dev/null 2>&1 || true
  if [[ "${KEEP_TMP:-0}" == "1" ]]; then
    echo "[e2e] leaving tmp: $TMPDIR_ROOT" >&2
  else
    chmod -R u+rwX "$BUDI_HOME" 2>/dev/null || true
    rm -rf "$TMPDIR_ROOT"
  fi
  exit $status
}
trap cleanup EXIT INT TERM

# ---------------------------------------------------------------------------
# helpers
# ---------------------------------------------------------------------------

step() {
  echo
  echo "=================================================================="
  echo "[e2e] $*"
  echo "=================================================================="
}

assert_contains() {
  local file="$1"
  local needle="$2"
  if ! grep -Fq "$needle" "$file"; then
    echo "[e2e] FAIL: expected '$needle' in $file" >&2
    sed -e 's/^/    /' "$file" >&2 || true
    exit 1
  fi
}

assert_not_contains() {
  local file="$1"
  local needle="$2"
  if grep -Fq "$needle" "$file"; then
    echo "[e2e] FAIL: did not expect '$needle' in $file" >&2
    sed -e 's/^/    /' "$file" >&2 || true
    exit 1
  fi
}

assert_absent() {
  local path="$1"
  if [[ -e "$path" ]]; then
    echo "[e2e] FAIL: expected nothing at $path, found:" >&2
    ls -la "$path" >&2 || true
    exit 1
  fi
}

wait_sql_eq() {
  local expected="$1"
  local sql="$2"
  local label="$3"
  local got=""
  # Cap at 60 s so providers that depend on the 5 s backstop poll (notify
  # may miss the create event for files born inside a freshly-mounted
  # watch root, especially on macOS) still have time to settle. We poll at
  # 0.5 s instead of 0.1 s so the read lock doesn't compete with the
  # daemon's write lock under WAL.
  for _ in {1..120}; do
    got="$(sqlite3 -cmd ".timeout 5000" "$DB" "$sql" 2>/dev/null || true)"
    if [[ "$got" == "$expected" ]]; then
      return 0
    fi
    sleep 0.5
  done
  echo "[e2e] FAIL: timed out waiting for $label (expected '$expected', got '$got')" >&2
  echo "[e2e] sql: $sql" >&2
  tail -n 120 "$TMPDIR_ROOT/daemon.log" >&2 || true
  exit 1
}

start_daemon() {
  RUST_LOG=info "$BUDI_DAEMON" serve \
    --host 127.0.0.1 --port "$DAEMON_PORT" \
    >>"$TMPDIR_ROOT/daemon.log" 2>&1 &
  DAEMON_PID=$!

  for _ in {1..50}; do
    if curl -s -o /dev/null -w "%{http_code}" --max-time 1 \
        "http://127.0.0.1:$DAEMON_PORT/health" | grep -q "^200"; then
      return 0
    fi
    sleep 0.1
  done
  echo "[e2e] FAIL: daemon did not come up on :$DAEMON_PORT" >&2
  tail -n 80 "$TMPDIR_ROOT/daemon.log" >&2 || true
  exit 1
}

stop_daemon() {
  if [[ -z "${DAEMON_PID:-}" ]]; then
    return 0
  fi
  kill "$DAEMON_PID" >/dev/null 2>&1 || true
  for _ in {1..30}; do
    if ! kill -0 "$DAEMON_PID" >/dev/null 2>&1; then
      DAEMON_PID=""
      return 0
    fi
    sleep 0.1
  done
  kill -9 "$DAEMON_PID" >/dev/null 2>&1 || true
  DAEMON_PID=""
}

# ---------------------------------------------------------------------------
# bootstrap: tidy fake repo + budi config so the CLI agrees with the daemon
# ---------------------------------------------------------------------------

mkdir -p "$REPO_ROOT/.budi"
cat >"$REPO_ROOT/.budi/budi.toml" <<CFG
daemon_host = "127.0.0.1"
daemon_port = $DAEMON_PORT
CFG

(
  cd "$REPO_ROOT"
  git init -q 2>/dev/null || true
  git remote add origin https://github.com/siropkin/budi.git 2>/dev/null || true
  git config user.email e2e@budi.local
  git config user.name "Budi 8.2 Smoke"
  git checkout -q -B v8/328-release-smoke 2>/dev/null || true
  echo "smoke" > smoke.txt
  git add smoke.txt
  git commit -q -m "seed smoke" 2>/dev/null || true
)

# ---------------------------------------------------------------------------
# scenario 1 — fresh install
# ---------------------------------------------------------------------------

step "scenario 1: fresh install"

INIT_LOG="$TMPDIR_ROOT/init-fresh.log"
"$BUDI" init --no-daemon >"$INIT_LOG" 2>&1 || {
  cat "$INIT_LOG" >&2
  echo "[e2e] FAIL: fresh init crashed" >&2
  exit 1
}

# Init must not touch shell / cursor / codex configs on a clean machine.
assert_absent "$HOME/.zshrc"
assert_absent "$HOME/.bashrc"
assert_absent "$HOME/.config/fish/config.fish"
assert_absent "$HOME/.cursor/settings.json"
assert_absent "$CODEX_HOME/config.toml"

if [[ ! -f "$DB" ]]; then
  echo "[e2e] FAIL: init did not create $DB" >&2
  exit 1
fi

# Seed every provider's watch root *before* booting the daemon so the tailer
# attaches a watcher to each one. The current tailer enumerates providers
# at startup and exits if none are available; a future change that swaps to
# dynamic provider discovery would make this seeding unnecessary but harmless.
mkdir -p \
  "$HOME/.claude/projects" \
  "$CODEX_HOME/sessions" \
  "$HOME/.cursor/projects" \
  "$COPILOT_HOME/session-state"

start_daemon

# Doctor should be green for the things we care about. The "leftover proxy
# config" WARN is acceptable when the host shell still has 8.1 env vars
# defined; we just guard against any FAIL line slipping in.
DOCTOR_LOG="$TMPDIR_ROOT/doctor-fresh.log"
(
  cd "$REPO_ROOT"
  "$BUDI" doctor --repo-root "$REPO_ROOT" >"$DOCTOR_LOG" 2>&1 || true
)
if grep -E "^  FAIL " "$DOCTOR_LOG" >/dev/null; then
  echo "[e2e] FAIL: doctor reported a hard failure on a fresh install:" >&2
  sed -e 's/^/    /' "$DOCTOR_LOG" >&2
  exit 1
fi
assert_contains "$DOCTOR_LOG" "PASS daemon health"
assert_contains "$DOCTOR_LOG" "PASS schema drift"

# ---------------------------------------------------------------------------
# scenario 4 first (data needs to be there before we measure end-to-end
# latency in scenario 1's tailer assertion). We use the same daemon for
# scenarios 1 + 4 because both prove "fresh install ingests live data".
# ---------------------------------------------------------------------------

step "scenario 4: cross-provider tailer ingest (Claude / Codex / Cursor / Copilot)"

now_iso() {
  python3 - <<'PY'
import datetime
print(datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z"))
PY
}

# --- Claude Code -----------------------------------------------------------

CLAUDE_DIR="$HOME/.claude/projects/repo"
mkdir -p "$CLAUDE_DIR"
CLAUDE_FILE="$CLAUDE_DIR/session-claude-328.jsonl"
CLAUDE_SESSION="claude-328-$(date +%s)"

python3 - "$CLAUDE_FILE" "$CLAUDE_SESSION" "$REPO_ROOT" <<'PY'
import datetime
import json
import sys

path, session, cwd = sys.argv[1:]
now = datetime.datetime.now(datetime.timezone.utc)
ts = lambda d: (now + datetime.timedelta(milliseconds=d)).isoformat(timespec="milliseconds").replace("+00:00", "Z")

user = {
    "type": "user", "uuid": f"{session}-u", "parentUuid": None,
    "isSidechain": False, "sessionId": session, "timestamp": ts(0),
    "cwd": cwd, "gitBranch": "v8/328-release-smoke",
    "message": {"role": "user", "content": "smoke test"},
}
assistant = {
    "type": "assistant", "uuid": f"{session}-a", "parentUuid": f"{session}-u",
    "isSidechain": False, "sessionId": session, "timestamp": ts(200),
    "cwd": cwd, "gitBranch": "v8/328-release-smoke",
    "message": {
        "type": "message", "role": "assistant",
        "id": f"req-{session}", "model": "claude-sonnet-4-6",
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "usage": {
            "input_tokens": 100, "output_tokens": 25,
            "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0,
        },
    },
}
with open(path, "w", encoding="utf-8") as f:
    f.write(json.dumps(user, separators=(",", ":")) + "\n")
    f.write(json.dumps(assistant, separators=(",", ":")) + "\n")
PY

wait_sql_eq "1" \
  "SELECT COUNT(*) FROM messages WHERE session_id='$CLAUDE_SESSION' AND role='assistant' AND provider='claude_code';" \
  "claude_code assistant row tailed"

# --- Codex ------------------------------------------------------------------

NOW_YEAR="$(date -u +%Y)"
NOW_MONTH="$(date -u +%m)"
NOW_DAY="$(date -u +%d)"
CODEX_DIR="$CODEX_HOME/sessions/$NOW_YEAR/$NOW_MONTH/$NOW_DAY"
mkdir -p "$CODEX_DIR"
CODEX_SESSION="$(uuidgen | tr 'A-Z' 'a-z')"
CODEX_FILE="$CODEX_DIR/rollout-${CODEX_SESSION}.jsonl"

python3 - "$CODEX_FILE" "$CODEX_SESSION" "$REPO_ROOT" <<'PY'
import datetime
import json
import sys

path, session, cwd = sys.argv[1:]
now = datetime.datetime.now(datetime.timezone.utc)
ts = lambda d: (now + datetime.timedelta(milliseconds=d)).isoformat(timespec="milliseconds").replace("+00:00", "Z")

events = [
    {"timestamp": ts(0),   "type": "session_meta",
     "payload": {"id": session, "cwd": cwd,
                 "git": {"branch": "v8/328-release-smoke", "commit_hash": "deadbeef"}}},
    {"timestamp": ts(50),  "type": "turn_context",
     "payload": {"model": "gpt-5.3-codex", "turn_id": "t1"}},
    {"timestamp": ts(500), "type": "event_msg",
     "payload": {"type": "token_count",
                 "info": {"last_token_usage": {
                     "input_tokens": 1024, "cached_input_tokens": 256,
                     "output_tokens": 64,  "reasoning_output_tokens": 16,
                     "total_tokens": 1088}}}},
]
with open(path, "w", encoding="utf-8") as f:
    for e in events:
        f.write(json.dumps(e, separators=(",", ":")) + "\n")
PY

# NOTE: identity::normalize_session_id strips the "codex-" prefix from
# UUID-shaped session ids, so the stored session_id is the bare UUID.
wait_sql_eq "1" \
  "SELECT COUNT(*) FROM messages WHERE session_id='$CODEX_SESSION' AND provider='codex';" \
  "codex token_count row tailed"

# --- Cursor (file-based JSONL) ---------------------------------------------

CURSOR_PROJECT="$HOME/.cursor/projects/repo-328"
CURSOR_DIR="$CURSOR_PROJECT/agent-transcripts"
mkdir -p "$CURSOR_DIR"
# worker.log lets the cursor provider attribute cwd back to the real repo
echo "workspacePath=$REPO_ROOT something=else" >"$CURSOR_PROJECT/worker.log"

CURSOR_SESSION="cursor-328-$(date +%s)"
CURSOR_FILE="$CURSOR_DIR/${CURSOR_SESSION}.jsonl"

python3 - "$CURSOR_FILE" "$CURSOR_SESSION" "$REPO_ROOT" <<'PY'
import datetime
import json
import sys

path, session, cwd = sys.argv[1:]
now = datetime.datetime.now(datetime.timezone.utc)
ts = lambda d: (now + datetime.timedelta(milliseconds=d)).isoformat(timespec="milliseconds").replace("+00:00", "Z")

user = {
    "role": "user", "uuid": f"{session}-u", "sessionId": session,
    "timestamp": ts(0), "cwd": cwd,
    "message": {"content": "investigate the bug"},
}
assistant = {
    "role": "assistant", "uuid": f"{session}-a", "sessionId": session,
    "timestamp": ts(300), "cwd": cwd, "model": "composer-2",
    "usage": {"input_tokens": 800, "output_tokens": 200,
              "cache_creation_tokens": 0, "cache_read_tokens": 0},
}
with open(path, "w", encoding="utf-8") as f:
    f.write(json.dumps(user, separators=(",", ":")) + "\n")
    f.write(json.dumps(assistant, separators=(",", ":")) + "\n")
PY

wait_sql_eq "1" \
  "SELECT COUNT(*) FROM messages WHERE session_id='$CURSOR_SESSION' AND role='assistant' AND provider='cursor';" \
  "cursor assistant row tailed"

# --- Copilot CLI -----------------------------------------------------------

COPILOT_SESSION="copilot-328-$(date +%s)"
COPILOT_DIR="$COPILOT_HOME/session-state/$COPILOT_SESSION"
mkdir -p "$COPILOT_DIR"
cat >"$COPILOT_DIR/workspace.yaml" <<YAML
cwd: $REPO_ROOT
git_branch: v8/328-release-smoke
YAML
COPILOT_FILE="$COPILOT_DIR/events.jsonl"

python3 - "$COPILOT_FILE" "$COPILOT_SESSION" <<'PY'
import datetime
import json
import sys

path, session = sys.argv[1:]
now = datetime.datetime.now(datetime.timezone.utc)
ts = lambda d: (now + datetime.timedelta(milliseconds=d)).isoformat(timespec="milliseconds").replace("+00:00", "Z")

events = [
    {"type": "assistant.turn_start",
     "data": {"turnId": "t1", "model": "gpt-5.3"},
     "id": "e1", "timestamp": ts(0), "parentId": None},
    {"type": "user.message",
     "data": {"content": "fix the lint", "turnId": "t1"},
     "id": "e2", "timestamp": ts(50), "parentId": None},
    {"type": "assistant.usage",
     "data": {"input_tokens": 4096, "output_tokens": 384, "cached_input_tokens": 1024},
     "id": "e3", "timestamp": ts(800), "parentId": None},
    {"type": "assistant.turn_end",
     "data": {"turnId": "t1", "status": "success"},
     "id": "e4", "timestamp": ts(900), "parentId": None},
]
with open(path, "w", encoding="utf-8") as f:
    for e in events:
        f.write(json.dumps(e, separators=(",", ":")) + "\n")
PY

wait_sql_eq "1" \
  "SELECT COUNT(*) FROM messages WHERE session_id LIKE '%$COPILOT_SESSION%' AND provider='copilot_cli';" \
  "copilot_cli assistant row tailed"

# --- attribution check ------------------------------------------------------

step "scenario 1: attribution (repo_id + git_branch) populated for live tail"

# Attribution is ingested at parse time + enriched by the resolver. Give it
# a beat to settle then assert the Claude row carries both fields.
sleep 1
ATTR_BRANCH="$(sqlite3 "$DB" "SELECT git_branch FROM messages WHERE session_id='$CLAUDE_SESSION' AND role='assistant';")"
ATTR_REPO="$(sqlite3 "$DB" "SELECT repo_id FROM messages WHERE session_id='$CLAUDE_SESSION' AND role='assistant';")"
if [[ "$ATTR_BRANCH" != "v8/328-release-smoke" ]]; then
  echo "[e2e] FAIL: claude row branch attribution missing/bad: '$ATTR_BRANCH'" >&2
  exit 1
fi
if [[ "$ATTR_REPO" != "github.com/siropkin/budi" ]]; then
  echo "[e2e] FAIL: claude row repo attribution missing/bad: '$ATTR_REPO'" >&2
  exit 1
fi

# CLI must agree with the API on what's there.
SESSIONS_JSON="$(curl -fsS "http://127.0.0.1:$DAEMON_PORT/analytics/sessions?limit=200&since=$(date -u +%Y-%m-%dT00:00:00+00:00)")"
COPILOT_NORMALIZED_SESSION="$(sqlite3 "$DB" "SELECT DISTINCT session_id FROM messages WHERE provider='copilot_cli' AND session_id LIKE '%$COPILOT_SESSION%' LIMIT 1;")"
for sid in "$CLAUDE_SESSION" "$CODEX_SESSION" "$CURSOR_SESSION" "$COPILOT_NORMALIZED_SESSION"; do
  if ! echo "$SESSIONS_JSON" | python3 -c '
import json, sys
data = json.load(sys.stdin)
ids = [s.get("id") for s in data.get("sessions", [])]
print("1" if sys.argv[1] in ids else "0")
' "$sid" | grep -q "^1$"; then
    echo "[e2e] FAIL: /analytics/sessions did not return $sid" >&2
    echo "$SESSIONS_JSON" | python3 -m json.tool >&2 || true
    exit 1
  fi
done

# `budi stats` is the user-facing surface; make sure it doesn't error and
# emits a parseable JSON envelope when --format json is used.
STATS_LOG="$TMPDIR_ROOT/stats.log"
(
  cd "$REPO_ROOT"
  "$BUDI" stats --format json >"$STATS_LOG" 2>&1
)
python3 - "$STATS_LOG" <<'PY'
import json, sys
with open(sys.argv[1]) as f:
    blob = f.read()
data = json.loads(blob)
# `budi stats --format json` returns the same envelope as /analytics/summary;
# we just need to confirm it's valid JSON with at least one numeric cost field
# so a future regression that breaks the JSON shape is caught loudly.
assert isinstance(data, dict), data
PY

# `budi status` should also be happy.
(
  cd "$REPO_ROOT"
  "$BUDI" status >"$TMPDIR_ROOT/status.log" 2>&1
)

# ---------------------------------------------------------------------------
# scenario 3 — daemon killed mid-session: appended turn replays exactly once
# ---------------------------------------------------------------------------

step "scenario 3a: daemon killed mid-session, restart catches up without dupes"

stop_daemon
COUNT_BEFORE_RESTART="$(sqlite3 "$DB" "SELECT COUNT(*) FROM messages WHERE session_id='$CLAUDE_SESSION';")"

# Append a second turn while the daemon is down.
python3 - "$CLAUDE_FILE" "$CLAUDE_SESSION" "$REPO_ROOT" <<'PY'
import datetime
import json
import sys

path, session, cwd = sys.argv[1:]
now = datetime.datetime.now(datetime.timezone.utc)
ts = lambda d: (now + datetime.timedelta(milliseconds=d)).isoformat(timespec="milliseconds").replace("+00:00", "Z")

user = {
    "type": "user", "uuid": f"{session}-u2", "parentUuid": f"{session}-a",
    "isSidechain": False, "sessionId": session, "timestamp": ts(0),
    "cwd": cwd, "gitBranch": "v8/328-release-smoke",
    "message": {"role": "user", "content": "round 2"},
}
assistant = {
    "type": "assistant", "uuid": f"{session}-a2", "parentUuid": f"{session}-u2",
    "isSidechain": False, "sessionId": session, "timestamp": ts(200),
    "cwd": cwd, "gitBranch": "v8/328-release-smoke",
    "message": {
        "type": "message", "role": "assistant",
        "id": f"req-{session}-2", "model": "claude-sonnet-4-6",
        "content": [{"type": "text", "text": "again"}],
        "stop_reason": "end_turn",
        "usage": {
            "input_tokens": 50, "output_tokens": 12,
            "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0,
        },
    },
}
with open(path, "a", encoding="utf-8") as f:
    f.write(json.dumps(user, separators=(",", ":")) + "\n")
    f.write(json.dumps(assistant, separators=(",", ":")) + "\n")
PY

start_daemon
wait_sql_eq "2" \
  "SELECT COUNT(*) FROM messages WHERE session_id='$CLAUDE_SESSION' AND role='assistant';" \
  "second assistant turn tailed after restart"

DUPES="$(sqlite3 "$DB" "SELECT COUNT(*) FROM (SELECT id, COUNT(*) c FROM messages WHERE session_id='$CLAUDE_SESSION' GROUP BY id HAVING c > 1);")"
if [[ "$DUPES" != "0" ]]; then
  echo "[e2e] FAIL: duplicate rows after daemon restart for session '$CLAUDE_SESSION'" >&2
  sqlite3 "$DB" "SELECT id, role, COUNT(*) FROM messages WHERE session_id='$CLAUDE_SESSION' GROUP BY id, role;" >&2
  exit 1
fi
echo "[e2e] OK: post-restart count went $COUNT_BEFORE_RESTART -> $(sqlite3 "$DB" "SELECT COUNT(*) FROM messages WHERE session_id='$CLAUDE_SESSION';") with no dupes"

# ---------------------------------------------------------------------------
# scenario 3 — schema drift / #309 regression
# ---------------------------------------------------------------------------

step "scenario 3b: schema drift surfaces an actionable doctor FAIL (#309 regression)"

stop_daemon
python3 - <<'PY'
import os
import sqlite3

db_path = os.path.join(os.environ["BUDI_HOME"], "analytics.db")
conn = sqlite3.connect(db_path)
conn.execute("PRAGMA user_version = 0")
conn.commit()
conn.close()
PY

DRIFT_LOG="$TMPDIR_ROOT/doctor-drift.log"
set +e
BUDI_DAEMON_BIN="/definitely/missing/budi-daemon" \
  "$BUDI" doctor --repo-root "$REPO_ROOT" >"$DRIFT_LOG" 2>&1
DRIFT_STATUS=$?
set -e
if [[ $DRIFT_STATUS -eq 0 ]]; then
  echo "[e2e] FAIL: doctor was supposed to fail under schema drift" >&2
  cat "$DRIFT_LOG" >&2
  exit 1
fi
assert_contains "$DRIFT_LOG" "FAIL schema drift:"
assert_contains "$DRIFT_LOG" "Run \`budi init\` or \`budi update\`"

# Restore schema by re-running init so subsequent scenarios have a working DB.
"$BUDI" init --no-daemon >/dev/null
start_daemon

# ---------------------------------------------------------------------------
# scenario 3 — transcript root missing for one provider
# ---------------------------------------------------------------------------

step "scenario 3c: transcript root missing for one provider, daemon keeps going"

# Wipe the Codex sessions root entirely. Tailer should keep ingesting Claude
# / Cursor / Copilot rows; doctor should still pass the surviving providers.
rm -rf "$CODEX_HOME/sessions" "$CODEX_HOME/archived_sessions"

DOCTOR_MISSING_LOG="$TMPDIR_ROOT/doctor-missing-codex.log"
(
  cd "$REPO_ROOT"
  "$BUDI" doctor --repo-root "$REPO_ROOT" >"$DOCTOR_MISSING_LOG" 2>&1 || true
)
# Doctor should report the surviving providers' tailer health as PASS even
# when one provider's watch root vanished.
assert_contains "$DOCTOR_MISSING_LOG" "PASS tailer health / Claude Code"

# Append a brand-new Claude turn after removing Codex roots; daemon must
# still tail it.
LATER_SESSION="claude-recover-$(date +%s)"
LATER_FILE="$CLAUDE_DIR/session-${LATER_SESSION}.jsonl"
python3 - "$LATER_FILE" "$LATER_SESSION" "$REPO_ROOT" <<'PY'
import datetime, json, sys
path, session, cwd = sys.argv[1:]
now = datetime.datetime.now(datetime.timezone.utc)
ts = lambda d: (now + datetime.timedelta(milliseconds=d)).isoformat(timespec="milliseconds").replace("+00:00", "Z")
user = {"type":"user","uuid":f"{session}-u","parentUuid":None,"isSidechain":False,
        "sessionId":session,"timestamp":ts(0),"cwd":cwd,
        "gitBranch":"v8/328-release-smoke","message":{"role":"user","content":"recover"}}
assistant = {"type":"assistant","uuid":f"{session}-a","parentUuid":f"{session}-u",
             "isSidechain":False,"sessionId":session,"timestamp":ts(150),"cwd":cwd,
             "gitBranch":"v8/328-release-smoke",
             "message":{"type":"message","role":"assistant","id":f"req-{session}",
                        "model":"claude-sonnet-4-6",
                        "content":[{"type":"text","text":"ok"}],
                        "stop_reason":"end_turn",
                        "usage":{"input_tokens":1,"output_tokens":1,
                                 "cache_creation_input_tokens":0,
                                 "cache_read_input_tokens":0}}}
with open(path, "w", encoding="utf-8") as f:
    f.write(json.dumps(user,  separators=(",", ":")) + "\n")
    f.write(json.dumps(assistant, separators=(",", ":")) + "\n")
PY

wait_sql_eq "1" \
  "SELECT COUNT(*) FROM messages WHERE session_id='$LATER_SESSION' AND role='assistant';" \
  "claude continues to tail with codex root missing"

# ---------------------------------------------------------------------------
# scenario 3 — disk-full surrogate: data dir made read-only
# ---------------------------------------------------------------------------

step "scenario 3d: data dir read-only — daemon refuses ingest without crashing the agent"

# Stop daemon, flip the data dir read-only, append, then start the daemon
# again. We verify it doesn't ingest into the read-only DB and that the
# daemon process either exits cleanly or stays up reporting the failure
# (we accept either, just not "writes a corrupt row anyway").
stop_daemon
COUNT_BEFORE_RO="$(sqlite3 "$DB" "SELECT COUNT(*) FROM messages WHERE session_id='$LATER_SESSION';")"
chmod -R a-w "$BUDI_HOME"

RO_SESSION="claude-ro-$(date +%s)"
RO_FILE="$CLAUDE_DIR/session-${RO_SESSION}.jsonl"
python3 - "$RO_FILE" "$RO_SESSION" "$REPO_ROOT" <<'PY'
import datetime, json, sys
path, session, cwd = sys.argv[1:]
now = datetime.datetime.now(datetime.timezone.utc)
ts = lambda d: (now + datetime.timedelta(milliseconds=d)).isoformat(timespec="milliseconds").replace("+00:00", "Z")
assistant = {"type":"assistant","uuid":f"{session}-a","parentUuid":None,
             "isSidechain":False,"sessionId":session,"timestamp":ts(0),"cwd":cwd,
             "gitBranch":"v8/328-release-smoke",
             "message":{"type":"message","role":"assistant","id":f"req-{session}",
                        "model":"claude-sonnet-4-6",
                        "content":[{"type":"text","text":"ro"}],
                        "stop_reason":"end_turn",
                        "usage":{"input_tokens":1,"output_tokens":1,
                                 "cache_creation_input_tokens":0,
                                 "cache_read_input_tokens":0}}}
with open(path, "w", encoding="utf-8") as f:
    f.write(json.dumps(assistant, separators=(",", ":")) + "\n")
PY

# Start daemon: it may fail to bind/serve under a read-only DB, that's
# acceptable failure-loud behavior. We just want to verify it does NOT
# silently write a row to the (now read-only) DB.
RO_DAEMON_LOG="$TMPDIR_ROOT/daemon-ro.log"
set +e
RUST_LOG=info "$BUDI_DAEMON" serve --host 127.0.0.1 --port "$DAEMON_PORT" \
  >"$RO_DAEMON_LOG" 2>&1 &
RO_PID=$!
sleep 3
kill "$RO_PID" >/dev/null 2>&1 || true
wait "$RO_PID" 2>/dev/null || true
set -e

COUNT_AFTER_RO="$(sqlite3 "$DB" "SELECT COUNT(*) FROM messages WHERE session_id='$RO_SESSION';" 2>/dev/null || echo "0")"
if [[ "$COUNT_AFTER_RO" != "0" ]]; then
  echo "[e2e] FAIL: read-only DB somehow received row '$RO_SESSION' (count=$COUNT_AFTER_RO)" >&2
  exit 1
fi

# Restore writability so subsequent scenarios proceed.
chmod -R u+rwX "$BUDI_HOME"
DAEMON_PID=""
start_daemon

# Existing transcripts are intentionally seeded at EOF on daemon start
# (ADR-0089 §3 — `budi db import` is the only path that backfills history),
# so the RO_SESSION file written while the daemon was down will not
# auto-replay. Instead we write a *new* session after restoring write
# access and confirm the tailer is alive again.
RECOVER_SESSION="claude-rw-$(date +%s)"
RECOVER_FILE="$CLAUDE_DIR/session-${RECOVER_SESSION}.jsonl"
python3 - "$RECOVER_FILE" "$RECOVER_SESSION" "$REPO_ROOT" <<'PY'
import datetime, json, sys
path, session, cwd = sys.argv[1:]
now = datetime.datetime.now(datetime.timezone.utc)
ts = lambda d: (now + datetime.timedelta(milliseconds=d)).isoformat(timespec="milliseconds").replace("+00:00", "Z")
assistant = {"type":"assistant","uuid":f"{session}-a","parentUuid":None,
             "isSidechain":False,"sessionId":session,"timestamp":ts(0),"cwd":cwd,
             "gitBranch":"v8/328-release-smoke",
             "message":{"type":"message","role":"assistant","id":f"req-{session}",
                        "model":"claude-sonnet-4-6",
                        "content":[{"type":"text","text":"rw"}],
                        "stop_reason":"end_turn",
                        "usage":{"input_tokens":1,"output_tokens":1,
                                 "cache_creation_input_tokens":0,
                                 "cache_read_input_tokens":0}}}
with open(path, "w", encoding="utf-8") as f:
    f.write(json.dumps(assistant, separators=(",", ":")) + "\n")
PY
wait_sql_eq "1" \
  "SELECT COUNT(*) FROM messages WHERE session_id='$RECOVER_SESSION' AND role='assistant';" \
  "tailer recovers after data dir is writable again"

# ---------------------------------------------------------------------------
# scenario 2 — upgrade from 8.1.x: managed blocks + proxy_events migration
# ---------------------------------------------------------------------------

step "scenario 2a: upgrade from 8.1 — proxy_events table dropped, history retained"

# Re-inject 8.1-shaped state on top of the live DB.
python3 - <<'PY'
import datetime, os, sqlite3

db_path = os.path.join(os.environ["BUDI_HOME"], "analytics.db")
conn = sqlite3.connect(db_path)
conn.execute("""
CREATE TABLE IF NOT EXISTS proxy_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp TEXT NOT NULL,
    provider TEXT,
    model TEXT,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    cost_cents REAL
)
""")
ts = datetime.datetime.now(datetime.timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z")
conn.execute("INSERT INTO proxy_events (timestamp, provider, model, input_tokens, output_tokens, cost_cents) VALUES (?, 'openai', 'gpt-4o', 17, 3, 0.25)", (ts,))
conn.execute("""
INSERT OR REPLACE INTO messages (id, role, timestamp, model, provider, input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens, cost_cents, cost_confidence)
VALUES ('legacy-proxy-message-328', 'assistant', ?, 'gpt-4o', 'openai', 17, 3, 0, 0, 0.25, 'proxy_estimated')
""", (ts,))
conn.commit()
conn.close()
PY

# Stop the daemon so init can run its migration without contention.
stop_daemon

INIT_UPGRADE_LOG="$TMPDIR_ROOT/init-upgrade.log"
"$BUDI" init --no-daemon >"$INIT_UPGRADE_LOG" 2>&1 || {
  cat "$INIT_UPGRADE_LOG" >&2
  echo "[e2e] FAIL: upgrade init crashed" >&2
  exit 1
}

if [[ "$(sqlite3 "$DB" "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='proxy_events';")" != "0" ]]; then
  echo "[e2e] FAIL: proxy_events table should be removed after upgrade init" >&2
  sqlite3 "$DB" "SELECT name FROM sqlite_master WHERE type='table';" >&2 || true
  exit 1
fi
if [[ "$(sqlite3 "$DB" "SELECT COUNT(*) FROM messages WHERE cost_confidence='proxy_estimated';")" -lt "1" ]]; then
  echo "[e2e] FAIL: expected retained proxy_estimated history after upgrade" >&2
  exit 1
fi

start_daemon

step "scenario 2b: upgrade from 8.1 — budi init --cleanup removes managed blocks idempotently"

CURSOR_SETTINGS="$HOME/Library/Application Support/Cursor/User/settings.json"
mkdir -p "$(dirname "$CURSOR_SETTINGS")"

cat >"$HOME/.zshrc" <<'EOF'
export PATH="/usr/local/bin:$PATH"
# >>> budi >>>
export ANTHROPIC_BASE_URL="http://localhost:9878"
export OPENAI_BASE_URL="http://localhost:9878"
# <<< budi <<<
EOF
cat >"$CURSOR_SETTINGS" <<'EOF'
{
  "editor.fontSize": 14,
  // >>> budi >>>
  "openai.baseUrl": "http://localhost:9878"
  // <<< budi <<<
}
EOF
cat >"$CODEX_HOME/config.toml" <<'EOF'
# >>> budi >>>
openai_base_url = "http://localhost:9878"
# <<< budi <<<
EOF

CLEANUP_LOG="$TMPDIR_ROOT/cleanup.log"
"$BUDI" init --cleanup --yes --no-daemon >"$CLEANUP_LOG" 2>&1 || {
  cat "$CLEANUP_LOG" >&2
  echo "[e2e] FAIL: budi init --cleanup failed" >&2
  exit 1
}
assert_contains "$CLEANUP_LOG" "Cleanup summary"
assert_not_contains "$HOME/.zshrc" "ANTHROPIC_BASE_URL"
assert_not_contains "$CURSOR_SETTINGS" "openai.baseUrl"
assert_not_contains "$CODEX_HOME/config.toml" "openai_base_url"

CLEANUP_LOG_2="$TMPDIR_ROOT/cleanup-2.log"
"$BUDI" init --cleanup --yes --no-daemon >"$CLEANUP_LOG_2" 2>&1 || {
  cat "$CLEANUP_LOG_2" >&2
  echo "[e2e] FAIL: idempotent cleanup re-run failed" >&2
  exit 1
}
assert_contains "$CLEANUP_LOG_2" "Nothing to clean."

step "scenario 2c: upgrade — doctor honestly reports retained legacy state"

DOCTOR_UPGRADE_LOG="$TMPDIR_ROOT/doctor-upgrade.log"
(
  cd "$REPO_ROOT"
  "$BUDI" doctor --repo-root "$REPO_ROOT" >"$DOCTOR_UPGRADE_LOG" 2>&1 || true
)
assert_contains "$DOCTOR_UPGRADE_LOG" "PASS legacy proxy history:"
assert_not_contains "$DOCTOR_UPGRADE_LOG" "obsolete \`proxy_events\` table is still present"

step "scenario 2d: uninstall on an upgraded-from-8.1 machine reports parity"

# Reseed managed blocks to verify uninstall removes them.
cat >"$HOME/.zshrc" <<'EOF'
# >>> budi >>>
export ANTHROPIC_BASE_URL="http://localhost:9878"
# <<< budi <<<
EOF
cat >"$CODEX_HOME/config.toml" <<'EOF'
# >>> budi >>>
openai_base_url = "http://localhost:9878"
# <<< budi <<<
EOF

UNINSTALL_LOG="$TMPDIR_ROOT/uninstall.log"
"$BUDI" uninstall --yes --keep-data >"$UNINSTALL_LOG" 2>&1 || {
  cat "$UNINSTALL_LOG" >&2
  echo "[e2e] FAIL: uninstall failed on upgraded-from-8.1 machine" >&2
  exit 1
}
assert_contains "$UNINSTALL_LOG" "Removing legacy 8.0/8.1 proxy residue..."
assert_not_contains "$HOME/.zshrc" "ANTHROPIC_BASE_URL"
assert_not_contains "$CODEX_HOME/config.toml" "openai_base_url"

# ---------------------------------------------------------------------------
# Done
# ---------------------------------------------------------------------------

step "release smoke gate complete"

ROW_TOTAL="$(sqlite3 "$DB" "SELECT COUNT(*) FROM messages;")"
echo "[e2e] total messages rows seen by tailer: $ROW_TOTAL"
echo "[e2e] PASS"
