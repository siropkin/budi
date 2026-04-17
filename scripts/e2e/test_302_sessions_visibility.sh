#!/usr/bin/env bash
# End-to-end smoke test for issue #302: verify that a proxied assistant message
# shows up in `budi sessions -p today` on a fresh database.
#
# - Isolates HOME to a temp dir so the test cannot touch real user data.
# - Starts a mock upstream HTTP server that returns a canned Anthropic reply.
# - Runs the real release `budi-daemon` pointed at the mock upstream.
# - POSTs through the proxy with an X-Budi-Session header.
# - Asserts `budi sessions -p today` lists the session and that the DB row
#   has a non-null session_id.
# - Runs `budi doctor` and asserts the new "sessions visibility" block reports
#   no mismatch.
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
PROXY_PORT=19878
UPSTREAM_PORT=19333

cleanup() {
  local status=$?
  # Kill the daemon we started, plus any CLI-autostarted daemon that bound
  # to the default port while running under this isolated HOME.
  { kill "${DAEMON_PID:-}" 2>/dev/null || true; } >/dev/null 2>&1
  { kill "${CLI_DAEMON_PID:-}" 2>/dev/null || true; } >/dev/null 2>&1
  { kill "${UPSTREAM_PID:-}" 2>/dev/null || true; } >/dev/null 2>&1
  # Defense-in-depth: pkill any stray daemon/mock still referencing our tmp.
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

# The `budi` CLI resolves the daemon URL from the repo-scoped `.budi/budi.toml`,
# so create a throwaway repo root inside HOME and point the CLI at our daemon.
REPO_ROOT="$HOME/repo"
mkdir -p "$REPO_ROOT/.budi"
cat >"$REPO_ROOT/.budi/budi.toml" <<EOF
daemon_host = "127.0.0.1"
daemon_port = $DAEMON_PORT

[proxy]
enabled = true
port = $PROXY_PORT
EOF
# Make it a git repo so repo-root resolution succeeds.
(cd "$REPO_ROOT" && git init -q 2>/dev/null || true)

# Mock upstream — responds to /v1/messages with a minimal non-streaming
# Anthropic reply carrying realistic usage so cost/token extraction runs.
cat >"$TMPDIR_ROOT/mock_upstream.py" <<'PY'
import http.server, json, sys

class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        n = int(self.headers.get("Content-Length", "0") or 0)
        _ = self.rfile.read(n)
        body = {
            "id": "msg_e2e_302",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-6",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 42,
                "output_tokens": 7,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0,
            },
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
PROXY_UP=0
for _ in {1..50}; do
  if curl -s -o /dev/null -w "%{http_code}" --max-time 1 -X POST -H "content-type: application/json" -d '{}' "http://127.0.0.1:$PROXY_PORT/v1/messages" | grep -qE "^(2|4|5)[0-9]{2}$"; then
    PROXY_UP=1
    break
  fi
  sleep 0.1
done
if [[ "$PROXY_UP" != "1" ]]; then
  echo "[e2e] FAIL: proxy did not come up on :$PROXY_PORT" >&2
  echo "--- daemon log ---" >&2
  cat "$TMPDIR_ROOT/daemon.log" >&2 || true
  exit 1
fi

SESSION_ID="e2e-sess-$(date +%s)"
echo "[e2e] sending proxied request with X-Budi-Session=$SESSION_ID"
STATUS=$(curl -s -o "$TMPDIR_ROOT/proxy_reply.json" -w "%{http_code}" --max-time 5 \
  -X POST \
  -H "content-type: application/json" \
  -H "x-budi-session: $SESSION_ID" \
  -H "x-budi-repo: github.com/siropkin/budi" \
  -H "x-budi-branch: v8/302-sessions-visibility" \
  -d '{"model":"claude-sonnet-4-6","messages":[{"role":"user","content":"hi"}]}' \
  "http://127.0.0.1:$PROXY_PORT/v1/messages")

if [[ "$STATUS" != "200" ]]; then
  echo "[e2e] FAIL: proxy POST returned $STATUS" >&2
  cat "$TMPDIR_ROOT/proxy_reply.json" >&2 || true
  echo "--- daemon log ---" >&2
  tail -n 60 "$TMPDIR_ROOT/daemon.log" >&2 || true
  exit 1
fi

# Give the daemon a beat to finish the spawn_blocking DB write.
sleep 0.5

echo "[e2e] sessions list (json):"
SESSIONS_JSON=$(curl -s "http://127.0.0.1:$DAEMON_PORT/analytics/sessions?since=$(date -u +%Y-%m-%dT00:00:00+00:00)")
echo "$SESSIONS_JSON" | python3 -m json.tool

FOUND=$(echo "$SESSIONS_JSON" | python3 -c "
import json, sys
data = json.load(sys.stdin)
ids = [s.get('id') for s in data.get('sessions', [])]
print('1' if '$SESSION_ID' in ids else '0')
")
if [[ "$FOUND" != "1" ]]; then
  echo "[e2e] FAIL: session '$SESSION_ID' not returned by /analytics/sessions" >&2
  echo "--- daemon log ---" >&2
  tail -n 60 "$TMPDIR_ROOT/daemon.log" >&2 || true
  exit 1
fi
echo "[e2e] OK: session '$SESSION_ID' visible via /analytics/sessions"

DB="$HOME/.local/share/budi/analytics.db"
echo "[e2e] DB rows for session:"
sqlite3 "$DB" "SELECT id, session_id, provider, model, input_tokens, output_tokens, cost_confidence FROM messages WHERE session_id = '$SESSION_ID';"

ROWCOUNT=$(sqlite3 "$DB" "SELECT COUNT(*) FROM messages WHERE session_id = '$SESSION_ID';")
if [[ "$ROWCOUNT" != "1" ]]; then
  echo "[e2e] FAIL: expected 1 messages row for '$SESSION_ID', found $ROWCOUNT" >&2
  exit 1
fi
NULL_SESSIONS=$(sqlite3 "$DB" "SELECT COUNT(*) FROM messages WHERE role='assistant' AND (session_id IS NULL OR session_id='');")
if [[ "$NULL_SESSIONS" != "0" ]]; then
  echo "[e2e] FAIL: $NULL_SESSIONS assistant rows were written with NULL/empty session_id — regression" >&2
  exit 1
fi
echo "[e2e] OK: session_id persisted, no NULL/empty-session_id rows"

# Run the CLI from inside the isolated repo root so `load_config` picks up
# our `.budi/budi.toml` and the CLI points at the test daemon on $DAEMON_PORT
# instead of auto-starting a second daemon on the default port.
echo "[e2e] budi sessions -p today:"
(cd "$REPO_ROOT" && "$BUDI" sessions -p today) | tee "$TMPDIR_ROOT/sessions_today.txt"
if ! grep -q "$SESSION_ID" "$TMPDIR_ROOT/sessions_today.txt" \
  && ! grep -q "${SESSION_ID:0:8}" "$TMPDIR_ROOT/sessions_today.txt"; then
  echo "[e2e] FAIL: \`budi sessions -p today\` did not list the session" >&2
  exit 1
fi
echo "[e2e] OK: budi sessions -p today lists the session"

echo "[e2e] budi doctor (sessions visibility section):"
DOCTOR_OUT="$(cd "$REPO_ROOT" && "$BUDI" doctor 2>&1 || true)"
echo "$DOCTOR_OUT" | grep -E "sessions visibility" || {
  echo "[e2e] FAIL: budi doctor did not print a sessions-visibility check" >&2
  echo "$DOCTOR_OUT" | tail -n 40 >&2
  exit 1
}
if echo "$DOCTOR_OUT" | grep -q "Sessions visibility mismatch"; then
  echo "[e2e] FAIL: budi doctor reported a sessions-visibility mismatch" >&2
  echo "$DOCTOR_OUT" | tail -n 40 >&2
  exit 1
fi
echo "[e2e] OK: budi doctor sessions-visibility check is green"

echo "[e2e] PASS"
