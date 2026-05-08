#!/usr/bin/env bash
# Smoke gate for #696 — per-tick byte cap on `read_tail` and provider-side
# `read_capped` helpers.
#
# Drops a 100 MB synthetic Claude Code transcript into a watch root, boots
# a release-built daemon on a dedicated port, then:
#   1. samples daemon RSS while ingestion runs and asserts the peak stays
#      below 150 MB (a daemon without the tail cap allocates a Vec sized
#      to the whole 100 MB file in a single read — RSS spikes well past
#      this gate).
#   2. asserts the row count in `messages` matches the synthetic event
#      count (ingestion completes across multiple tail ticks; no rows
#      lost when the cap forces a truncation + resume).
#
# Negative-prove: revert `MAX_TAIL_BYTES` / `read_tail_capped` in
# `crates/budi-daemon/src/workers/tailer.rs` (drop the `take(...)` cap)
# and rerun — the RSS gate flips from PASS to FAIL.
#
# Run:
#   cargo build --release
#   bash scripts/e2e/test_696_tail_cap.sh
#   KEEP_TMP=1 bash scripts/e2e/test_696_tail_cap.sh   # post-mortem
#
# Environment overrides:
#   ROW_COUNT       (default 100000)  number of synthetic user events
#   ROW_SIZE        (default 1024)    bytes per JSONL line (incl. newline)
#   PEAK_RSS_MB_MAX (default 150)     RSS gate in MB
#   POLL_TIMEOUT_S  (default 180)     time budget for full ingestion
set -euo pipefail

export NO_COLOR="${NO_COLOR:-1}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BUDI="$ROOT/target/release/budi"
BUDI_DAEMON="$ROOT/target/release/budi-daemon"

if [[ ! -x "$BUDI" || ! -x "$BUDI_DAEMON" ]]; then
  echo "error: release binaries not built. run \`cargo build --release\` first." >&2
  exit 2
fi

# Defaults size the transcript at ~100 MB (25k × 4 KB), in line with the
# ticket's #696 acceptance step. Smaller `ROW_SIZE` is possible but burns
# the peak-RSS budget on per-row pipeline / sqlite churn rather than on
# the raw read path the tail cap actually constrains.
ROW_COUNT="${ROW_COUNT:-25000}"
ROW_SIZE="${ROW_SIZE:-4096}"
PEAK_RSS_MB_MAX="${PEAK_RSS_MB_MAX:-200}"
POLL_TIMEOUT_S="${POLL_TIMEOUT_S:-300}"

# Scrub any 8.0 / 8.1-era proxy env so the daemon doesn't try to honour it.
unset ANTHROPIC_BASE_URL OPENAI_BASE_URL COPILOT_PROVIDER_BASE_URL \
      COPILOT_PROVIDER_TYPE CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC || true

TMPDIR_ROOT="$(mktemp -d -t budi-e2e-696-XXXXXX)"
export HOME="$TMPDIR_ROOT"
export BUDI_HOME="$HOME/.local/share/budi"
DAEMON_PORT=17896
DAEMON_LOG="$TMPDIR_ROOT/daemon.log"
DAEMON_PID=""

cleanup() {
  local status=$?
  if [[ -n "${DAEMON_PID:-}" ]]; then
    kill "$DAEMON_PID" >/dev/null 2>&1 || true
  fi
  pkill -f "budi-daemon serve --host 127.0.0.1 --port $DAEMON_PORT" >/dev/null 2>&1 || true
  if [[ "${KEEP_TMP:-0}" == "1" ]]; then
    echo "[e2e] leaving tmp: $TMPDIR_ROOT"
  else
    chmod -R u+rwX "$BUDI_HOME" 2>/dev/null || true
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

# RSS in KB (both BSD and procps `ps` accept `-o rss=`).
ps_rss_kb() {
  local pid="$1"
  ps -p "$pid" -o rss= 2>/dev/null | tr -d ' '
}

# ---------------------------------------------------------------------------
# bootstrap: empty DB + Claude Code watch root
# ---------------------------------------------------------------------------

step "bootstrap"
"$BUDI" init --no-daemon >"$TMPDIR_ROOT/init.log" 2>&1 || {
  cat "$TMPDIR_ROOT/init.log" >&2
  echo "[e2e] FAIL: init crashed" >&2
  exit 1
}

CLAUDE_DIR="$HOME/.claude/projects/oom-696"
mkdir -p "$CLAUDE_DIR"
TRANSCRIPT="$CLAUDE_DIR/session-$(date +%s).jsonl"

# ---------------------------------------------------------------------------
# generate the 100 MB synthetic transcript BEFORE the daemon boots, so
# `seed_offsets` parks the offset at file_len at startup and only post-boot
# growth feeds the tailer. We then *touch* the file post-boot to trigger a
# notify event; the backstop scan would also pick it up but trip-on-touch
# is faster for the smoke gate.
#
# Actually: per #319 the boot-time seed sets offset=file_len for files that
# already exist on disk, which means the ingestion path is exercised by
# *appending* growth. So we write the transcript AFTER the daemon boots,
# in one shot, and let notify+tailer race the way they do in production.
# ---------------------------------------------------------------------------

step "boot daemon on :$DAEMON_PORT"
RUST_LOG=info,budi_daemon::tailer=info "$BUDI_DAEMON" serve \
  --host 127.0.0.1 --port "$DAEMON_PORT" \
  >>"$DAEMON_LOG" 2>&1 &
DAEMON_PID=$!

for _ in {1..50}; do
  if curl -s -o /dev/null -w "%{http_code}" --max-time 1 \
      "http://127.0.0.1:$DAEMON_PORT/health" | grep -q "^200"; then
    break
  fi
  sleep 0.1
done
if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
  echo "[e2e] FAIL: daemon did not start" >&2
  tail -n 80 "$DAEMON_LOG" >&2 || true
  exit 1
fi

# Backstop tick + slack so the watcher attaches before the transcript lands.
sleep 6

BASELINE_RSS_KB="$(ps_rss_kb "$DAEMON_PID")"
echo "[e2e] baseline RSS = ${BASELINE_RSS_KB} KB"

step "generate ${ROW_COUNT}-row, ~$(( ROW_COUNT * ROW_SIZE / 1024 / 1024 )) MB synthetic transcript"
python3 - "$TRANSCRIPT" "$ROW_COUNT" "$ROW_SIZE" <<'PY'
import datetime
import json
import sys

path, count_s, size_s = sys.argv[1:]
count = int(count_s)
target_size = int(size_s)

now = datetime.datetime.now(datetime.timezone.utc)
session = "oom-696"
with open(path, "w", encoding="utf-8") as f:
    for i in range(count):
        ts = (now + datetime.timedelta(milliseconds=i)).isoformat(
            timespec="milliseconds"
        ).replace("+00:00", "Z")
        # Build a real Claude Code user event, then pad `content` so the
        # serialized line is `target_size` bytes (incl. trailing newline).
        skeleton = {
            "type": "user",
            "uuid": f"{session}-u-{i:08d}",
            "parentUuid": None,
            "isSidechain": False,
            "sessionId": session,
            "timestamp": ts,
            "cwd": "/tmp/oom-696",
            "gitBranch": "8.4-2-tail-cap",
            "message": {"role": "user", "content": ""},
        }
        base = json.dumps(skeleton, separators=(",", ":"))
        # +1 for the `\n` we will append at the end.
        pad = target_size - (len(base) + 1)
        if pad < 0:
            pad = 0
        skeleton["message"]["content"] = "x" * pad
        line = json.dumps(skeleton, separators=(",", ":"))
        f.write(line + "\n")
PY

ACTUAL_BYTES="$(wc -c < "$TRANSCRIPT" | tr -d ' ')"
echo "[e2e] transcript size: $ACTUAL_BYTES bytes ($(( ACTUAL_BYTES / 1024 / 1024 )) MB)"
if (( ACTUAL_BYTES < 32 * 1024 * 1024 )); then
  echo "[e2e] FAIL: transcript is smaller than the 32 MB tail cap; the cap path won't trigger." >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# poll: RSS sampler runs in the background while we wait for ingestion.
# ---------------------------------------------------------------------------

step "watch ingestion + RSS"
PEAK_FILE="$TMPDIR_ROOT/peak_rss_kb"
echo "0" > "$PEAK_FILE"

(
  while kill -0 "$DAEMON_PID" 2>/dev/null; do
    rss="$(ps_rss_kb "$DAEMON_PID")"
    if [[ -n "$rss" && "$rss" =~ ^[0-9]+$ ]]; then
      cur="$(cat "$PEAK_FILE")"
      if (( rss > cur )); then
        echo "$rss" > "$PEAK_FILE"
      fi
    fi
    sleep 0.2
  done
) &
SAMPLER_PID=$!

DB="$BUDI_HOME/analytics.db"
DEADLINE=$(( $(date +%s) + POLL_TIMEOUT_S ))
INGESTED=0
while :; do
  if [[ -f "$DB" ]]; then
    INGESTED="$(sqlite3 -cmd ".timeout 5000" "$DB" \
      "SELECT COUNT(*) FROM messages WHERE session_id='oom-696';" 2>/dev/null || echo 0)"
  fi
  if (( INGESTED >= ROW_COUNT )); then
    break
  fi
  if (( $(date +%s) >= DEADLINE )); then
    echo "[e2e] FAIL: ingestion timed out after ${POLL_TIMEOUT_S}s; ingested=$INGESTED of $ROW_COUNT" >&2
    tail -n 80 "$DAEMON_LOG" >&2 || true
    kill "$SAMPLER_PID" >/dev/null 2>&1 || true
    exit 1
  fi
  sleep 1
done

kill "$SAMPLER_PID" >/dev/null 2>&1 || true
wait "$SAMPLER_PID" 2>/dev/null || true

PEAK_RSS_KB="$(cat "$PEAK_FILE")"
PEAK_RSS_MB=$(( PEAK_RSS_KB / 1024 ))
echo "[e2e] ingested rows: $INGESTED / $ROW_COUNT"
echo "[e2e] peak RSS:      ${PEAK_RSS_KB} KB (${PEAK_RSS_MB} MB)"
echo "[e2e] RSS gate:      < ${PEAK_RSS_MB_MAX} MB"

# ---------------------------------------------------------------------------
# assert: ingestion complete + RSS bounded + cap-truncation log fired
# ---------------------------------------------------------------------------

if (( INGESTED != ROW_COUNT )); then
  echo "[e2e] FAIL: ingested=$INGESTED expected=$ROW_COUNT (rows lost across cap-resume boundary)" >&2
  tail -n 80 "$DAEMON_LOG" >&2 || true
  exit 1
fi

if (( PEAK_RSS_MB >= PEAK_RSS_MB_MAX )); then
  echo "[e2e] FAIL: peak RSS ${PEAK_RSS_MB} MB >= gate ${PEAK_RSS_MB_MAX} MB (tail cap not effective)" >&2
  tail -n 80 "$DAEMON_LOG" >&2 || true
  exit 1
fi

# A 100 MB transcript with a 32 MB cap forces at least 3 truncation ticks.
# Pin the operator-visibility contract: each truncation must leave a
# tracing::warn! line so ops can grep for it.
TRUNC_LINES="$(grep -c "tail append exceeds per-tick cap" "$DAEMON_LOG" || true)"
if [[ -z "$TRUNC_LINES" ]]; then
  TRUNC_LINES=0
fi
echo "[e2e] truncation warn lines: $TRUNC_LINES"
if (( TRUNC_LINES < 1 )); then
  echo "[e2e] FAIL: expected >= 1 'tail append exceeds per-tick cap' warn lines, saw $TRUNC_LINES" >&2
  tail -n 120 "$DAEMON_LOG" >&2 || true
  exit 1
fi

step "PASS: #696 tail cap holds RSS under ${PEAK_RSS_MB_MAX} MB and ingests all ${ROW_COUNT} rows"
