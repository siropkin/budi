#!/usr/bin/env bash
# Release smoke gate for v8.4.0 / v8.4.1 — pins the automated portion of
# the #655 (8.4.0) and #672 (8.4.1) smoke test plans
# (docs/release/v8.4.0-smoke-test.md, docs/release/v8.4.1-smoke-test.md).
#
# This script is the executable contract behind the R2.2 release gate.
# Manual host-extension UI verification (Cursor / VS Code status bar,
# `budi doctor` output on a clean machine, dashboard click-through) lives
# in docs/release/v8.4.{0,1}-smoke-test.md and is run out-of-band by the
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
#   20. Real-shape Copilot Chat parser pipeline (8.4.1 R1.5 #672) — drop
#       the R1.2 (#669) `vscode_chat_0_47_0.jsonl` fixture into the
#       daemon's watched workspaceStorage path and assert rows materialize
#       through parser → tailer → DB. Would have FAILed on the 8.4.0
#       broken parser; PASSes on the post-#R1.1 reducer. Also pins the
#       #681 enrichment contract: drop a sibling `workspace.json`
#       pointing at $ROOT and assert cwd / repo_id / git_branch are
#       non-null on every materialized row (pre-#681 they were all
#       NULL because the parser hard-skipped workspace enrichment).
#   21. Streaming-truncation resilience (8.4.1 R1.5 #672) — append a
#       kind:2 stub (no completionTokens yet) and assert no row emits;
#       then append the kind:1 completionTokens patch and assert exactly
#       one new row materializes. Pins the no-emit-until-completion
#       contract from #R1.1 against the live tailer.
#   22. Doctor AMBER signal (8.4.1 R1.5 #672 / R1.3 #670) — verify
#       `budi doctor --format json` reports `pass` for the
#       `tailer rows / Copilot Chat` check after step 20, then simulate
#       the 8.4.0 broken-parser state (bytes consumed, zero rows
#       emitted) by clearing copilot_chat messages while leaving
#       tail_offsets intact, and assert the same check flips to `warn`
#       with the parser-regression hint. The state we simulate is the
#       exact one a v3 parser would have produced on a v4 mutation log,
#       so this is the gate that would have caught 8.4.0 before tag.
#       Also asserts the new `pre-boot history detected / Copilot Chat`
#       INFO check (8.4.2 #693) — silent while messages exist, fires
#       `info` with the `budi db import` backfill hint when tail_offsets
#       advance is present but messages are empty for the provider.
#
# Steps 1-12 (host extension UI) and 16-17 (Billing API reconciliation
# fixtures) are manual and tracked in the per-platform PASS table in
# docs/release/v8.4.{0,1}-smoke-test.md.
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

# ---------------------------------------------------------------------------
# Step 20 — real-shape Copilot Chat parser pipeline (8.4.1 R1.5, #672).
#
# Drop the R1.2 (#669) `vscode_chat_0_47_0.jsonl` fixture under the
# daemon's watched `workspaceStorage/<hash>/chatSessions/` path and
# assert rows materialize via parser → tailer → DB. The 8.4.0 R2.2 gate
# (steps 13–15) seeded `messages` directly with sqlite3 to exercise the
# wire shape of the multi-provider statusline endpoint, which is the
# right test for ADR-0088 §7 contract enforcement but bypasses the
# parser entirely. A parser regression of the kind documented in #668
# is, by construction, invisible to a smoke gate that doesn't run the
# parser. Step 20 closes that gap.
# ---------------------------------------------------------------------------

step "step 20: real-shape Copilot Chat fixture flows through the parser pipeline"

FIXTURE_SRC="$ROOT/crates/budi-core/src/providers/copilot_chat/fixtures/vscode_chat_0_47_0.jsonl"
FIXTURE_EXPECTED="$ROOT/crates/budi-core/src/providers/copilot_chat/fixtures/vscode_chat_0_47_0.expected.json"
if [[ ! -f "$FIXTURE_SRC" || ! -f "$FIXTURE_EXPECTED" ]]; then
  echo "[e2e] FAIL: R1.2 fixtures missing — expected:" >&2
  echo "  $FIXTURE_SRC" >&2
  echo "  $FIXTURE_EXPECTED" >&2
  exit 1
fi

# The fixture's kind:0 snapshot pins sessionId
# 35a2ecbc-1144-4ac2-993e-1ca6850280a3 (sanitized from a real
# github.copilot-chat 0.47.0 capture per ADR-0092 §2.3 v4). Place under
# a freshly-materialized workspaceStorage hash dir so the tailer
# attaches a watcher on the next backstop tick (#385) and ingests the
# file from offset 0 (post-boot materialization is treated as live
# content, not history — see tailer.rs `backstop_scan`).
FIXTURE_SESSION_ID="35a2ecbc-1144-4ac2-993e-1ca6850280a3"
FIXTURE_HASH_DIR="$USER_ROOT/workspaceStorage/r1-5-smoke-hash"
FIXTURE_DEST_DIR="$FIXTURE_HASH_DIR/chatSessions"
FIXTURE_DEST="$FIXTURE_DEST_DIR/$FIXTURE_SESSION_ID.jsonl"
mkdir -p "$FIXTURE_DEST_DIR"
cp "$FIXTURE_SRC" "$FIXTURE_DEST"
# #681: drop a sibling workspace.json so the parser resolves cwd via the
# workspaceStorage enrichment path and the GitEnricher fills in repo_id
# from the resulting cwd. Point at $ROOT (the budi repo itself) so the
# enrichment chain has a real git repo with an origin remote — the only
# way the post-step assertion below can pin all three fields non-null.
cat >"$FIXTURE_HASH_DIR/workspace.json" <<JSON
{
  "folder": "file://$ROOT"
}
JSON
echo "[e2e] dropped fixture: $FIXTURE_DEST"
echo "[e2e] dropped workspace.json -> $ROOT"

EXPECTED_COUNT=$(python3 -c "import json; d=json.load(open('$FIXTURE_EXPECTED')); print(len(d))")
EXPECTED_OUTPUT_TOKENS=$(python3 -c "import json; d=json.load(open('$FIXTURE_EXPECTED')); print(sum(int(r['output_tokens']) for r in d))")
echo "[e2e] fixture expects $EXPECTED_COUNT requests, $EXPECTED_OUTPUT_TOKENS total output tokens"

# BACKSTOP_POLL is 5 s in the tailer; allow one full reconcile tick for
# attach_new_watchers + backstop_scan + ingest_messages, plus buffer.
sleep 7

ROW_COUNT=$(sqlite3 "$DB" \
  "SELECT COUNT(*) FROM messages WHERE provider='copilot_chat' AND session_id='$FIXTURE_SESSION_ID';")
if [[ "$ROW_COUNT" != "$EXPECTED_COUNT" ]]; then
  echo "[e2e] FAIL: copilot_chat rows for session $FIXTURE_SESSION_ID = $ROW_COUNT, expected $EXPECTED_COUNT" >&2
  echo "[e2e] daemon log tail:" >&2
  tail -n 120 "$TMPDIR_ROOT/daemon.log" >&2 || true
  echo "[e2e] tail_offsets snapshot:" >&2
  sqlite3 "$DB" "SELECT provider, path, byte_offset, last_seen FROM tail_offsets WHERE provider='copilot_chat';" >&2 || true
  exit 1
fi
echo "[e2e] OK: parser materialized $ROW_COUNT row(s) for session $FIXTURE_SESSION_ID"

OUTPUT_TOKENS=$(sqlite3 "$DB" \
  "SELECT COALESCE(SUM(output_tokens), 0) FROM messages WHERE provider='copilot_chat' AND session_id='$FIXTURE_SESSION_ID';")
if [[ "$OUTPUT_TOKENS" != "$EXPECTED_OUTPUT_TOKENS" ]]; then
  echo "[e2e] FAIL: SUM(output_tokens) for session $FIXTURE_SESSION_ID = $OUTPUT_TOKENS, expected $EXPECTED_OUTPUT_TOKENS" >&2
  exit 1
fi
echo "[e2e] OK: SUM(output_tokens) for session $FIXTURE_SESSION_ID == $OUTPUT_TOKENS"

# #681 acceptance: workspace.json -> cwd, repo_id, git_branch on every
# materialized row. Pre-#681 every copilot_chat row landed with all three
# NULL because the parser hard-skipped workspace enrichment (cwd: None
# in build_message[_for_request]); the GitEnricher then no-op'd because
# `cwd is None`. With the workspace.json sibling pointing at $ROOT (a
# real git repo), the parser resolves cwd, the parser-side HEAD read
# resolves git_branch, and the GitEnricher resolves repo_id from cwd.
NULL_CWD=$(sqlite3 "$DB" \
  "SELECT COUNT(*) FROM messages WHERE provider='copilot_chat' AND session_id='$FIXTURE_SESSION_ID' AND cwd IS NULL;")
NULL_REPO=$(sqlite3 "$DB" \
  "SELECT COUNT(*) FROM messages WHERE provider='copilot_chat' AND session_id='$FIXTURE_SESSION_ID' AND repo_id IS NULL;")
NULL_BRANCH=$(sqlite3 "$DB" \
  "SELECT COUNT(*) FROM messages WHERE provider='copilot_chat' AND session_id='$FIXTURE_SESSION_ID' AND git_branch IS NULL;")
if [[ "$NULL_CWD" != "0" || "$NULL_REPO" != "0" || "$NULL_BRANCH" != "0" ]]; then
  echo "[e2e] FAIL: copilot_chat cwd/repo_id/git_branch enrichment regressed (#681)" >&2
  echo "  rows with NULL cwd:        $NULL_CWD (expected 0)" >&2
  echo "  rows with NULL repo_id:    $NULL_REPO (expected 0)" >&2
  echo "  rows with NULL git_branch: $NULL_BRANCH (expected 0)" >&2
  sqlite3 "$DB" \
    "SELECT id, cwd, repo_id, git_branch FROM messages WHERE provider='copilot_chat' AND session_id='$FIXTURE_SESSION_ID' LIMIT 5;" >&2 || true
  exit 1
fi
echo "[e2e] OK: cwd / repo_id / git_branch all non-null on $EXPECTED_COUNT row(s) (#681)"

# #686 acceptance: both row roles materialize from the canonical fixture.
# Pre-#686 the parser hard-coded role='assistant' on every emit and
# /analytics/providers reported user_messages: 0 for copilot_chat. The
# canonical fixture now carries synthetic message.text on its first two
# requests (clearly fake, see SANITIZATION CHECKLIST in the fixture
# header), so the parser is expected to emit at least one user row and
# at least one assistant row for the session.
EXPECTED_USER_ROWS=$(python3 -c "import json; d=json.load(open('$FIXTURE_EXPECTED')); print(sum(1 for r in d if r.get('role') == 'user'))")
EXPECTED_ASSISTANT_ROWS=$(python3 -c "import json; d=json.load(open('$FIXTURE_EXPECTED')); print(sum(1 for r in d if r.get('role') == 'assistant'))")
USER_ROWS=$(sqlite3 "$DB" \
  "SELECT COUNT(*) FROM messages WHERE provider='copilot_chat' AND session_id='$FIXTURE_SESSION_ID' AND role='user';")
ASSISTANT_ROWS=$(sqlite3 "$DB" \
  "SELECT COUNT(*) FROM messages WHERE provider='copilot_chat' AND session_id='$FIXTURE_SESSION_ID' AND role='assistant';")
if [[ "$USER_ROWS" != "$EXPECTED_USER_ROWS" || "$ASSISTANT_ROWS" != "$EXPECTED_ASSISTANT_ROWS" ]]; then
  echo "[e2e] FAIL: copilot_chat row roles mismatch (#686)" >&2
  echo "  user rows:       $USER_ROWS (expected $EXPECTED_USER_ROWS)" >&2
  echo "  assistant rows:  $ASSISTANT_ROWS (expected $EXPECTED_ASSISTANT_ROWS)" >&2
  sqlite3 "$DB" \
    "SELECT id, role, parent_uuid, prompt_category FROM messages WHERE provider='copilot_chat' AND session_id='$FIXTURE_SESSION_ID' ORDER BY id LIMIT 20;" >&2 || true
  exit 1
fi
echo "[e2e] OK: both row roles materialize ($USER_ROWS user, $ASSISTANT_ROWS assistant) (#686)"

# #687 acceptance: tool data extracted from result.metadata.toolCallRounds
# lands in the tags table. The canonical fixture now carries a synthetic
# toolCallRounds entry on request dc9f930d (`replace_string_in_file` +
# `read_file`, file paths `src/auth.rs` + `src/main.rs`); pre-#687 the
# parser hard-coded `tool_names: Vec::new()` / `tool_use_ids: Vec::new()`
# / `tool_files: Vec::new()` so `budi stats files --provider copilot_chat`
# was permanently empty. Assert the per-tag rows materialize via the
# pipeline enrichers (ToolEnricher + FileEnricher).
EXPECTED_TOOL_ROWS=$(python3 -c "import json; d=json.load(open('$FIXTURE_EXPECTED')); print(sum(len(r.get('tool_names', [])) for r in d if r.get('role') == 'assistant'))")
EXPECTED_TOOL_USE_ID_ROWS=$(python3 -c "import json; d=json.load(open('$FIXTURE_EXPECTED')); print(sum(len(r.get('tool_use_ids', [])) for r in d if r.get('role') == 'assistant'))")
EXPECTED_FILE_PATH_ROWS=$(python3 -c "import json; d=json.load(open('$FIXTURE_EXPECTED')); print(sum(len(r.get('tool_files', [])) for r in d if r.get('role') == 'assistant'))")
TOOL_ROWS=$(sqlite3 "$DB" \
  "SELECT COUNT(*) FROM tags t JOIN messages m ON m.id = t.message_id \
   WHERE m.provider='copilot_chat' AND m.session_id='$FIXTURE_SESSION_ID' AND t.key='tool';")
TOOL_USE_ID_ROWS=$(sqlite3 "$DB" \
  "SELECT COUNT(*) FROM tags t JOIN messages m ON m.id = t.message_id \
   WHERE m.provider='copilot_chat' AND m.session_id='$FIXTURE_SESSION_ID' AND t.key='tool_use_id';")
FILE_PATH_ROWS=$(sqlite3 "$DB" \
  "SELECT COUNT(*) FROM tags t JOIN messages m ON m.id = t.message_id \
   WHERE m.provider='copilot_chat' AND m.session_id='$FIXTURE_SESSION_ID' AND t.key='file_path';")
if [[ "$TOOL_ROWS" != "$EXPECTED_TOOL_ROWS" || "$TOOL_USE_ID_ROWS" != "$EXPECTED_TOOL_USE_ID_ROWS" || "$FILE_PATH_ROWS" != "$EXPECTED_FILE_PATH_ROWS" ]]; then
  echo "[e2e] FAIL: copilot_chat tool-attribution tags mismatch (#687)" >&2
  echo "  tool tags:         $TOOL_ROWS (expected $EXPECTED_TOOL_ROWS)" >&2
  echo "  tool_use_id tags:  $TOOL_USE_ID_ROWS (expected $EXPECTED_TOOL_USE_ID_ROWS)" >&2
  echo "  file_path tags:    $FILE_PATH_ROWS (expected $EXPECTED_FILE_PATH_ROWS)" >&2
  sqlite3 "$DB" \
    "SELECT t.key, t.value FROM tags t JOIN messages m ON m.id = t.message_id \
     WHERE m.provider='copilot_chat' AND m.session_id='$FIXTURE_SESSION_ID' \
     AND t.key IN ('tool','tool_use_id','file_path') ORDER BY t.key, t.value;" >&2 || true
  exit 1
fi
echo "[e2e] OK: tool/tool_use_id/file_path tags materialized ($TOOL_ROWS/$TOOL_USE_ID_ROWS/$FILE_PATH_ROWS) (#687)"

# ---------------------------------------------------------------------------
# Step 21 — streaming-truncation resilience (8.4.1 R1.5, #672).
#
# Append a kind:2 stub for a brand-new request (no completionTokens
# inline, no kind:1 patch yet) to the live session file. Assert the
# tailer's reducer does NOT emit a row for the in-flight request — it
# must wait for a token-bearing patch. Then append the kind:1
# completionTokens patch and assert exactly one new row materializes.
# This pins the "wait for the completion token to arrive" contract
# from #R1.1 against the live tailer (the unit-test sibling lives in
# crates/budi-core/src/providers/copilot_chat.rs).
# ---------------------------------------------------------------------------

step "step 21: in-flight kind:2 stub does not emit; kind:1 completionTokens patch emits exactly one row"

STREAMING_REQUEST_ID="request_e2e655-r1-5-streaming-stub"
# Index 9 in the requests array — step 20's full fixture produced a 9-item
# array (one initial request from kind:0 + eight requests added by kind:2
# splices in lines 5-9 of the fixture, so indices 0-8 are populated).
STREAMING_INDEX=9
STREAMING_STUB_LINE='{"kind":2,"k":["requests"],"v":[{"requestId":"'"$STREAMING_REQUEST_ID"'","timestamp":1778172900000,"modelId":"copilot/auto","modelState":{"value":0},"timeSpentWaiting":1778172900000}]}'
printf '%s\n' "$STREAMING_STUB_LINE" >>"$FIXTURE_DEST"
echo "[e2e] appended kind:2 stub for $STREAMING_REQUEST_ID (no completionTokens yet)"

sleep 7

PRE_PATCH_COUNT=$(sqlite3 "$DB" \
  "SELECT COUNT(*) FROM messages WHERE provider='copilot_chat' AND session_id='$FIXTURE_SESSION_ID';")
if [[ "$PRE_PATCH_COUNT" != "$EXPECTED_COUNT" ]]; then
  echo "[e2e] FAIL: streaming stub emitted prematurely — count for session $FIXTURE_SESSION_ID = $PRE_PATCH_COUNT, expected $EXPECTED_COUNT (no new row until completionTokens arrives)" >&2
  exit 1
fi
echo "[e2e] OK: in-flight stub did NOT emit ($PRE_PATCH_COUNT row(s), unchanged from step 20)"

STREAMING_PATCH_LINE='{"kind":1,"k":["requests",'"$STREAMING_INDEX"',"completionTokens"],"v":42}'
printf '%s\n' "$STREAMING_PATCH_LINE" >>"$FIXTURE_DEST"
echo "[e2e] appended kind:1 completionTokens=42 patch at requests[$STREAMING_INDEX]"

sleep 7

POST_PATCH_COUNT=$(sqlite3 "$DB" \
  "SELECT COUNT(*) FROM messages WHERE provider='copilot_chat' AND session_id='$FIXTURE_SESSION_ID';")
EXPECTED_POST_PATCH=$((EXPECTED_COUNT + 1))
if [[ "$POST_PATCH_COUNT" != "$EXPECTED_POST_PATCH" ]]; then
  echo "[e2e] FAIL: completionTokens patch did not emit exactly one new row — count = $POST_PATCH_COUNT, expected $EXPECTED_POST_PATCH" >&2
  exit 1
fi
echo "[e2e] OK: completionTokens patch emitted exactly one new row ($PRE_PATCH_COUNT → $POST_PATCH_COUNT)"

# ---------------------------------------------------------------------------
# Step 22 — doctor AMBER signal (8.4.1 R1.5 #672 / R1.3 #670).
#
# After steps 20 and 21, `tail_offsets` for copilot_chat shows non-zero
# advance and a recent `last_seen`, AND `messages` carries fresh rows
# (the step 13 seed insert is also recent). The R1.3 doctor check
# `tailer rows / Copilot Chat` must report `pass` here.
#
# Then we simulate the exact state the 8.4.0 broken parser would have
# produced — bytes consumed, no rows emitted — by deleting every
# copilot_chat row from `messages` while leaving `tail_offsets` intact.
# The state below is byte-equivalent to running the v3 parser against a
# v4 (mutation-log) fixture: tailer happily advances bytes, parser
# returns zero rows. The R1.3 check must flip to `warn` (AMBER) with
# the parser-regression hint.
#
# We don't need to actually swap to a broken parser binary — the doctor
# signal is a downstream-of-parser observation (`tail_offsets` advance
# AND `messages` empty), so simulating that state directly is faithful
# to the gate's purpose ("would 8.4.0 have FAILed this gate before
# tag"). The same gate would catch the 8.4.0 regression unmodified.
#
# Note on daemon-port plumbing: `budi doctor` reads daemon_port from
# `BudiConfig::default()` (= 7878) when no per-repo config.toml is
# resolvable, but the test daemon is on $DAEMON_PORT (17865) for
# isolation from a developer's real daemon. So the doctor's
# `daemon health` check probes 7878 and fails (we point BUDI_DAEMON_BIN
# at a missing path so it doesn't auto-start a competing daemon either),
# which makes doctor exit 2 because of the daemon-health FAIL row. The
# JSON payload is still printed before the exit, so we capture it with
# `|| true` and parse it regardless. The `tailer rows / Copilot Chat`
# check doesn't depend on the daemon being up — it reads tail_offsets
# and messages from the analytics DB directly via the schema check's
# connection, which is what we're asserting on here.
# ---------------------------------------------------------------------------

step "step 22: budi doctor flips to AMBER for tailer rows / Copilot Chat under broken-parser state"

# Helper: extract the status of a check by name from doctor's JSON.
doctor_status_for() {
  local json="$1"
  local check_name="$2"
  python3 -c "
import json, sys
with open('$json') as f:
    d = json.load(f)
for c in d.get('checks', []):
    if c.get('name') == '$check_name':
        print(c.get('status', ''))
        sys.exit(0)
print('NOT_FOUND')
"
}

# Helper: extract the detail string of a check by name.
doctor_detail_for() {
  local json="$1"
  local check_name="$2"
  python3 -c "
import json
with open('$json') as f:
    d = json.load(f)
for c in d.get('checks', []):
    if c.get('name') == '$check_name':
        print(c.get('detail', ''))
        break
"
}

run_doctor() {
  local out="$1"
  # BUDI_DAEMON_BIN points at a missing path so doctor does not try to
  # auto-start a competing daemon on the default port (7878) — we want
  # doctor to read the analytics DB only, not interact with a daemon.
  # Doctor will exit 2 because of the daemon-health FAIL row; the JSON
  # is emitted before the exit, so `|| true` keeps the script alive.
  BUDI_DAEMON_BIN="/definitely/missing/budi-daemon" \
    "$BUDI" doctor --format json >"$out" 2>"$out.stderr" || true
  if [[ ! -s "$out" ]]; then
    echo "[e2e] FAIL: \`budi doctor --format json\` produced no JSON output" >&2
    cat "$out.stderr" >&2 || true
    exit 1
  fi
}

DOCTOR_PASS_JSON="$TMPDIR_ROOT/doctor-pass.json"
run_doctor "$DOCTOR_PASS_JSON"

PASS_STATUS=$(doctor_status_for "$DOCTOR_PASS_JSON" "tailer rows / Copilot Chat")
if [[ "$PASS_STATUS" != "pass" ]]; then
  echo "[e2e] FAIL: post-step-20 'tailer rows / Copilot Chat' status = '$PASS_STATUS', expected 'pass'" >&2
  echo "[e2e] doctor json:" >&2
  cat "$DOCTOR_PASS_JSON" >&2 || true
  exit 1
fi
echo "[e2e] OK: doctor reports 'tailer rows / Copilot Chat' = pass after rows landed"

# #693: with messages already landed for copilot_chat (steps 13/20/21), the
# pre-boot history INFO check is idempotently silent — passes with
# "nothing to backfill". Pins the discoverability contract: the INFO state
# is reserved for the truly-pre-boot scenario.
PREBOOT_PASS_STATUS=$(doctor_status_for "$DOCTOR_PASS_JSON" "pre-boot history detected / Copilot Chat")
if [[ "$PREBOOT_PASS_STATUS" != "pass" ]]; then
  echo "[e2e] FAIL: post-step-20 'pre-boot history detected / Copilot Chat' status = '$PREBOOT_PASS_STATUS', expected 'pass' (idempotent silence once messages exist)" >&2
  echo "[e2e] doctor json:" >&2
  cat "$DOCTOR_PASS_JSON" >&2 || true
  exit 1
fi
echo "[e2e] OK: doctor reports 'pre-boot history detected / Copilot Chat' = pass while messages exist"

# Simulate the 8.4.0 broken-parser state: tail_offsets show recent
# byte advance, but `messages` carries zero copilot_chat rows. The
# DELETE clears the step-13 seed plus the parser-emitted rows; we keep
# `tail_offsets` rows untouched so the doctor's classify_tailer_rows
# logic sees `advanced_bytes > 0 && last_seen recent && rows_in_window
# == 0` — the AMBER trigger from R1.3.
sqlite3 "$DB" "DELETE FROM messages WHERE provider='copilot_chat';"
TAIL_BYTES=$(sqlite3 "$DB" "SELECT COALESCE(SUM(byte_offset), 0) FROM tail_offsets WHERE provider='copilot_chat';")
if [[ "$TAIL_BYTES" == "0" ]]; then
  echo "[e2e] FAIL: tail_offsets shows zero bytes for copilot_chat after step 21 — AMBER trigger requires advanced_bytes > 0" >&2
  exit 1
fi
echo "[e2e] simulated broken-parser state: rows cleared, tail_offsets shows $TAIL_BYTES bytes advanced"

DOCTOR_AMBER_JSON="$TMPDIR_ROOT/doctor-amber.json"
run_doctor "$DOCTOR_AMBER_JSON"

AMBER_STATUS=$(doctor_status_for "$DOCTOR_AMBER_JSON" "tailer rows / Copilot Chat")
if [[ "$AMBER_STATUS" != "warn" ]]; then
  echo "[e2e] FAIL: under broken-parser state, 'tailer rows / Copilot Chat' status = '$AMBER_STATUS', expected 'warn' (AMBER)" >&2
  echo "[e2e] doctor json:" >&2
  cat "$DOCTOR_AMBER_JSON" >&2 || true
  exit 1
fi
echo "[e2e] OK: doctor flipped 'tailer rows / Copilot Chat' to warn (AMBER) under broken-parser state"

AMBER_DETAIL=$(doctor_detail_for "$DOCTOR_AMBER_JSON" "tailer rows / Copilot Chat")
if ! grep -Fq "ADR-0092 §2.6 / MIN_API_VERSION" <<<"$AMBER_DETAIL"; then
  echo "[e2e] FAIL: AMBER detail missing the parser-regression hint (ADR-0092 §2.6 / MIN_API_VERSION)" >&2
  echo "[e2e] detail: $AMBER_DETAIL" >&2
  exit 1
fi
echo "[e2e] OK: AMBER detail carries the parser-regression hint"

# #693: same observable state (`tail_offsets` advanced + `messages` empty
# for the provider) is also the pre-boot-history trigger. The INFO check
# fires with the `budi db import` backfill hint — distinct severity and
# remediation from the AMBER tailer-rows check above.
PREBOOT_INFO_STATUS=$(doctor_status_for "$DOCTOR_AMBER_JSON" "pre-boot history detected / Copilot Chat")
if [[ "$PREBOOT_INFO_STATUS" != "info" ]]; then
  echo "[e2e] FAIL: under empty-messages state, 'pre-boot history detected / Copilot Chat' status = '$PREBOOT_INFO_STATUS', expected 'info'" >&2
  echo "[e2e] doctor json:" >&2
  cat "$DOCTOR_AMBER_JSON" >&2 || true
  exit 1
fi
echo "[e2e] OK: doctor flipped 'pre-boot history detected / Copilot Chat' to info under empty-messages state"

PREBOOT_INFO_DETAIL=$(doctor_detail_for "$DOCTOR_AMBER_JSON" "pre-boot history detected / Copilot Chat")
if ! grep -Fq "budi db import" <<<"$PREBOOT_INFO_DETAIL"; then
  echo "[e2e] FAIL: INFO detail missing the backfill hint (\`budi db import\`)" >&2
  echo "[e2e] detail: $PREBOOT_INFO_DETAIL" >&2
  exit 1
fi
echo "[e2e] OK: INFO detail carries the \`budi db import\` backfill hint"

step "PASS: automated portion of v8.4.{0,1} smoke test plan green"
echo "Manual UI steps 1–12 + Billing API steps 16–17 are tracked in"
echo "docs/release/v8.4.{0,1}-smoke-test.md per-platform PASS tables."
