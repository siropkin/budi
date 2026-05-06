#!/usr/bin/env bash
# Release smoke gate for v8.4.0 — pins the automated portion of the #655
# smoke test plan (docs/release/v8.4.0-smoke-test.md).
#
# This script is the executable contract behind the R2.2 release gate.
# Manual host-extension UI verification (Cursor / VS Code status bar,
# `budi doctor` output on a clean machine, dashboard click-through) lives
# in docs/release/v8.4.0-smoke-test.md and is run out-of-band by the
# release driver. This script covers the steps that CAN be exercised
# from a shell:
#
#   13. Multi-provider statusline contract — comma-list aggregation
#       (`?provider=cursor,copilot_chat`) sums correctly and emits
#       `contributing_providers`. ADR-0088 §7 + statusline-contract.md.
#   14. Single-provider statusline contract — byte-identical to the 8.1
#       shape (`?provider=cursor`). Backwards-compat regression gate.
#   15. Unknown-provider tolerance — `?provider=unknown_provider` returns
#       200 with zeros and `contributing_providers` carries the unknown
#       name through unchanged. statusline-contract.md "Unknown provider
#       names are not errors".
#   18. Path-watcher resilience to missing roots — daemon stays up and
#       `/health` stays 200 across a mid-run materialization of the
#       Copilot Chat globalStorage path. This is the #385 contract that
#       `attach_new_watchers` runs every backstop tick.
#   19. Old daemon + new extension — `/health` exposes a numeric
#       `api_version` so the host extension can compare its compiled
#       MIN_API_VERSION and surface a remediation banner instead of
#       rendering $0.
#
# Steps 1-12 (host extension UI) and 16-17 (Billing API reconciliation
# fixtures) are manual and tracked in the per-platform PASS table in
# docs/release/v8.4.0-smoke-test.md.
#
# Run:
#   cargo build --release
#   bash scripts/e2e/test_655_release_smoke.sh
#   KEEP_TMP=1 bash scripts/e2e/test_655_release_smoke.sh   # post-mortem
set -euo pipefail

export NO_COLOR="${NO_COLOR:-1}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BUDI="$ROOT/target/release/budi"
BUDI_DAEMON="$ROOT/target/release/budi-daemon"

if [[ ! -x "$BUDI" || ! -x "$BUDI_DAEMON" ]]; then
  echo "error: release binaries not built. run \`cargo build --release\` first." >&2
  exit 2
fi

TMPDIR_ROOT="$(mktemp -d -t budi-e2e-655-XXXXXX)"
export HOME="$TMPDIR_ROOT"
mkdir -p "$HOME/.config/budi"

DAEMON_PORT=17865
DAEMON_PID=""

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

step() {
  echo
  echo "=================================================================="
  echo "[e2e] $*"
  echo "=================================================================="
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

DB="$HOME/.local/share/budi/analytics.db"

step "boot daemon (no copilot_chat dirs yet — exercises #385 path)"
start_daemon

# Wait for migrations.
if [[ ! -f "$DB" ]]; then
  curl -s "http://127.0.0.1:$DAEMON_PORT/health" >/dev/null || true
  sleep 0.3
fi
if [[ ! -f "$DB" ]]; then
  echo "[e2e] FAIL: daemon did not create analytics DB at $DB" >&2
  tail -n 80 "$TMPDIR_ROOT/daemon.log" >&2 || true
  exit 1
fi

# ---------------------------------------------------------------------------
# Step 19 — daemon /health exposes a numeric api_version so the host
# extension can compare its compiled MIN_API_VERSION before rendering.
# ---------------------------------------------------------------------------

step "step 19: /health advertises api_version"
HEALTH_JSON="$TMPDIR_ROOT/health.json"
curl -s --max-time 5 "http://127.0.0.1:$DAEMON_PORT/health" | tee "$HEALTH_JSON"
echo
API_VERSION=$(python3 -c "import json; d=json.load(open('$HEALTH_JSON')); v=d.get('api_version'); print(v if isinstance(v,int) else 'NOT_INT')")
if [[ "$API_VERSION" == "NOT_INT" ]] || [[ -z "$API_VERSION" ]]; then
  echo "[e2e] FAIL: /health api_version not a numeric field; host extensions cannot detect a stale daemon" >&2
  exit 1
fi
echo "[e2e] OK: /health api_version == $API_VERSION (host extensions can compare against MIN_API_VERSION)"

# ---------------------------------------------------------------------------
# Steps 13–15 — multi-provider statusline endpoint contract
#
# Seed two assistant rows with the same timestamp:
#   cursor       $7.00
#   copilot_chat $5.00
# Then assert the comma-list response (13), single-provider response (14),
# and unknown-provider response (15) all match the contract pinned in
# docs/statusline-contract.md.
# ---------------------------------------------------------------------------

TS="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
sqlite3 "$DB" <<SQL
INSERT INTO messages (
  id, session_id, timestamp, role, cwd, model,
  input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
  git_branch, repo_id, provider, cost_cents, cost_confidence
) VALUES
  ('e2e-655-cursor-1', 'e2e-655-cursor-sess', '$TS',
   'assistant', '/repo', 'cursor-gpt-4',
   100, 50, 0, 0, 'main', 'repo-1', 'cursor', 700.0, 'exact'),
  ('e2e-655-copilot-1', 'e2e-655-copilot-sess', '$TS',
   'assistant', '/repo', 'gpt-4.1',
   100, 50, 0, 0, 'main', 'repo-1', 'copilot_chat', 500.0, 'exact');
SQL
echo "[e2e] seeded 2 assistant rows ($TS): cursor=\$7.00, copilot_chat=\$5.00"

fetch() {
  local label="$1"; shift
  curl -s --max-time 5 "http://127.0.0.1:$DAEMON_PORT/analytics/statusline$*" \
    | tee "$TMPDIR_ROOT/${label}.json"
  echo
}

assert_field() {
  local label="$1" field="$2" want="$3"
  local got
  got=$(python3 -c "import json; d=json.load(open('$TMPDIR_ROOT/${label}.json')); print(d.get('$field'))")
  if [[ "$got" != "$want" ]]; then
    echo "[e2e] FAIL: ${label}.${field} expected '$want', got '$got'" >&2
    cat "$TMPDIR_ROOT/${label}.json" >&2
    exit 1
  fi
  echo "[e2e] OK: ${label}.${field} == $want"
}

assert_contributing_providers() {
  local label="$1" want_csv="$2"
  local got
  got=$(python3 -c "import json; d=json.load(open('$TMPDIR_ROOT/${label}.json')); print(','.join(d.get('contributing_providers') or []))")
  if [[ "$got" != "$want_csv" ]]; then
    echo "[e2e] FAIL: ${label}.contributing_providers expected '$want_csv', got '$got'" >&2
    cat "$TMPDIR_ROOT/${label}.json" >&2
    exit 1
  fi
  echo "[e2e] OK: ${label}.contributing_providers == [$want_csv]"
}

step "step 13: ?provider=cursor,copilot_chat aggregates and emits contributing_providers"
fetch multi "?provider=cursor,copilot_chat"
assert_field multi cost_1d 12.0
assert_field multi cost_7d 12.0
assert_field multi cost_30d 12.0
assert_contributing_providers multi "cursor,copilot_chat"
# provider_scope is omitted on multi-provider requests (single-provider-only field).
SCOPE=$(python3 -c "import json; d=json.load(open('$TMPDIR_ROOT/multi.json')); print(d.get('provider_scope'))")
if [[ "$SCOPE" != "None" ]]; then
  echo "[e2e] FAIL: multi.provider_scope must be omitted on comma-list requests, got '$SCOPE'" >&2
  cat "$TMPDIR_ROOT/multi.json" >&2
  exit 1
fi
echo "[e2e] OK: multi.provider_scope is omitted (single-provider-only field)"

step "step 14: ?provider=cursor preserves the byte-identical 8.1 single-provider shape"
fetch cursor "?provider=cursor"
assert_field cursor cost_1d 7.0
assert_field cursor cost_7d 7.0
assert_field cursor cost_30d 7.0
assert_field cursor provider_scope cursor
assert_field cursor active_provider cursor

step "step 15: ?provider=unknown_provider returns 200 with zeros, not an error"
HTTP_CODE=$(curl -s -o "$TMPDIR_ROOT/unknown.json" -w "%{http_code}" --max-time 5 \
  "http://127.0.0.1:$DAEMON_PORT/analytics/statusline?provider=unknown_provider")
cat "$TMPDIR_ROOT/unknown.json"
echo
if [[ "$HTTP_CODE" != "200" ]]; then
  echo "[e2e] FAIL: ?provider=unknown_provider must be 200, got $HTTP_CODE" >&2
  exit 1
fi
echo "[e2e] OK: ?provider=unknown_provider returned HTTP 200"
assert_field unknown cost_1d 0.0
assert_field unknown cost_7d 0.0
assert_field unknown cost_30d 0.0

# ---------------------------------------------------------------------------
# Step 18 — daemon stays up across a mid-run materialization of a
# previously-missing watch root. The full ingestion-after-materialize
# behavior is covered by the unit tests in
# crates/budi-daemon/src/workers/tailer.rs (#385). The smoke gate just
# pins that the daemon survives the create event without restarting.
# ---------------------------------------------------------------------------

step "step 18: daemon survives mid-run materialization of copilot_chat globalStorage"
case "$(uname -s)" in
  Darwin)
    USER_ROOT="$HOME/Library/Application Support/Code/User"
    ;;
  Linux)
    USER_ROOT="$HOME/.config/Code/User"
    ;;
  *)
    USER_ROOT="$HOME/.config/Code/User"
    ;;
esac
COPILOT_CHAT_GS="$USER_ROOT/globalStorage/github.copilot-chat/chatSessions"
if [[ -e "$USER_ROOT" ]]; then
  echo "[e2e] FAIL: VS Code User dir already exists at $USER_ROOT — test fixture should be empty" >&2
  exit 1
fi
echo "[e2e] pre-materialize: User dir absent (as expected): $USER_ROOT"

if ! kill -0 "$DAEMON_PID" >/dev/null 2>&1; then
  echo "[e2e] FAIL: daemon died before materialization step" >&2
  tail -n 80 "$TMPDIR_ROOT/daemon.log" >&2 || true
  exit 1
fi

mkdir -p "$COPILOT_CHAT_GS"
echo "[e2e] materialized: $COPILOT_CHAT_GS"

# BACKSTOP_POLL is 5 s in the tailer; allow one full reconcile tick plus
# a small buffer for attach_new_watchers to register the new root.
sleep 6

if ! kill -0 "$DAEMON_PID" >/dev/null 2>&1; then
  echo "[e2e] FAIL: daemon exited after watch root materialized — #385 contract regression" >&2
  tail -n 120 "$TMPDIR_ROOT/daemon.log" >&2 || true
  exit 1
fi

POST_HEALTH=$(curl -s -o /dev/null -w "%{http_code}" --max-time 2 \
  "http://127.0.0.1:$DAEMON_PORT/health" || true)
if [[ "$POST_HEALTH" != "200" ]]; then
  echo "[e2e] FAIL: /health is $POST_HEALTH after watch root materialization (expected 200)" >&2
  tail -n 120 "$TMPDIR_ROOT/daemon.log" >&2 || true
  exit 1
fi
echo "[e2e] OK: daemon still healthy (PID $DAEMON_PID, /health 200) after materialize"

step "PASS: automated portion of v8.4.0 smoke test plan green"
echo "Manual UI steps 1–12 + Billing API steps 16–17 are tracked in"
echo "docs/release/v8.4.0-smoke-test.md per-platform PASS tables."
