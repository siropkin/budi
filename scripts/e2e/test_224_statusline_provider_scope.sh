#!/usr/bin/env bash
# End-to-end regression for issue #224: verify the shared provider-scoped
# statusline contract (ADR-0088 §4).
#
# The 8.0 bug was that `budi statusline --format claude` (invoked by Claude
# Code's status line) returned blended multi-provider totals — so a machine
# that also used Cursor would see Cursor spend counted in the Claude Code
# statusline. This script pins the fix:
#
# - Ingest two assistant messages with the same timestamp: one from
#   `claude_code` ($5.00), one from `cursor` ($7.00).
# - Call `/analytics/statusline` three ways and assert the cost_* fields:
#     1. no provider scope      -> $12 (blended)
#     2. provider=claude_code   -> $5
#     3. provider=cursor        -> $7
# - Assert the response exposes the new rolling-window fields
#   (`cost_1d`, `cost_7d`, `cost_30d`, `provider_scope`) alongside the
#   deprecated aliases (`today_cost` / `week_cost` / `month_cost`) that
#   are populated with the same rolling values for one release of
#   backward compatibility.
#
# Negative-path check: revert the `provider` filter on
# `statusline_stats` (in `crates/budi-core/src/analytics/queries.rs`) or
# the CLI's auto-scoping for `--format claude` and this script must fail.
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

TMPDIR_ROOT="$(mktemp -d -t budi-e2e-224-XXXXXX)"
export HOME="$TMPDIR_ROOT"
mkdir -p "$HOME/.config/budi"

DAEMON_PORT=17882

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

echo "[e2e] starting budi-daemon on :$DAEMON_PORT"
RUST_LOG=warn \
  "$BUDI_DAEMON" serve \
    --host 127.0.0.1 \
    --port $DAEMON_PORT \
    >"$TMPDIR_ROOT/daemon.log" 2>&1 &
DAEMON_PID=$!

for _ in {1..50}; do
  if curl -s -o /dev/null -w "%{http_code}" --max-time 1 "http://127.0.0.1:$DAEMON_PORT/health" | grep -q "^200"; then
    break
  fi
  sleep 0.1
done

DB="$HOME/.local/share/budi/analytics.db"
if [[ ! -f "$DB" ]]; then
  # Hit /health once more to ensure migrations have run and the DB exists.
  curl -s "http://127.0.0.1:$DAEMON_PORT/health" >/dev/null || true
  sleep 0.3
fi
if [[ ! -f "$DB" ]]; then
  echo "[e2e] FAIL: daemon did not create analytics DB at $DB" >&2
  tail -n 60 "$TMPDIR_ROOT/daemon.log" >&2 || true
  exit 1
fi

# Seed two assistant rows with timestamps inside the rolling 24h window. We
# insert directly into SQLite because this test pins the analytics shape,
# not the ingest pipeline.
TS="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
sqlite3 "$DB" <<SQL
INSERT INTO messages (
  id, session_id, timestamp, role, cwd, model,
  input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
  git_branch, repo_id, provider, cost_cents, cost_confidence
) VALUES
  ('e2e-224-cc-1', 'e2e-224-cc-sess', '$TS',
   'assistant', '/repo', 'claude-sonnet',
   100, 50, 0, 0, 'main', 'repo-1', 'claude_code', 500.0, 'exact'),
  ('e2e-224-cursor-1', 'e2e-224-cursor-sess', '$TS',
   'assistant', '/repo', 'cursor-gpt-4',
   100, 50, 0, 0, 'main', 'repo-1', 'cursor', 700.0, 'exact');
SQL

echo "[e2e] seeded 2 assistant rows ($TS): claude_code=\$5.00, cursor=\$7.00"

fetch() {
  local label="$1"; shift
  curl -s --max-time 5 "http://127.0.0.1:$DAEMON_PORT/analytics/statusline$*" \
    | tee "$TMPDIR_ROOT/${label}.json"
  echo
}

assert_cost() {
  local label="$1" field="$2" want="$3"
  local got
  got=$(python3 -c "import json,sys; d=json.load(open('$TMPDIR_ROOT/${label}.json')); print(d.get('$field'))")
  if [[ "$got" != "$want" ]]; then
    echo "[e2e] FAIL: ${label}.${field} expected $want, got $got" >&2
    cat "$TMPDIR_ROOT/${label}.json" >&2
    exit 1
  fi
  echo "[e2e] OK: ${label}.${field} == $want"
}

echo "[e2e] fetch: unscoped"
fetch unscoped ""
assert_cost unscoped cost_1d 12.0
assert_cost unscoped cost_7d 12.0
assert_cost unscoped cost_30d 12.0
# Deprecated aliases must mirror the rolling values for one-release compat.
assert_cost unscoped today_cost 12.0
assert_cost unscoped week_cost 12.0
assert_cost unscoped month_cost 12.0

echo "[e2e] fetch: provider=claude_code"
fetch claude "?provider=claude_code"
assert_cost claude cost_1d 5.0
assert_cost claude cost_7d 5.0
assert_cost claude cost_30d 5.0
SCOPE=$(python3 -c "import json; print(json.load(open('$TMPDIR_ROOT/claude.json')).get('provider_scope'))")
if [[ "$SCOPE" != "claude_code" ]]; then
  echo "[e2e] FAIL: claude.provider_scope expected 'claude_code', got '$SCOPE'" >&2
  exit 1
fi
echo "[e2e] OK: claude.provider_scope == claude_code"
ACTIVE=$(python3 -c "import json; print(json.load(open('$TMPDIR_ROOT/claude.json')).get('active_provider'))")
if [[ "$ACTIVE" != "claude_code" ]]; then
  echo "[e2e] FAIL: claude.active_provider expected 'claude_code', got '$ACTIVE'" >&2
  exit 1
fi
echo "[e2e] OK: claude.active_provider == claude_code"

echo "[e2e] fetch: provider=cursor"
fetch cursor "?provider=cursor"
assert_cost cursor cost_1d 7.0
assert_cost cursor cost_7d 7.0
assert_cost cursor cost_30d 7.0
ACTIVE=$(python3 -c "import json; print(json.load(open('$TMPDIR_ROOT/cursor.json')).get('active_provider'))")
if [[ "$ACTIVE" != "cursor" ]]; then
  echo "[e2e] FAIL: cursor.active_provider expected 'cursor', got '$ACTIVE'" >&2
  exit 1
fi
echo "[e2e] OK: cursor.active_provider == cursor"

echo "[e2e] PASS"
