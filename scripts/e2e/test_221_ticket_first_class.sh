#!/usr/bin/env bash
# End-to-end regression for issue #221 (R1.3): verify the unified ticket
# extractor and the `ticket_source` first-class sibling tag on the live
# proxy path.
#
# What this script guards:
# - Alphanumeric branch tickets (e.g. `PROJ-221-foo`) land as
#   `ticket_id=PROJ-221` + `ticket_prefix=PROJ` + `ticket_source=branch`.
# - Numeric-only branch tickets (e.g. `feature/1234`) land as
#   `ticket_id=1234` + `ticket_source=branch_numeric` + NO `ticket_prefix`
#   (pre-R1.3 the proxy wrote numeric-only ids with no source, and the
#   pipeline rejected them entirely).
# - Integration branches (`main`) emit no ticket tags at all — previously
#   the proxy wrote `ticket_id="Unassigned"`, creating a second bucket
#   next to `(untagged)`.
# - The legacy `"Unassigned"` sentinel is never persisted on the
#   `ticket_id` key.
# - `/analytics/tickets` surfaces the dominant `source` per ticket, with
#   `branch` vs `branch_numeric` propagated correctly.
# - `/analytics/tickets/{ID}` echoes the same source in its detail
#   payload (feeds the `Source` row in `budi stats --ticket <ID>`).
#
# Negative-path proof: revert `ProxyAttribution::resolve` in
# `crates/budi-core/src/proxy.rs` to call the old `extract_numeric_ticket`
# / write `"Unassigned"`, and this script must fail on either the
# numeric-source assertion or the no-Unassigned assertion.
set -euo pipefail

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
PROXY_PORT=19881
UPSTREAM_PORT=19336

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
            "id": "msg_e2e_221",
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
ALPHA_SESSION="e2e-221-alpha-$TS"
NUM_SESSION="e2e-221-numeric-$TS"
MAIN_SESSION="e2e-221-main-$TS"

ALPHA_BRANCH="PROJ-221-ticket-attribution"
NUM_BRANCH="feature/1234"
MAIN_BRANCH="main"

send() {
  local label="$1"; local session="$2"; local branch="$3"
  echo "[e2e] proxy request: $label (session=$session, branch=$branch)"
  local status
  status=$(curl -s -o "$TMPDIR_ROOT/${label}.json" -w "%{http_code}" --max-time 5 \
    -X POST \
    -H "content-type: application/json" \
    -H "x-budi-session: $session" \
    -H "x-budi-branch: $branch" \
    -H "x-budi-repo: github.com/siropkin/budi" \
    -d '{"model":"claude-sonnet-4-6","messages":[{"role":"user","content":"hi"}]}' \
    "http://127.0.0.1:$PROXY_PORT/v1/messages")
  if [[ "$status" != "200" ]]; then
    echo "[e2e] FAIL: $label returned $status" >&2
    tail -n 60 "$TMPDIR_ROOT/daemon.log" >&2 || true
    exit 1
  fi
}

send "alpha"    "$ALPHA_SESSION" "$ALPHA_BRANCH"
send "numeric"  "$NUM_SESSION"   "$NUM_BRANCH"
send "main"     "$MAIN_SESSION"  "$MAIN_BRANCH"

# Let spawn_blocking DB writes land.
sleep 1

DB="$HOME/.local/share/budi/analytics.db"
if [[ ! -f "$DB" ]]; then
  echo "[e2e] FAIL: analytics DB missing at $DB" >&2
  tail -n 40 "$TMPDIR_ROOT/daemon.log" >&2 || true
  exit 1
fi

tag_value() {
  local mid="$1"; local key="$2"
  sqlite3 "$DB" "SELECT value FROM tags WHERE message_id = '$mid' AND key = '$key' LIMIT 1;"
}

tag_count() {
  local mid="$1"; local key="$2"
  sqlite3 "$DB" "SELECT COUNT(*) FROM tags WHERE message_id = '$mid' AND key = '$key';"
}

# --- 1. Alphanumeric branch -> ticket_id + ticket_prefix + ticket_source=branch.
MSG_ALPHA=$(sqlite3 "$DB" "SELECT id FROM messages WHERE session_id = '$ALPHA_SESSION' ORDER BY timestamp DESC LIMIT 1;")
if [[ -z "$MSG_ALPHA" ]]; then
  echo "[e2e] FAIL: no message row for alpha session" >&2
  exit 1
fi

TID_A=$(tag_value "$MSG_ALPHA" ticket_id)
TPREF_A=$(tag_value "$MSG_ALPHA" ticket_prefix)
TSRC_A=$(tag_value "$MSG_ALPHA" ticket_source)
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

# --- 2. Numeric-only branch -> ticket_id=1234, ticket_source=branch_numeric,
#        and NO ticket_prefix (there is no alphabetic prefix).
MSG_NUM=$(sqlite3 "$DB" "SELECT id FROM messages WHERE session_id = '$NUM_SESSION' ORDER BY timestamp DESC LIMIT 1;")
if [[ -z "$MSG_NUM" ]]; then
  echo "[e2e] FAIL: no message row for numeric session" >&2
  exit 1
fi

TID_N=$(tag_value "$MSG_NUM" ticket_id)
TSRC_N=$(tag_value "$MSG_NUM" ticket_source)
TPREF_N_COUNT=$(tag_count "$MSG_NUM" ticket_prefix)
echo "[e2e] numeric row tags: ticket_id=$TID_N ticket_source=$TSRC_N ticket_prefix_count=$TPREF_N_COUNT"

if [[ "$TID_N" != "1234" ]]; then
  echo "[e2e] FAIL: expected ticket_id=1234 on numeric row, got '$TID_N'" >&2
  sqlite3 "$DB" "SELECT key, value FROM tags WHERE message_id = '$MSG_NUM';" >&2
  exit 1
fi
if [[ "$TSRC_N" != "branch_numeric" ]]; then
  echo "[e2e] FAIL: expected ticket_source=branch_numeric on numeric row, got '$TSRC_N'" >&2
  echo "[e2e] (this guards the R1.3 unified extractor — pre-R1.3 the proxy wrote no source at all)" >&2
  exit 1
fi
if [[ "$TPREF_N_COUNT" != "0" ]]; then
  echo "[e2e] FAIL: expected no ticket_prefix on numeric-only ticket, got $TPREF_N_COUNT" >&2
  exit 1
fi
echo "[e2e] OK: numeric branch -> ticket_id=1234 source=branch_numeric, no prefix"

# --- 3. `main` branch -> no ticket tags at all.
MSG_MAIN=$(sqlite3 "$DB" "SELECT id FROM messages WHERE session_id = '$MAIN_SESSION' ORDER BY timestamp DESC LIMIT 1;")
TID_M_COUNT=$(tag_count "$MSG_MAIN" ticket_id)
TSRC_M_COUNT=$(tag_count "$MSG_MAIN" ticket_source)
echo "[e2e] main row tag counts: ticket_id=$TID_M_COUNT ticket_source=$TSRC_M_COUNT"
if [[ "$TID_M_COUNT" != "0" ]]; then
  echo "[e2e] FAIL: 'main' should never emit a ticket_id tag, got $TID_M_COUNT" >&2
  sqlite3 "$DB" "SELECT key, value FROM tags WHERE message_id = '$MSG_MAIN';" >&2
  exit 1
fi
if [[ "$TSRC_M_COUNT" != "0" ]]; then
  echo "[e2e] FAIL: 'main' should never emit a ticket_source tag, got $TSRC_M_COUNT" >&2
  exit 1
fi
echo "[e2e] OK: integration branch 'main' emits no ticket_* tags"

# --- 4. No row, anywhere, carries the legacy "Unassigned" sentinel on
#        the ticket_id key. Pre-R1.3 the proxy wrote that literal when
#        attribution failed; R1.3 drops it on write.
UNASSIGNED=$(sqlite3 "$DB" "SELECT COUNT(*) FROM tags WHERE key = 'ticket_id' AND value = 'Unassigned';")
if [[ "$UNASSIGNED" != "0" ]]; then
  echo "[e2e] FAIL: 'Unassigned' ticket_id sentinel leaked into DB ($UNASSIGNED rows)" >&2
  sqlite3 "$DB" "SELECT message_id, key, value FROM tags WHERE key = 'ticket_id' AND value = 'Unassigned';" >&2
  exit 1
fi
echo "[e2e] OK: no 'Unassigned' ticket_id rows (legacy sentinel retired)"

# --- 5. /analytics/tickets surfaces the dominant source per ticket.
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
    print("       (this guards the R1.3 analytics loader: ticket_source must "
          "propagate from the tags table to the API response)")
    sys.exit(1)

# The integration-branch request must land in `(untagged)`, not a separate
# `Unassigned` bucket.
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

# --- 6. /analytics/tickets/{id} echoes the dominant source in the detail
#        payload — this feeds the `Source` row in `budi stats --ticket <ID>`.
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
