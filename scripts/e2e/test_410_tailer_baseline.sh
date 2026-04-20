#!/usr/bin/env bash
# Tailer perf baseline harness for 8.2.0 — pins the methodology from issue #410.
#
# This script is the executable side of the R3.5 audit F-4 ask. #410 is a
# pure measurement ticket: "future regressions in resource use will only be
# visible against a measured baseline." The R3.5 audit reviewed the tailer's
# resource profile from code (single blocking worker, 500 ms debounce, 5 s
# backstop, no new HTTP listeners) but did not capture a live baseline, so
# this harness captures one in a reproducible way on any host.
#
# Method (one daemon, one pass):
#
#   1. Isolate: mktemp HOME + BUDI_HOME so the operator's real analytics DB
#      and autostart service are never touched; seed empty watch roots for
#      all four providers (Claude Code, Codex, Cursor, Copilot CLI) so the
#      tailer attaches a notify watcher for each.
#   2. Boot: run the already-built release `budi-daemon` on a non-default
#      port, wait for /health to go green.
#   3. Idle soak: every SAMPLE_EVERY seconds, snapshot RSS (KB), CPU %, and
#      file-descriptor count into an in-memory CSV for SOAK_SECS seconds.
#      Default 10 minutes, per #410.
#   4. Synthetic burst: append a BURST_EVENTS-event Claude Code session into
#      one watch root with BURST_GAP_MS between appends. Sample RSS / CPU /
#      FD at ~100 ms granularity during the burst and for BURST_WINDOW_SECS
#      afterwards so the post-debounce flush is captured too.
#   5. notify watcher count: grep the daemon log for the tailer's
#      `target="budi_daemon::tailer" … "watching"` spans. One span per
#      attached watch root; provider-name field tells us how they distribute.
#   6. Print a Markdown summary table suitable for pasting verbatim into a
#      #410 comment (or a wiki page under `Releases/8.2 smoke records`).
#
# This is an operator-driven instrument, not a CI regression gate. The
# default 10-minute soak is too long for PR CI; a shorter preview run can
# be had with `SOAK_SECS=60 SAMPLE_EVERY=5 ... bash scripts/e2e/test_410_...`.
#
# Usage:
#   cargo build --release
#   bash scripts/e2e/test_410_tailer_baseline.sh
#   KEEP_TMP=1 bash scripts/e2e/test_410_tailer_baseline.sh   # keep temp HOME
#   SOAK_SECS=60 SAMPLE_EVERY=5 bash scripts/e2e/test_410_tailer_baseline.sh
#
# Environment overrides:
#   SOAK_SECS         (default 600)   idle soak duration in seconds
#   SAMPLE_EVERY      (default 30)    idle soak sample interval in seconds
#   BURST_EVENTS      (default 100)   number of JSONL events replayed
#   BURST_GAP_MS      (default 1)     sleep between append events
#   BURST_WINDOW_SECS (default 30)    post-burst observation window
#   BURST_SAMPLE_MS   (default 100)   burst-phase sample interval
#   KEEP_TMP          (default 0)     when "1", keep the temp HOME/BUDI_HOME
#
# This script does not assert PASS/FAIL on specific numbers. #410 asks for
# a baseline; future regressions compare against the record this produces.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BUDI="$ROOT/target/release/budi"
BUDI_DAEMON="$ROOT/target/release/budi-daemon"

if [[ ! -x "$BUDI" || ! -x "$BUDI_DAEMON" ]]; then
  echo "error: release binaries not built. run \`cargo build --release\` first." >&2
  exit 2
fi

SOAK_SECS="${SOAK_SECS:-600}"
SAMPLE_EVERY="${SAMPLE_EVERY:-30}"
BURST_EVENTS="${BURST_EVENTS:-100}"
BURST_GAP_MS="${BURST_GAP_MS:-1}"
BURST_WINDOW_SECS="${BURST_WINDOW_SECS:-30}"
BURST_SAMPLE_MS="${BURST_SAMPLE_MS:-100}"

# Scrub env that a 8.0 / 8.1 install on the outer shell may still export.
unset ANTHROPIC_BASE_URL OPENAI_BASE_URL COPILOT_PROVIDER_BASE_URL \
      COPILOT_PROVIDER_TYPE CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC || true

TMPDIR_ROOT="$(mktemp -d -t budi-e2e-410-XXXXXX)"
export HOME="$TMPDIR_ROOT"
export BUDI_HOME="$HOME/.local/share/budi"
export CODEX_HOME="$HOME/.codex"
export COPILOT_HOME="$HOME/.copilot"

DAEMON_PORT=17941
DAEMON_LOG="$TMPDIR_ROOT/daemon.log"
IDLE_CSV="$TMPDIR_ROOT/idle_samples.csv"
BURST_CSV="$TMPDIR_ROOT/burst_samples.csv"
SUMMARY_MD="$TMPDIR_ROOT/summary.md"

DAEMON_PID=""

cleanup() {
  local status=$?
  if [[ -n "${DAEMON_PID:-}" ]]; then
    kill "$DAEMON_PID" >/dev/null 2>&1 || true
  fi
  pkill -f "budi-daemon serve --host 127.0.0.1 --port $DAEMON_PORT" >/dev/null 2>&1 || true
  if [[ "${KEEP_TMP:-0}" == "1" ]]; then
    echo "[bench] leaving temp: $TMPDIR_ROOT" >&2
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
  echo "[bench] $*"
  echo "=================================================================="
}

# Portable FD count. macOS doesn't expose /proc; fall back to lsof (which
# is preinstalled everywhere Budi is supported).
fd_count() {
  local pid="$1"
  if [[ -d "/proc/$pid/fd" ]]; then
    # shellcheck disable=SC2012
    ls "/proc/$pid/fd" 2>/dev/null | wc -l | tr -d ' '
  else
    # `lsof -p $PID -Fn` emits one line per FD plus one `p$PID` header line;
    # counting lines starting with `f` gives the FD count directly.
    lsof -p "$pid" -Fn 2>/dev/null | grep -c '^f' | tr -d ' '
  fi
}

# RSS + CPU from `ps`. Both BSD (macOS) and procps (Linux) accept `-o`.
# rss is in kilobytes on both platforms.
ps_rss_cpu() {
  local pid="$1"
  # trim leading whitespace, collapse runs, replace space with comma
  ps -p "$pid" -o rss=,%cpu= 2>/dev/null | awk '{printf "%s,%s", $1, $2}'
}

sample_once() {
  # CSV row: elapsed_s,rss_kb,cpu_pct,fd_count
  local pid="$1"
  local elapsed="$2"
  local rss_cpu
  rss_cpu="$(ps_rss_cpu "$pid")"
  if [[ -z "$rss_cpu" ]]; then
    echo "$elapsed,,," # daemon gone; caller decides what to do
    return
  fi
  local fds
  fds="$(fd_count "$pid")"
  echo "$elapsed,$rss_cpu,$fds"
}

# Sleep in fractional seconds; GNU coreutils sleep supports float, and
# macOS has BSD sleep which also supports float on modern systems.
sleep_float() {
  sleep "$1"
}

# Millisecond-resolution epoch. BSD `date` silently produces a literal
# `3N` suffix for `%3N` rather than failing, so we cannot rely on the
# shell fallback pattern `date +%s%3N 2>/dev/null || python3 ...`. Always
# route through python, which is already a hard dep for the other e2e
# harnesses in this directory.
now_ms() {
  python3 -c 'import time;print(int(time.time()*1000))'
}

# ---------------------------------------------------------------------------
# bootstrap: init DB + seed empty provider watch roots
# ---------------------------------------------------------------------------

step "bootstrap: init DB + seed provider watch roots"

INIT_LOG="$TMPDIR_ROOT/init.log"
"$BUDI" init --no-daemon >"$INIT_LOG" 2>&1 || {
  cat "$INIT_LOG" >&2
  echo "[bench] FAIL: init crashed" >&2
  exit 1
}

# Seed every provider's root so the tailer attaches a watcher per provider.
# Directories stay empty; the burst phase below writes only into
# ~/.claude/projects/repo.
CLAUDE_DIR="$HOME/.claude/projects/repo"
CODEX_DIR="$CODEX_HOME/sessions"
CURSOR_DIR="$HOME/.cursor/projects"
COPILOT_DIR="$COPILOT_HOME/session-state"
mkdir -p "$CLAUDE_DIR" "$CODEX_DIR" "$CURSOR_DIR" "$COPILOT_DIR"

# ---------------------------------------------------------------------------
# boot daemon
# ---------------------------------------------------------------------------

step "boot daemon on :$DAEMON_PORT"

RUST_LOG=info "$BUDI_DAEMON" serve \
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
  echo "[bench] FAIL: daemon did not start" >&2
  tail -n 80 "$DAEMON_LOG" >&2 || true
  exit 1
fi

# Give the tailer one backstop tick (5 s) + slack to attach every provider's
# watcher before we take the first idle sample. Otherwise the first few
# samples would be taken while routes are still being reconciled.
sleep 6

# ---------------------------------------------------------------------------
# idle soak
# ---------------------------------------------------------------------------

step "idle soak: ${SOAK_SECS}s, sample every ${SAMPLE_EVERY}s"

: > "$IDLE_CSV"
echo "elapsed_s,rss_kb,cpu_pct,fd_count" >> "$IDLE_CSV"

SOAK_START="$(date +%s)"
NEXT_SAMPLE=0
while :; do
  NOW="$(date +%s)"
  ELAPSED=$(( NOW - SOAK_START ))
  if (( ELAPSED >= SOAK_SECS )); then
    break
  fi
  if (( ELAPSED >= NEXT_SAMPLE )); then
    row="$(sample_once "$DAEMON_PID" "$ELAPSED")"
    echo "$row" >> "$IDLE_CSV"
    NEXT_SAMPLE=$(( ELAPSED + SAMPLE_EVERY ))
    printf '[bench] idle %4ds  %s\n' "$ELAPSED" "$row"
  fi
  sleep 1
done

# ---------------------------------------------------------------------------
# synthetic burst
# ---------------------------------------------------------------------------

step "synthetic burst: $BURST_EVENTS events @ ${BURST_GAP_MS}ms, observe ${BURST_WINDOW_SECS}s"

BURST_FILE="$CLAUDE_DIR/session-bench-410.jsonl"
BURST_SESSION="bench-410-$(date +%s)"

# Pre-render all $BURST_EVENTS JSONL lines into a scratch file; the burst
# phase just appends them with per-event sleeps. Keeping the JSON shape
# aligned with test_328_release_smoke.sh's Claude Code fixture so the
# pipeline actually parses the events (not just dumps raw bytes on disk).
BURST_LINES="$TMPDIR_ROOT/burst_lines.jsonl"
python3 - "$BURST_LINES" "$BURST_SESSION" "$BURST_EVENTS" <<'PY'
import datetime
import json
import sys

path, session, count_s = sys.argv[1:]
count = int(count_s)

now = datetime.datetime.now(datetime.timezone.utc)
with open(path, "w", encoding="utf-8") as f:
    for i in range(count):
        ts = (now + datetime.timedelta(milliseconds=i)).isoformat(
            timespec="milliseconds"
        ).replace("+00:00", "Z")
        user = {
            "type": "user",
            "uuid": f"{session}-u-{i}",
            "parentUuid": None,
            "isSidechain": False,
            "sessionId": session,
            "timestamp": ts,
            "cwd": "/tmp/bench-410",
            "gitBranch": "v8/410-tailer-perf-baseline",
            "message": {"role": "user", "content": f"bench event {i}"},
        }
        f.write(json.dumps(user, separators=(",", ":")) + "\n")
PY

: > "$BURST_CSV"
echo "phase,elapsed_ms,rss_kb,cpu_pct,fd_count" >> "$BURST_CSV"

# Sampler runs in the background for the full observation window so it
# doesn't miss samples while the append loop is in flight.
BURST_OBS_SECS=$(( BURST_WINDOW_SECS + (BURST_EVENTS * BURST_GAP_MS + 999) / 1000 + 2 ))
BURST_SAMPLE_SECS="$(awk -v ms="$BURST_SAMPLE_MS" 'BEGIN{printf "%.3f", ms/1000.0}')"

(
  BURST_T0="$(now_ms)"
  END_MS=$(( BURST_T0 + BURST_OBS_SECS * 1000 ))
  while :; do
    NOW_MS="$(now_ms)"
    if (( NOW_MS >= END_MS )); then
      break
    fi
    ELAPSED_MS=$(( NOW_MS - BURST_T0 ))
    row="$(sample_once "$DAEMON_PID" "$ELAPSED_MS")"
    echo "burst,$row" >> "$BURST_CSV"
    sleep_float "$BURST_SAMPLE_SECS"
  done
) &
SAMPLER_PID=$!

# Give the sampler a beat to take at least one pre-burst row.
sleep_float "$BURST_SAMPLE_SECS"

# Append events one at a time with BURST_GAP_MS between them. We append
# line-by-line instead of a single `cat` so the tailer sees real growth
# events that the notify-debouncer-mini can collapse, mirroring how a
# real agent streams turns to disk.
APPEND_DELAY="$(awk -v ms="$BURST_GAP_MS" 'BEGIN{printf "%.3f", ms/1000.0}')"
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$BURST_FILE"
  sleep_float "$APPEND_DELAY"
done < "$BURST_LINES"

wait "$SAMPLER_PID" || true

# Confirm the burst actually round-tripped into the DB. Not a hard
# assertion — if ingest silently dropped the fixture, the baseline would
# still measure the tailer's CPU cost of reading and parsing, which is
# what we care about — but worth logging.
DB="$BUDI_HOME/analytics.db"
INGESTED="$(sqlite3 -cmd ".timeout 5000" "$DB" \
  "SELECT COUNT(*) FROM messages WHERE session_id='$BURST_SESSION';" 2>/dev/null || echo 0)"
echo "[bench] ingested_rows_after_burst=$INGESTED (out of $BURST_EVENTS user events)"

# ---------------------------------------------------------------------------
# notify watcher count
# ---------------------------------------------------------------------------

step "notify watcher inventory"

# Each `attach_new_watchers` Ok branch logs an `INFO watching provider=X
# root=Y` line (target filter on `budi_daemon::tailer`, but `RUST_LOG=info`
# doesn't render target prefixes). Count distinct roots from that span.
WATCHING_LINES="$(grep -E '\bwatching provider=' "$DAEMON_LOG" || true)"
WATCH_ROOTS=0
if [[ -n "$WATCHING_LINES" ]]; then
  WATCH_ROOTS="$(printf '%s\n' "$WATCHING_LINES" \
    | sed -n 's/.*root=\([^ ]*\).*/\1/p' \
    | sort -u | wc -l | tr -d ' ')"
fi
# Also surface which providers we saw.
WATCH_PROVIDERS="$(printf '%s\n' "$WATCHING_LINES" \
  | sed -n 's/.*provider=\([^ ]*\).*/\1/p' \
  | sort -u | paste -sd, - || true)"

echo "[bench] watch_roots=$WATCH_ROOTS providers=${WATCH_PROVIDERS:-(none)}"

# ---------------------------------------------------------------------------
# summary
# ---------------------------------------------------------------------------

step "summary"

python3 - "$IDLE_CSV" "$BURST_CSV" "$WATCH_ROOTS" "$WATCH_PROVIDERS" "$INGESTED" \
         "$SOAK_SECS" "$SAMPLE_EVERY" "$BURST_EVENTS" "$BURST_GAP_MS" \
         "$BURST_WINDOW_SECS" "$BURST_SAMPLE_MS" > "$SUMMARY_MD" <<'PY'
import csv
import os
import platform
import statistics
import sys

(idle_csv, burst_csv, watch_roots, providers, ingested,
 soak_secs, sample_every, burst_events, burst_gap_ms,
 burst_window_secs, burst_sample_ms) = sys.argv[1:]


def load(path):
    with open(path, newline="", encoding="utf-8") as f:
        return list(csv.DictReader(f))


def col(rows, key):
    out = []
    for r in rows:
        v = r.get(key, "")
        if v == "" or v is None:
            continue
        try:
            out.append(float(v))
        except ValueError:
            continue
    return out


def summ(vals):
    if not vals:
        return "n/a", "n/a", "n/a"
    return (
        f"{min(vals):.1f}",
        f"{statistics.mean(vals):.1f}",
        f"{max(vals):.1f}",
    )


idle = load(idle_csv)
burst = load(burst_csv)

idle_rss = col(idle, "rss_kb")
idle_cpu = col(idle, "cpu_pct")
idle_fd = col(idle, "fd_count")

burst_rss = col(burst, "rss_kb")
burst_cpu = col(burst, "cpu_pct")
burst_fd = col(burst, "fd_count")

rss_min, rss_mean, rss_max = summ(idle_rss)
cpu_min, cpu_mean, cpu_max = summ(idle_cpu)
fd_min, fd_mean, fd_max = summ(idle_fd)

brss_min, brss_mean, brss_max = summ(burst_rss)
bcpu_min, bcpu_mean, bcpu_max = summ(burst_cpu)
bfd_min, bfd_mean, bfd_max = summ(burst_fd)


def fmt_int(s):
    try:
        return f"{int(float(s))}"
    except Exception:
        return s


print(f"## Tailer perf baseline — `{platform.system()}` `{platform.machine()}`")
print()
print(f"- Host: `{platform.system()} {platform.release()}` ({platform.machine()})")
print(f"- Binary: `budi-daemon` release build from `target/release/`")
print(f"- Soak: `{soak_secs}s` @ `{sample_every}s` interval → {len(idle)} idle samples")
print(f"- Burst: `{burst_events}` Claude Code user events @ `{burst_gap_ms}ms` "
      f"gap, observed for `{burst_window_secs}s` post-start @ `{burst_sample_ms}ms` "
      f"sample interval → {len(burst)} burst samples")
print(f"- notify watchers attached: `{watch_roots}` root(s)")
print(f"- Providers observed: `{providers or '(none)'}`")
print(f"- Ingested rows from burst: `{ingested}` / `{burst_events}`")
print()
print("| Phase | Metric | min | mean | max |")
print("|---|---|---|---|---|")
print(f"| idle | RSS (KB) | {fmt_int(rss_min)} | {fmt_int(rss_mean)} | {fmt_int(rss_max)} |")
print(f"| idle | CPU (%) | {cpu_min} | {cpu_mean} | {cpu_max} |")
print(f"| idle | FD count | {fmt_int(fd_min)} | {fmt_int(fd_mean)} | {fmt_int(fd_max)} |")
print(f"| burst | RSS (KB) | {fmt_int(brss_min)} | {fmt_int(brss_mean)} | {fmt_int(brss_max)} |")
print(f"| burst | CPU (%) | {bcpu_min} | {bcpu_mean} | {bcpu_max} |")
print(f"| burst | FD count | {fmt_int(bfd_min)} | {fmt_int(bfd_mean)} | {fmt_int(bfd_max)} |")
PY

cat "$SUMMARY_MD"

# Persist the raw CSVs next to the summary if the operator wants to keep
# them for a wiki page.
if [[ "${KEEP_TMP:-0}" == "1" ]]; then
  echo
  echo "[bench] raw samples:"
  echo "  idle:  $IDLE_CSV"
  echo "  burst: $BURST_CSV"
  echo "  md:    $SUMMARY_MD"
  echo "  log:   $DAEMON_LOG"
fi

echo
echo "[bench] DONE"
