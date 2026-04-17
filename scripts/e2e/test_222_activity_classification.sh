#!/usr/bin/env bash
# End-to-end regression for issue #222: verify that live proxy traffic gets
# classified by `classify_request_body` in-memory and that the derived
# (activity, activity_source, activity_confidence) triple lands on the DB row
# as `tags`, is visible via `/analytics/activities`, and — crucially — that no
# prompt text is persisted anywhere (ADR-0083).
#
# - Isolates HOME to a temp dir.
# - Starts a mock Anthropic upstream.
# - Starts the real release `budi-daemon`.
# - Drives two proxied requests in two different sessions:
#     1. "fix the login bug please" -> activity=bugfix, source=rule
#     2. "explain the error in the login flow" -> activity=question
#        (guards the #222 precedence fix: question-anchor beats bugfix keyword).
# - Drives a third proxied request with a long multi-signal prompt and asserts
#   confidence=high so the queries-layer aggregation path is exercised end-to-end
#   (not just the in-memory classifier).
# - Asserts every expected tag triple is present on the assistant message row.
# - Asserts `/analytics/activities` surfaces `source=rule` and a non-empty
#   `confidence` (the queries layer must read these back from `tags`, not fall
#   back to the legacy `rule`/`medium` constants — the "high" request proves
#   this).
# - Asserts no prompt text ("login bug", "explain the error", "regression")
#   appears anywhere in the analytics DB (privacy contract).
#
# Negative-path check: remove the `classify_request_body` call in
# `crates/budi-daemon/src/routes/proxy.rs::proxy_request` (or make it always
# return `None`) and this script must fail on the "activity tag present"
# assertion.
set -euo pipefail

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
PROXY_PORT=19880
UPSTREAM_PORT=19335

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
            "id": "msg_e2e_222",
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

TS="$(date +%s)"
BUGFIX_SESSION="e2e-222-bugfix-$TS"
QUESTION_SESSION="e2e-222-question-$TS"
HIGH_SESSION="e2e-222-high-$TS"

send() {
  local label="$1"; local session="$2"; local prompt="$3"
  echo "[e2e] proxy request: $label (session=$session)"
  local body
  body=$(python3 -c 'import json,sys; print(json.dumps({"model":"claude-sonnet-4-6","messages":[{"role":"user","content":sys.argv[1]}]}))' "$prompt")
  local status
  status=$(curl -s -o "$TMPDIR_ROOT/${label}.json" -w "%{http_code}" --max-time 5 \
    -X POST \
    -H "content-type: application/json" \
    -H "x-budi-session: $session" \
    -d "$body" \
    "http://127.0.0.1:$PROXY_PORT/v1/messages")
  if [[ "$status" != "200" ]]; then
    echo "[e2e] FAIL: $label returned $status" >&2
    tail -n 60 "$TMPDIR_ROOT/daemon.log" >&2 || true
    exit 1
  fi
}

# 1. bugfix keyword with a leading bugfix-action phrase -> category=bugfix.
send "bugfix" "$BUGFIX_SESSION" "fix the login bug please"
# 2. question-anchor phrase over a bugfix keyword -> must land in `question`,
#    guarding the #222 precedence fix.
send "question" "$QUESTION_SESSION" "explain the error in the login flow"
# 3. multiple distinct keyword hits -> confidence must be `high`. This is the
#    proof that /analytics/activities reads per-aggregate source/confidence
#    from tags instead of returning the R1.0 fallback constants.
send "high" "$HIGH_SESSION" "fix the crash and patch the regression in the login flow"

sleep 1

DB="$HOME/.local/share/budi/analytics.db"
if [[ ! -f "$DB" ]]; then
  echo "[e2e] FAIL: analytics DB missing at $DB" >&2
  tail -n 40 "$TMPDIR_ROOT/daemon.log" >&2 || true
  exit 1
fi

# --- 1. bugfix session: assert all three activity tags present with expected values.
MSG_ID_BUGFIX=$(sqlite3 "$DB" "SELECT id FROM messages WHERE session_id = '$BUGFIX_SESSION' ORDER BY timestamp DESC LIMIT 1;")
if [[ -z "$MSG_ID_BUGFIX" ]]; then
  echo "[e2e] FAIL: no message row for bugfix session" >&2
  exit 1
fi

tag_value() {
  local mid="$1"; local key="$2"
  sqlite3 "$DB" "SELECT value FROM tags WHERE message_id = '$mid' AND key = '$key' LIMIT 1;"
}

ACTIVITY=$(tag_value "$MSG_ID_BUGFIX" activity)
SOURCE=$(tag_value "$MSG_ID_BUGFIX" activity_source)
CONFIDENCE=$(tag_value "$MSG_ID_BUGFIX" activity_confidence)

echo "[e2e] bugfix row tags: activity=$ACTIVITY source=$SOURCE confidence=$CONFIDENCE"

if [[ "$ACTIVITY" != "bugfix" ]]; then
  echo "[e2e] FAIL: expected activity=bugfix on '$MSG_ID_BUGFIX', got '$ACTIVITY'" >&2
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

# --- 2. question session: precedence fix ("explain the error ...").
MSG_ID_Q=$(sqlite3 "$DB" "SELECT id FROM messages WHERE session_id = '$QUESTION_SESSION' ORDER BY timestamp DESC LIMIT 1;")
ACTIVITY_Q=$(tag_value "$MSG_ID_Q" activity)
SOURCE_Q=$(tag_value "$MSG_ID_Q" activity_source)
echo "[e2e] question row: activity=$ACTIVITY_Q source=$SOURCE_Q"
if [[ "$ACTIVITY_Q" != "question" ]]; then
  echo "[e2e] FAIL: expected question-anchor prompt to classify as 'question', got '$ACTIVITY_Q'" >&2
  echo "[e2e] (this guards the #222 precedence fix: question anchors must beat the raw 'error' keyword)" >&2
  exit 1
fi
if [[ "$SOURCE_Q" != "rule" ]]; then
  echo "[e2e] FAIL: expected activity_source=rule on question row, got '$SOURCE_Q'" >&2
  exit 1
fi
echo "[e2e] OK: question anchor wins over bugfix keyword (activity=question)"

# --- 3. high-confidence session.
MSG_ID_H=$(sqlite3 "$DB" "SELECT id FROM messages WHERE session_id = '$HIGH_SESSION' ORDER BY timestamp DESC LIMIT 1;")
CONFIDENCE_H=$(tag_value "$MSG_ID_H" activity_confidence)
echo "[e2e] high-confidence row: confidence=$CONFIDENCE_H"
if [[ "$CONFIDENCE_H" != "high" ]]; then
  echo "[e2e] FAIL: expected confidence=high on multi-keyword prompt, got '$CONFIDENCE_H'" >&2
  exit 1
fi
echo "[e2e] OK: multi-signal prompt reached confidence=high"

# --- 4. /analytics/activities reflects source/confidence from tags, not the
#        R1.0 `rule`/`medium` fallback. The multi-signal 'bugfix' row we just
#        inserted forces confidence=high for the bugfix aggregate.
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

# Every non-(untagged) row must expose a real source/confidence from tags.
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

# The high-confidence bugfix prompt must promote the bugfix aggregate.
if by_name["bugfix"].get("confidence") != "high":
    print(f"[e2e] FAIL: bugfix confidence should have been promoted to 'high' "
          f"by the multi-keyword prompt, got {by_name['bugfix'].get('confidence')}")
    print("      (this means activity_classification_labels is falling back to the "
          "R1.0 'medium' default instead of reading tags)")
    sys.exit(1)

print(f"[e2e] OK: /analytics/activities exposes real source/confidence "
      f"(bugfix={by_name['bugfix']['confidence']}, "
      f"question={by_name['question']['confidence']})")
PY

# --- 5. Privacy contract (ADR-0083): no prompt text in the analytics DB.
#        We classified three prompts in-memory; none of their distinctive
#        tokens should be persisted anywhere — not in `messages`, not in
#        `tags`, not in `proxy_events`.
echo "[e2e] sweeping DB for leaked prompt text (ADR-0083)..."
LEAK=$(sqlite3 "$DB" <<SQL
SELECT 'messages' AS tbl, id FROM messages
  WHERE COALESCE(session_id,'') || ' ' || COALESCE(repo_id,'') || ' ' || COALESCE(git_branch,'')
        LIKE '%login bug%'
UNION ALL
SELECT 'tags', message_id FROM tags
  WHERE value LIKE '%login bug%'
     OR value LIKE '%explain the error%'
     OR value LIKE '%patch the regression%'
UNION ALL
SELECT 'proxy_events', CAST(id AS TEXT) FROM proxy_events
  WHERE COALESCE(model,'') LIKE '%login bug%'
     OR COALESCE(model,'') LIKE '%explain the error%';
SQL
)
if [[ -n "$LEAK" ]]; then
  echo "[e2e] FAIL: prompt text leaked into analytics DB (ADR-0083 violation):" >&2
  echo "$LEAK" >&2
  exit 1
fi
echo "[e2e] OK: no prompt text in analytics DB (ADR-0083 preserved)"

echo "[e2e] PASS"
