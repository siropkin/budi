#!/usr/bin/env bash
# Smoke gate for #695 — DNS-rebinding defense via Host header allowlist.
#
# Boots a release-built daemon on a dedicated port and asserts that
# requests carrying a non-local Host header are rejected with 403 on:
#   - GET  /health                       (public, was un-gated by require_loopback)
#   - GET  /analytics/sessions           (public analytics surface)
#   - POST /admin/integrations/install   (loopback-only protected route)
#
# Negative-prove the gate: revert the require_local_host layer in
# `crates/budi-daemon/src/main.rs::build_router` and rerun this script —
# every assertion below should flip from PASS to FAIL.
#
# Run:
#   cargo build --release
#   bash scripts/e2e/test_695_host_header_validation.sh
#   KEEP_TMP=1 bash scripts/e2e/test_695_host_header_validation.sh   # post-mortem
set -euo pipefail

export NO_COLOR="${NO_COLOR:-1}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BUDI_DAEMON="$ROOT/target/release/budi-daemon"

if [[ ! -x "$BUDI_DAEMON" ]]; then
  echo "error: release daemon binary not built. run \`cargo build --release\` first." >&2
  exit 2
fi

TMPDIR_ROOT="$(mktemp -d -t budi-e2e-695-XXXXXX)"
export HOME="$TMPDIR_ROOT"
mkdir -p "$HOME/.config/budi"

DAEMON_PORT=17895
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

step "boot daemon on :$DAEMON_PORT"
start_daemon

# ---------------------------------------------------------------------------
# Step 1 — baseline: a request with the canonical local Host header is 200.
# ---------------------------------------------------------------------------

step "step 1: GET /health with Host: 127.0.0.1:$DAEMON_PORT returns 200"
GOOD_HEALTH=$(curl -s -o /dev/null -w "%{http_code}" --max-time 5 \
  -H "Host: 127.0.0.1:$DAEMON_PORT" \
  "http://127.0.0.1:$DAEMON_PORT/health")
if [[ "$GOOD_HEALTH" != "200" ]]; then
  echo "[e2e] FAIL: /health with valid Host = $GOOD_HEALTH (expected 200)" >&2
  tail -n 80 "$TMPDIR_ROOT/daemon.log" >&2 || true
  exit 1
fi
echo "[e2e] OK: /health with valid Host returned 200"

# ---------------------------------------------------------------------------
# Step 2 — DNS-rebinding shape: the peer is 127.0.0.1 (curl's TCP target),
# but the Host header advertises an attacker-controlled name. require_loopback
# alone would let this through; require_local_host must reject it on every
# affected route. We inspect both the status and the body shape so a future
# refactor that returns 403 from a different source is also caught.
# ---------------------------------------------------------------------------

assert_rejected() {
  local label="$1"
  local method="$2"
  local path="$3"
  local out="$TMPDIR_ROOT/${label}.out"
  local code

  code=$(curl -s -o "$out" -w "%{http_code}" --max-time 5 \
    -X "$method" \
    -H "Host: rebound.example" \
    "http://127.0.0.1:$DAEMON_PORT$path")

  if [[ "$code" != "403" ]]; then
    echo "[e2e] FAIL: $method $path with rebound Host returned $code (expected 403)" >&2
    cat "$out" >&2 || true
    tail -n 80 "$TMPDIR_ROOT/daemon.log" >&2 || true
    exit 1
  fi

  # Body must match the contract from the ticket acceptance:
  #   { "ok": false, "error": "invalid Host header" }
  local err
  err=$(python3 -c "import json; d=json.load(open('$out')); print(d.get('ok'), d.get('error'))" 2>/dev/null || echo "PARSE_ERROR")
  if [[ "$err" != "False invalid Host header" ]]; then
    echo "[e2e] FAIL: $method $path body did not match contract — got: $err" >&2
    cat "$out" >&2 || true
    exit 1
  fi
  echo "[e2e] OK: $method $path with rebound Host -> 403 invalid Host header"
}

step "step 2: GET /health with Host: rebound.example returns 403"
assert_rejected health GET /health

step "step 3: GET /analytics/sessions with Host: rebound.example returns 403"
assert_rejected sessions GET /analytics/sessions

step "step 4: POST /admin/integrations/install with Host: rebound.example returns 403"
# This is the rebinding shape that pre-#695 was reachable: peer is
# loopback (127.0.0.1) so require_loopback admits the request, the Host
# header is the attacker's rebound DNS name. The middleware closes the
# gap before the handler ever spawns `budi integrations install`.
assert_rejected admin_install POST /admin/integrations/install

step "step 5: warn line was emitted to daemon.log for each blocked request"
# Pin the operator-visibility contract: each rejection should leave a
# tracing::warn! line so ops can grep for it. Three blocked requests in
# steps 2–4, so we expect at least three matching lines.
WARN_LINES=$(grep -c "blocked request with non-local Host header" "$TMPDIR_ROOT/daemon.log" || true)
if [[ "$WARN_LINES" -lt 3 ]]; then
  echo "[e2e] FAIL: expected >= 3 warn lines for blocked Host headers, saw $WARN_LINES" >&2
  tail -n 120 "$TMPDIR_ROOT/daemon.log" >&2 || true
  exit 1
fi
echo "[e2e] OK: $WARN_LINES warn line(s) recorded for blocked Host headers"

step "PASS: #695 Host-header allowlist rejects rebound names on public, analytics, and admin routes"
