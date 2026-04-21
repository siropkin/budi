#!/usr/bin/env bash
# End-to-end regression for issue #376 / ADR-0091: verify the pricing
# manifest loader, schema migration, `budi pricing status` CLI, and
# daemon routes work against a real release binary against a throwaway
# $HOME. Network access is suppressed via BUDI_PRICING_REFRESH=0 so
# this test is deterministic offline.
#
# Specifically proves:
#   1. Migration adds `messages.pricing_source` with the documented DEFAULT.
#   2. Migration creates `pricing_manifests` with v=0 pre-manifest anchor
#      and v=1 embedded row; known_model_count on v=1 is > 0.
#   3. `GET /pricing/status` returns the golden key set (ADR-0091 §8).
#   4. `budi pricing status --format json` mirrors the daemon payload.
#   5. `budi pricing status` text output renders the expected header.
#   6. `BUDI_PRICING_REFRESH=0` keeps the embedded baseline authoritative
#      (source_label = "embedded baseline").
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BUDI="$ROOT/target/release/budi"
BUDI_DAEMON="$ROOT/target/release/budi-daemon"

if [[ ! -x "$BUDI" || ! -x "$BUDI_DAEMON" ]]; then
  echo "error: release binaries not built. run \`cargo build --release\` first." >&2
  exit 2
fi

TMPDIR_ROOT="$(mktemp -d -t budi-e2e-376-XXXXXX)"
export HOME="$TMPDIR_ROOT"
export BUDI_PRICING_REFRESH=0
export NO_COLOR=1
mkdir -p "$HOME/.config/budi"

DAEMON_PORT=17876

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
echo "[e2e] BUDI_PRICING_REFRESH=$BUDI_PRICING_REFRESH (network suppressed)"

REPO_ROOT="$HOME/repo"
mkdir -p "$REPO_ROOT/.budi"
cat >"$REPO_ROOT/.budi/budi.toml" <<CFG
daemon_host = "127.0.0.1"
daemon_port = $DAEMON_PORT
CFG

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
  echo "[e2e] FAIL: daemon did not come up" >&2
  tail -n 60 "$TMPDIR_ROOT/daemon.log" >&2 || true
  exit 1
fi

DB="$HOME/.local/share/budi/analytics.db"

# -------- 1. Migration added pricing_source column with the default ----------

PRICING_SOURCE_DFLT="$(sqlite3 "$DB" "SELECT dflt_value FROM pragma_table_info('messages') WHERE name='pricing_source';")"
if [[ "$PRICING_SOURCE_DFLT" != "'legacy:pre-manifest'" ]]; then
  echo "[e2e] FAIL: messages.pricing_source default mismatch: got '$PRICING_SOURCE_DFLT'" >&2
  exit 1
fi
echo "[e2e] OK: messages.pricing_source DEFAULT is 'legacy:pre-manifest'"

# -------- 2. pricing_manifests table seeded with v=0 and v=1 -----------------

ROW_V0="$(sqlite3 "$DB" "SELECT source FROM pricing_manifests WHERE version=0;")"
ROW_V1="$(sqlite3 "$DB" "SELECT source FROM pricing_manifests WHERE version=1;")"
V1_COUNT="$(sqlite3 "$DB" "SELECT known_model_count FROM pricing_manifests WHERE version=1;")"

if [[ "$ROW_V0" != "pre-manifest" ]]; then
  echo "[e2e] FAIL: expected v=0 source='pre-manifest', got '$ROW_V0'" >&2
  exit 1
fi
if [[ "$ROW_V1" != "embedded" ]]; then
  echo "[e2e] FAIL: expected v=1 source='embedded', got '$ROW_V1'" >&2
  exit 1
fi
if [[ -z "$V1_COUNT" || "$V1_COUNT" -lt 100 ]]; then
  echo "[e2e] FAIL: expected v=1 known_model_count >=100, got '$V1_COUNT'" >&2
  exit 1
fi
echo "[e2e] OK: pricing_manifests seeded (v=0 pre-manifest, v=1 embedded @ $V1_COUNT models)"

# -------- 3. GET /pricing/status has the golden key set ----------------------

STATUS_JSON="$(curl -s "http://127.0.0.1:$DAEMON_PORT/pricing/status")"
REQUIRED_KEYS=(source_label manifest_version fetched_at next_refresh_at known_model_count embedded_baseline_build unknown_models)
for key in "${REQUIRED_KEYS[@]}"; do
  if ! echo "$STATUS_JSON" | python3 -c "import sys, json; sys.exit(0 if '$key' in json.load(sys.stdin) else 1)"; then
    echo "[e2e] FAIL: /pricing/status missing key '$key'; body=$STATUS_JSON" >&2
    exit 1
  fi
done
echo "[e2e] OK: GET /pricing/status has all 7 golden keys"

# Source label should be "embedded baseline" when refresh is disabled and
# no on-disk cache exists yet.
SOURCE_LABEL="$(echo "$STATUS_JSON" | python3 -c "import sys, json; print(json.load(sys.stdin)['source_label'])")"
if [[ "$SOURCE_LABEL" != "embedded baseline" ]]; then
  echo "[e2e] FAIL: expected source_label='embedded baseline', got '$SOURCE_LABEL'" >&2
  exit 1
fi
echo "[e2e] OK: source_label = 'embedded baseline' with refresh disabled"

# known_model_count should match the migration's v=1 count.
STATUS_COUNT="$(echo "$STATUS_JSON" | python3 -c "import sys, json; print(json.load(sys.stdin)['known_model_count'])")"
if [[ "$STATUS_COUNT" -lt 100 ]]; then
  echo "[e2e] FAIL: expected known_model_count >=100, got $STATUS_COUNT" >&2
  exit 1
fi
echo "[e2e] OK: GET /pricing/status known_model_count=$STATUS_COUNT"

# -------- 4. `budi pricing status --format json` mirrors the daemon ----------

CLI_JSON="$(cd "$REPO_ROOT" && "$BUDI" pricing status --format json)"
CLI_COUNT="$(echo "$CLI_JSON" | python3 -c "import sys, json; print(json.load(sys.stdin)['known_model_count'])")"
if [[ "$CLI_COUNT" != "$STATUS_COUNT" ]]; then
  echo "[e2e] FAIL: CLI --format json known_model_count ($CLI_COUNT) != HTTP ($STATUS_COUNT)" >&2
  exit 1
fi
echo "[e2e] OK: budi pricing status --format json matches /pricing/status"

# -------- 5. Text output renders the Pricing manifest header ----------------

CLI_TEXT="$(cd "$REPO_ROOT" && "$BUDI" pricing status)"
if ! echo "$CLI_TEXT" | grep -q "Pricing manifest"; then
  echo "[e2e] FAIL: text output missing 'Pricing manifest' header:" >&2
  echo "$CLI_TEXT" >&2
  exit 1
fi
if ! echo "$CLI_TEXT" | grep -q "Source"; then
  echo "[e2e] FAIL: text output missing 'Source' row:" >&2
  echo "$CLI_TEXT" >&2
  exit 1
fi
echo "[e2e] OK: budi pricing status (text) renders Pricing manifest header + Source row"

# -------- 6. Refresh suppressed — daemon log carries the disabled line -------

# We asserted via source_label above; double-check the log path for the
# explicit disabled-info line so future regressions of the env-var shape
# surface here, not just in the source_label.
if ! grep -q "network refresh disabled" "$TMPDIR_ROOT/daemon.log"; then
  echo "[e2e] FAIL: daemon log missing the 'network refresh disabled' line" >&2
  tail -n 50 "$TMPDIR_ROOT/daemon.log" >&2
  exit 1
fi
echo "[e2e] OK: daemon logged 'network refresh disabled' for BUDI_PRICING_REFRESH=0"

echo "[e2e] PASS"
