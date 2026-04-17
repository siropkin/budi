#!/usr/bin/env bash
# End-to-end regression for issue #303: verify that live proxy traffic ends up
# with a real `git_branch` on the `messages` row (not `(untagged)`), including
# the session-level propagation that backfills earlier rows when a later
# request in the same session finally carries attribution.
#
# - Isolates HOME to a temp dir.
# - Starts a mock Anthropic upstream.
# - Starts the real release `budi-daemon`.
# - Drives three proxied requests in one session: the first two have no
#   attribution headers, the third supplies `X-Budi-Branch`. After the third,
#   every row in the session must carry that branch.
# - Verifies `budi stats --branches` reports the branch (not `(untagged)`).
# - Verifies `budi doctor` branch-attribution check is green.
#
# Negative-path check: revert the session-level backfill in
# `crates/budi-core/src/proxy.rs::insert_proxy_message` and this script must
# fail on the "earlier rows adopted the branch" assertion.
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
PROXY_PORT=19879
UPSTREAM_PORT=19334

cleanup() {
  local status=$?
  { kill "${DAEMON_PID:-}" 2>/dev/null || true; } >/dev/null 2>&1
  { kill "${UPSTREAM_PID:-}" 2>/dev/null || true; } >/dev/null 2>&1
  pkill -f "budi-daemon serve --host 127.0.0.1 --port $DAEMON_PORT" >/dev/null 2>&1 || true
  pkill -f "mock_upstream.py $UPSTREAM_PORT" >/dev/null 2>&1 || true
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
cat >"$REPO_ROOT/.budi/budi.toml" <<EOF
daemon_host = "127.0.0.1"
daemon_port = $DAEMON_PORT

[proxy]
enabled = true
port = $PROXY_PORT
EOF
(cd "$REPO_ROOT" && git init -q 2>/dev/null || true)

cat >"$TMPDIR_ROOT/mock_upstream.py" <<'PY'
import http.server, json, sys

class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        n = int(self.headers.get("Content-Length", "0") or 0)
        _ = self.rfile.read(n)
        body = {
            "id": "msg_e2e_303",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-6",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 42, "output_tokens": 7,
                      "cache_creation_input_tokens": 0,
                      "cache_read_input_tokens": 0},
        }
        payload = json.dumps(body).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *a, **k):
        pass

port = int(sys.argv[1])
http.server.HTTPServer(("127.0.0.1", port), H).serve_forever()
PY

echo "[e2e] starting mock upstream on :$UPSTREAM_PORT"
python3 "$TMPDIR_ROOT/mock_upstream.py" "$UPSTREAM_PORT" >"$TMPDIR_ROOT/upstream.log" 2>&1 &
UPSTREAM_PID=$!
for _ in {1..30}; do
  if curl -s -o /dev/null -w "%{http_code}" --max-time 1 "http://127.0.0.1:$UPSTREAM_PORT/" | grep -qE "^[45]"; then
    break
  fi
  sleep 0.1
done

echo "[e2e] starting budi-daemon on :$DAEMON_PORT / proxy :$PROXY_PORT"
BUDI_ANTHROPIC_UPSTREAM="http://127.0.0.1:$UPSTREAM_PORT" \
BUDI_OPENAI_UPSTREAM="http://127.0.0.1:$UPSTREAM_PORT" \
RUST_LOG=info \
  "$BUDI_DAEMON" serve \
    --host 127.0.0.1 \
    --port $DAEMON_PORT \
    --proxy-port $PROXY_PORT \
    >"$TMPDIR_ROOT/daemon.log" 2>&1 &
DAEMON_PID=$!

for _ in {1..50}; do
  if curl -s -o /dev/null -w "%{http_code}" --max-time 1 "http://127.0.0.1:$DAEMON_PORT/health" | grep -q "^200"; then
    break
  fi
  sleep 0.1
done
for _ in {1..50}; do
  if curl -s -o /dev/null -w "%{http_code}" --max-time 1 -X POST -H "content-type: application/json" -d '{}' "http://127.0.0.1:$PROXY_PORT/v1/messages" | grep -qE "^(2|4|5)[0-9]{2}$"; then
    break
  fi
  sleep 0.1
done

SESSION_ID="e2e-sess-303-$(date +%s)"
BRANCH="PROJ-303-branch-attribution"

send() {
  local label="$1"; shift
  echo "[e2e] proxy request: $label"
  local status
  status=$(curl -s -o "$TMPDIR_ROOT/${label}.json" -w "%{http_code}" --max-time 5 \
    -X POST \
    -H "content-type: application/json" \
    -H "x-budi-session: $SESSION_ID" \
    "$@" \
    -d '{"model":"claude-sonnet-4-6","messages":[{"role":"user","content":"hi"}]}' \
    "http://127.0.0.1:$PROXY_PORT/v1/messages")
  if [[ "$status" != "200" ]]; then
    echo "[e2e] FAIL: $label returned $status" >&2
    tail -n 60 "$TMPDIR_ROOT/daemon.log" >&2 || true
    exit 1
  fi
}

send "req1-no-attribution"
send "req2-no-attribution"
send "req3-with-branch" -H "x-budi-branch: $BRANCH" -H "x-budi-repo: github.com/siropkin/budi"

# Let spawn_blocking DB writes land.
sleep 1

DB="$HOME/.local/share/budi/analytics.db"
echo "[e2e] DB rows for session:"
sqlite3 "$DB" "SELECT id, session_id, git_branch, repo_id FROM messages WHERE session_id = '$SESSION_ID';"

TOTAL=$(sqlite3 "$DB" "SELECT COUNT(*) FROM messages WHERE session_id = '$SESSION_ID';")
if [[ "$TOTAL" != "3" ]]; then
  echo "[e2e] FAIL: expected 3 rows for session, got $TOTAL" >&2
  exit 1
fi

WITH_BRANCH=$(sqlite3 "$DB" "SELECT COUNT(*) FROM messages WHERE session_id = '$SESSION_ID' AND git_branch = '$BRANCH';")
if [[ "$WITH_BRANCH" != "3" ]]; then
  echo "[e2e] FAIL: expected all 3 rows to carry branch '$BRANCH' (session propagation/backfill), got $WITH_BRANCH" >&2
  echo "[e2e] rows:" >&2
  sqlite3 "$DB" "SELECT id, git_branch FROM messages WHERE session_id = '$SESSION_ID';" >&2
  exit 1
fi
echo "[e2e] OK: all 3 session rows share branch '$BRANCH'"

# Exercise the same query that `budi stats --branches` issues, via the daemon
# HTTP API. We cannot reliably invoke the `budi` CLI here because the CLI's
# `load_config` path reads `<BUDI_HOME>/repos/<hash>/budi.toml`, not the
# `$REPO_ROOT/.budi/budi.toml` we create — so the CLI would talk to whatever
# default-port daemon is present on the developer machine and return a stale
# 500. The daemon endpoint under test is the exact one the CLI calls, so
# hitting it here is a genuine regression guard for #303.
SINCE_TS="$(date -u -v0H -v0M -v0S +%Y-%m-%dT%H:%M:%SZ 2>/dev/null \
           || date -u --date="today 00:00:00" +%Y-%m-%dT%H:%M:%SZ)"
BRANCHES_JSON="$(curl -s --max-time 5 \
  "http://127.0.0.1:$DAEMON_PORT/analytics/branches?limit=20&since=$SINCE_TS")"
echo "[e2e] /analytics/branches -> $BRANCHES_JSON"
if ! echo "$BRANCHES_JSON" | grep -Fq "\"git_branch\":\"$BRANCH\""; then
  echo "[e2e] FAIL: /analytics/branches did not surface branch '$BRANCH'" >&2
  exit 1
fi
echo "[e2e] OK: branch '$BRANCH' visible via /analytics/branches (what \`budi stats --branches\` queries)"

# `budi doctor` reads the analytics DB directly via `BUDI_HOME` (=$HOME/.local/share/budi
# here), not the daemon HTTP client, so it runs cleanly inside the isolated
# test HOME without fighting the developer-machine daemon.
echo "[e2e] budi doctor (branch attribution section):"
DOCTOR_OUT="$(cd "$REPO_ROOT" && "$BUDI" doctor --repo-root "$REPO_ROOT" 2>&1 || true)"
echo "$DOCTOR_OUT" | grep -E "branch attribution" || {
  echo "[e2e] FAIL: budi doctor did not print a branch-attribution check" >&2
  echo "$DOCTOR_OUT" | tail -n 40 >&2
  exit 1
}
if echo "$DOCTOR_OUT" | grep -q "Branch attribution is broken"; then
  echo "[e2e] FAIL: budi doctor reported a red branch-attribution result" >&2
  echo "$DOCTOR_OUT" | tail -n 40 >&2
  exit 1
fi
echo "[e2e] OK: budi doctor branch-attribution check is not red"

echo "[e2e] PASS"
