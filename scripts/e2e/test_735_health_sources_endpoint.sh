#!/usr/bin/env bash
# End-to-end regression for issue #735: verify
# `GET /health/sources?surface=<id>` returns the on-disk paths the daemon
# is currently tailing for the requested surface, and that the unfiltered
# form returns every surface in the grouped shape.
#
# Contract pinned (from the issue body):
#   GET /health/sources?surface=jetbrains
#   → 200 { "surface": "jetbrains", "paths": ["/abs/...", "/abs/..."] }
#   GET /health/sources
#   → 200 { "surfaces": [ { "surface": "...", "paths": [...] }, ... ] }
#
# The plugin caps the response at 64 KB and times out at 3 s; we don't
# replay those caps here (they're plugin-side), but we do assert the
# happy-path response shape and surface filtering using a JetBrains
# AI Assistant `aiAssistant/chats/` directory seeded under a temp HOME.
#
# Negative-path: drop the route registration in
# `crates/budi-daemon/src/main.rs` and this script must fail with the
# `404 Not Found` branch.
set -euo pipefail

export NO_COLOR="${NO_COLOR:-1}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BUDI_DAEMON="$ROOT/target/release/budi-daemon"

if [[ ! -x "$BUDI_DAEMON" ]]; then
  echo "error: release binary not built. run \`cargo build --release\` first." >&2
  exit 2
fi

TMPDIR_ROOT="$(mktemp -d -t budi-e2e-735-XXXXXX)"
export HOME="$TMPDIR_ROOT"

# Seed a JetBrains AI Assistant chats dir so the JetBrains provider's
# watch_roots() returns a non-empty path. macOS + Linux both probe
# `${HOME}/Library/Application Support/JetBrains/<Product><Year>/aiAssistant/chats`
# or `${HOME}/.config/JetBrains/<Product><Year>/aiAssistant/chats`; seed
# both so the test works on either OS.
JB_MAC_DIR="$HOME/Library/Application Support/JetBrains/IdeaIC2026.1/aiAssistant/chats"
JB_LINUX_DIR="$HOME/.config/JetBrains/IdeaIC2026.1/aiAssistant/chats"
mkdir -p "$JB_MAC_DIR" "$JB_LINUX_DIR"

DAEMON_PORT=17935

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

if ! curl -s -o /dev/null -w "%{http_code}" --max-time 1 "http://127.0.0.1:$DAEMON_PORT/health" | grep -q "^200"; then
  echo "[e2e] FAIL: daemon /health did not return 200 in time" >&2
  tail -n 60 "$TMPDIR_ROOT/daemon.log" >&2 || true
  exit 1
fi

echo "[e2e] querying /health/sources?surface=jetbrains"
FILTERED_BODY="$TMPDIR_ROOT/sources-jetbrains.json"
HTTP_CODE="$(curl -s -o "$FILTERED_BODY" -w "%{http_code}" --max-time 3 \
  "http://127.0.0.1:$DAEMON_PORT/health/sources?surface=jetbrains")"
if [[ "$HTTP_CODE" != "200" ]]; then
  echo "[e2e] FAIL: expected 200 from /health/sources?surface=jetbrains, got $HTTP_CODE" >&2
  cat "$FILTERED_BODY" >&2 || true
  exit 1
fi

# Response shape: { "surface": "jetbrains", "paths": [...] }
python3 - "$FILTERED_BODY" <<'PY'
import json
import sys
path = sys.argv[1]
with open(path) as f:
    body = json.load(f)
if body.get("surface") != "jetbrains":
    print(f"[e2e] FAIL: surface field is {body.get('surface')!r}, want 'jetbrains'", file=sys.stderr)
    sys.exit(1)
paths = body.get("paths")
if not isinstance(paths, list):
    print(f"[e2e] FAIL: paths is not a list: {paths!r}", file=sys.stderr)
    sys.exit(1)
if not paths:
    print("[e2e] FAIL: expected at least one jetbrains path; got empty list", file=sys.stderr)
    sys.exit(1)
for p in paths:
    if not isinstance(p, str) or not p.startswith("/"):
        print(f"[e2e] FAIL: path entry not an absolute string: {p!r}", file=sys.stderr)
        sys.exit(1)
print(f"[e2e] OK: filtered jetbrains response has {len(paths)} path(s)")
PY

echo "[e2e] querying /health/sources (unfiltered)"
GROUPED_BODY="$TMPDIR_ROOT/sources-all.json"
HTTP_CODE="$(curl -s -o "$GROUPED_BODY" -w "%{http_code}" --max-time 3 \
  "http://127.0.0.1:$DAEMON_PORT/health/sources")"
if [[ "$HTTP_CODE" != "200" ]]; then
  echo "[e2e] FAIL: expected 200 from /health/sources, got $HTTP_CODE" >&2
  cat "$GROUPED_BODY" >&2 || true
  exit 1
fi

# Response shape: { "surfaces": [ { "surface": "...", "paths": [...] }, ... ] }
python3 - "$GROUPED_BODY" <<'PY'
import json
import sys
path = sys.argv[1]
with open(path) as f:
    body = json.load(f)
surfaces = body.get("surfaces")
if not isinstance(surfaces, list):
    print(f"[e2e] FAIL: surfaces is not a list: {surfaces!r}", file=sys.stderr)
    sys.exit(1)
canonical = {"vscode", "cursor", "jetbrains", "terminal", "unknown"}
found_jb = False
for group in surfaces:
    if group.get("surface") not in canonical:
        print(f"[e2e] FAIL: non-canonical surface {group.get('surface')!r}", file=sys.stderr)
        sys.exit(1)
    if not isinstance(group.get("paths"), list):
        print(f"[e2e] FAIL: paths for {group.get('surface')!r} is not a list", file=sys.stderr)
        sys.exit(1)
    if group["surface"] == "jetbrains" and group["paths"]:
        found_jb = True
if not found_jb:
    print("[e2e] FAIL: grouped response did not include jetbrains paths", file=sys.stderr)
    sys.exit(1)
print(f"[e2e] OK: grouped response has {len(surfaces)} surface group(s)")
PY

echo "[e2e] querying /health/sources?surface=does-not-exist"
EMPTY_BODY="$TMPDIR_ROOT/sources-empty.json"
HTTP_CODE="$(curl -s -o "$EMPTY_BODY" -w "%{http_code}" --max-time 3 \
  "http://127.0.0.1:$DAEMON_PORT/health/sources?surface=does-not-exist")"
if [[ "$HTTP_CODE" != "200" ]]; then
  echo "[e2e] FAIL: expected 200 from unknown-surface query, got $HTTP_CODE" >&2
  cat "$EMPTY_BODY" >&2 || true
  exit 1
fi
python3 - "$EMPTY_BODY" <<'PY'
import json
import sys
path = sys.argv[1]
with open(path) as f:
    body = json.load(f)
if body.get("surface") != "does-not-exist":
    print(f"[e2e] FAIL: surface field is {body.get('surface')!r}", file=sys.stderr)
    sys.exit(1)
if body.get("paths") != []:
    print(f"[e2e] FAIL: expected empty paths for unknown surface; got {body.get('paths')!r}", file=sys.stderr)
    sys.exit(1)
print("[e2e] OK: unknown surface yields empty paths")
PY

echo "[e2e] PASS"
