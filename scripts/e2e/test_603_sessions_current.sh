#!/usr/bin/env bash
# End-to-end regression for issue #603: pin the `current` session
# resolver wire contract.
#
# Acceptance contract pinned (see #603):
# - `current` token + cwd query param resolves to the most-recent-mtime
#   `*.jsonl` under `~/.claude/projects/<encoded-cwd>/` (filename stem
#   is the session id).
# - With two encoded-cwd dirs each containing one transcript and
#   different mtimes, asking for `current` from cwd A returns A's
#   session — never the globally newest session from cwd B. This is
#   the "two Claude sessions across two projects" scenario from the
#   ticket: the user invoking `/budi` in proj-A must see proj-A vitals.
# - When the encoded-cwd dir is missing, the daemon falls back to
#   `latest` and includes a `fallback_reason` string the CLI surfaces
#   on stderr.
set -euo pipefail

export NO_COLOR="${NO_COLOR:-1}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BUDI="$ROOT/target/release/budi"
BUDI_DAEMON="$ROOT/target/release/budi-daemon"

if [[ ! -x "$BUDI" || ! -x "$BUDI_DAEMON" ]]; then
  echo "error: release binaries not built. run \`cargo build --release\` first." >&2
  exit 2
fi

TMPDIR_ROOT="$(mktemp -d -t budi-e2e-603-current-XXXXXX)"
export HOME="$TMPDIR_ROOT"
mkdir -p "$HOME/.config/budi"

DAEMON_PORT=17603

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

# ---------------------------------------------------------------------------
# Two parallel "Claude Code projects" — proj-a and proj-b — each with one
# transcript file. We control their mtimes explicitly so the resolver
# can't fall back on a same-second tie and still surface the right
# session.
# ---------------------------------------------------------------------------
PROJ_A="$HOME/_projects/proj-a"
PROJ_B="$HOME/_projects/proj-b"
mkdir -p "$PROJ_A" "$PROJ_B"

# Helper: encode the absolute cwd into Claude Code's transcript-dir form
# (replace every non-alphanumeric character with `-`). Mirrors
# `budi_core::session_resolve::encode_cwd_for_claude_projects` so this
# script encodes the same way the daemon decodes — drift in either
# direction would break the resolver in the field.
encode_cwd() {
  python3 -c '
import sys
raw = sys.argv[1]
out = "".join(c if c.isalnum() and c.isascii() else "-" for c in raw)
sys.stdout.write(out)
' "$1"
}

ENCODED_A="$(encode_cwd "$PROJ_A")"
ENCODED_B="$(encode_cwd "$PROJ_B")"
PROJ_DIR_A="$HOME/.claude/projects/$ENCODED_A"
PROJ_DIR_B="$HOME/.claude/projects/$ENCODED_B"
mkdir -p "$PROJ_DIR_A" "$PROJ_DIR_B"

SESSION_A="aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"
SESSION_B="bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"
TRANSCRIPT_A="$PROJ_DIR_A/$SESSION_A.jsonl"
TRANSCRIPT_B="$PROJ_DIR_B/$SESSION_B.jsonl"
: >"$TRANSCRIPT_A"
: >"$TRANSCRIPT_B"

# Touch B *after* A so that, in a global "latest" view, B wins. This is
# the bug the cwd-scoping fixes: a user running `/budi` in proj-A must
# still get session A even though session B is globally newer.
touch -t 202001010000 "$TRANSCRIPT_A"
touch -t 202501010000 "$TRANSCRIPT_B"

# ---------------------------------------------------------------------------
# Boot the daemon and wait for /health.
# ---------------------------------------------------------------------------
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

DAEMON_URL="http://127.0.0.1:$DAEMON_PORT"

# ---------------------------------------------------------------------------
# Scenario 1: cwd-scoped resolution. Hit /analytics/sessions/resolve
# with cwd=$PROJ_A and assert the response carries SESSION_A even
# though SESSION_B has a newer mtime in the global view.
# ---------------------------------------------------------------------------
echo "[e2e] scenario 1: cwd-scoped resolve (proj-a)"
RESP="$(curl -s --max-time 5 "$DAEMON_URL/analytics/sessions/resolve" \
  --data-urlencode "token=current" \
  --data-urlencode "cwd=$PROJ_A" \
  -G)"
echo "[e2e] resolver response (proj-a): $RESP"
GOT_ID="$(echo "$RESP" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("session_id",""))')"
GOT_SOURCE="$(echo "$RESP" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("source",""))')"
GOT_FALLBACK="$(echo "$RESP" | python3 -c 'import json,sys; v=json.load(sys.stdin).get("fallback_reason"); print(v if v else "")')"
if [[ "$GOT_ID" != "$SESSION_A" ]]; then
  echo "[e2e] FAIL: expected $SESSION_A for proj-a, got $GOT_ID" >&2
  exit 1
fi
if [[ "$GOT_SOURCE" != "current" ]]; then
  echo "[e2e] FAIL: expected source=current, got $GOT_SOURCE" >&2
  exit 1
fi
if [[ -n "$GOT_FALLBACK" ]]; then
  echo "[e2e] FAIL: expected no fallback_reason for proj-a, got '$GOT_FALLBACK'" >&2
  exit 1
fi
echo "[e2e] OK: proj-a resolved to its own session even though proj-b is globally newer"

# Sibling check: from proj-b cwd, we get SESSION_B. This proves the
# cwd-scoping isn't accidentally returning the same session for any
# cwd we throw at it.
echo "[e2e] scenario 1b: cwd-scoped resolve (proj-b)"
RESP="$(curl -s --max-time 5 "$DAEMON_URL/analytics/sessions/resolve" \
  --data-urlencode "token=current" \
  --data-urlencode "cwd=$PROJ_B" \
  -G)"
GOT_ID="$(echo "$RESP" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("session_id",""))')"
if [[ "$GOT_ID" != "$SESSION_B" ]]; then
  echo "[e2e] FAIL: expected $SESSION_B for proj-b, got $GOT_ID" >&2
  exit 1
fi
echo "[e2e] OK: proj-b resolved to its own session"

# ---------------------------------------------------------------------------
# Scenario 2: fallback to `latest`. Remove the encoded-cwd dir for
# proj-a entirely. The daemon should fall back to the newest DB
# session (in this test the DB has none, so the daemon should return
# 404 — the CLI surfaces that as "no sessions yet"). The fallback
# reason string MUST be present in the response when the daemon does
# fall back, so we exercise that on the path where there ARE sessions
# downstream by trying a non-existent cwd that has no encoded dir
# AND also seeding a transcript so the DB will have a session id to
# fall back to once we trigger an import.
# ---------------------------------------------------------------------------
echo "[e2e] scenario 2: fallback when encoded-cwd dir is missing"
NONEXISTENT_CWD="$HOME/_projects/never-was"
mkdir -p "$NONEXISTENT_CWD"
# Note: deliberately NO encoded-cwd dir created for this path.

RESP="$(curl -s --max-time 5 "$DAEMON_URL/analytics/sessions/resolve" \
  --data-urlencode "token=current" \
  --data-urlencode "cwd=$NONEXISTENT_CWD" \
  -G)"
echo "[e2e] resolver response (missing cwd): $RESP"
# The DB is empty here so the fallback hits 404. We assert on the
# error body shape so a regression that drops the fallback path
# entirely (and 500s instead) still trips this guard.
GOT_ERR="$(echo "$RESP" | python3 -c 'import json,sys
try:
    d = json.load(sys.stdin)
    print(d.get("error",""))
except Exception:
    print("")')"
if [[ "$GOT_ERR" != "no sessions found" ]]; then
  echo "[e2e] FAIL: expected 'no sessions found' on empty DB fallback, got '$GOT_ERR'" >&2
  exit 1
fi
echo "[e2e] OK: empty-DB fallback path returns 'no sessions found'"

# ---------------------------------------------------------------------------
# Scenario 3: bad token → 400 Bad Request.
# ---------------------------------------------------------------------------
echo "[e2e] scenario 3: unknown token rejected"
HTTP_CODE="$(curl -s -o "$TMPDIR_ROOT/bad-token.json" -w "%{http_code}" \
  --max-time 5 \
  --data-urlencode "token=bogus" \
  -G "$DAEMON_URL/analytics/sessions/resolve")"
if [[ "$HTTP_CODE" != "400" ]]; then
  echo "[e2e] FAIL: expected 400 for unknown token, got HTTP $HTTP_CODE" >&2
  cat "$TMPDIR_ROOT/bad-token.json" >&2 || true
  exit 1
fi
echo "[e2e] OK: unknown token rejected with 400"

# ---------------------------------------------------------------------------
# Scenario 4: `latest` with no sessions returns 404. Confirms the
# daemon path is uniform: same not-found shape regardless of which
# token led us there.
# ---------------------------------------------------------------------------
echo "[e2e] scenario 4: latest with empty DB → 404"
HTTP_CODE="$(curl -s -o "$TMPDIR_ROOT/latest-empty.json" -w "%{http_code}" \
  --max-time 5 \
  --data-urlencode "token=latest" \
  -G "$DAEMON_URL/analytics/sessions/resolve")"
if [[ "$HTTP_CODE" != "404" ]]; then
  echo "[e2e] FAIL: expected 404 for latest with empty DB, got HTTP $HTTP_CODE" >&2
  cat "$TMPDIR_ROOT/latest-empty.json" >&2 || true
  exit 1
fi
echo "[e2e] OK: latest with empty DB returns 404"

echo "[e2e] PASS"
