#!/usr/bin/env bash
# Cursor Usage API lag measurement — instrument for [8.2][R1.5] (#321).
#
# Purpose
#   ADR-0089 §7 commits Budi 8.2 to using Cursor's Usage API as a *pull*
#   (scheduled by `budi sync`), not as a live ingestion path. The pivot is
#   only honest if the Usage API surfaces per-request usage events fast
#   enough that "same-day cost accuracy" is preserved for Cursor users.
#   This script measures that lag empirically against a live Cursor
#   session on the operator's own machine.
#
#   See: ADR-0089 (`docs/adr/0089-reverse-proxy-first-jsonl-tailing-as-sole-live-path.md`)
#        and #316 / #321 for the gating context.
#
# Methodology
#   1. Resolve Cursor's auth (JWT + user_id) from `state.vscdb` exactly the
#      way `crates/budi-core/src/providers/cursor.rs::extract_cursor_auth`
#      does. The script reads the DB read-only and does not modify any
#      Cursor state.
#   2. Poll `https://cursor.com/api/dashboard/get-filtered-usage-events`
#      every POLL_INTERVAL seconds (default: 5). The API returns the most
#      recent ~100 events newest-first; we only ever look at the first
#      page so each poll costs one HTTP request.
#   3. For every event we have not seen before (keyed on
#      `(timestamp, model, inputTokens, outputTokens, cacheReadTokens, totalCents)`
#      — the API does not expose a stable request_id), record:
#        - `event_timestamp_ms`    — the `timestamp` the API attaches
#                                    (the moment Cursor's backend says
#                                    the request happened)
#        - `first_seen_at_ms`      — wall clock when our poller first
#                                    observed the event in the API
#                                    response
#        - `lag_ms`                — `first_seen_at_ms - event_timestamp_ms`
#                                    (clamped at zero; clock skew between
#                                    the operator's machine and Cursor's
#                                    servers can in principle make this
#                                    slightly negative — we treat that as
#                                    "lag below measurement floor" and
#                                    record it as 0)
#   4. Stream rows to a CSV as they happen so a SIGINT mid-run still
#      preserves data. On exit (Ctrl-C, --duration expiry, or the
#      operator running --analyze on an existing CSV), compute p50 / p90
#      / p99 of `lag_ms` plus the min / max / count, and write a JSON
#      summary file alongside the CSV.
#   5. The operator drives a real Cursor session in parallel — typical
#      coding interactions in Cursor's chat / composer / inline edit
#      flows. To get a meaningful sample the script keeps polling until
#      it has logged at least --min-events events (default: 100) or the
#      --duration limit expires, whichever comes first.
#
# Output verdict (operator action)
#   Once the run completes, the operator publishes:
#     - a wiki page under `Research/` on
#       https://github.com/siropkin/budi/wiki summarising methodology,
#       p50 / p90 / p99 numbers, classification of failure modes
#       (bounded vs unbounded vs spiky), and a recommendation chosen
#       from §C.{a,b,c} of #321,
#     - a comment on #321 linking the wiki page,
#     - an updated ADR-0089 §7 with the chosen recommendation and a
#       link to the wiki page (the ADR cannot promote from `Proposed`
#       to `Accepted` until this section is filled in — see
#       ADR-0089 Promotion Criteria).
#
#   Per #316 rule 12 the verdict memo lives in the wiki, not in
#   `docs/research/`. This script is the durable in-tree artifact; the
#   numeric memo is the perishable run output.
#
# Privacy
#   The script reads only:
#     - Cursor's `state.vscdb` (read-only) for the auth JWT
#     - Cursor's Usage API (the same endpoint `budi sync` already calls)
#   No prompts, code, transcripts, or response bodies are read or
#   recorded. The CSV contains only event timestamps, model names, and
#   token / cost columns the Usage API already exposes — the same data
#   surface ADR-0083 already permits.
#
# Operator-only
#   This is real-machine, real-API-key work in the same way the smoke
#   PASS records are. Per #316 Lessons §5, agents do not run this
#   script — they ship and document it; the maintainer runs it and
#   posts the verdict.
#
# Usage
#   scripts/research/cursor_usage_api_lag.sh [options]
#
#   Options:
#     --output PATH           CSV output path (default: ./cursor-usage-lag-<utc>.csv)
#     --interval SECONDS      Poll interval (default: 5)
#     --duration SECONDS      Max wall-clock run length (default: 14400 = 4h)
#     --min-events N          Stop early once N distinct events logged (default: 100)
#     --analyze CSV           Skip polling; recompute summary from an existing CSV
#     -h, --help              Show this help
#
# Requirements
#   - bash 4+ (macOS users: `brew install bash` or rely on /bin/bash 3.x;
#     the script avoids bash-4-only syntax on purpose)
#   - sqlite3 (macOS / Linux: pre-installed; Windows: use WSL)
#   - curl
#   - jq
#   - awk (POSIX awk is fine)

set -uo pipefail

if (( ${BASH_VERSINFO[0]:-0} < 4 )); then
  echo "error: bash 4+ required (this script uses associative arrays)." >&2
  echo "       macOS ships bash 3.2 by default — install via Homebrew:" >&2
  echo "         brew install bash" >&2
  echo "       then re-run with /usr/local/bin/bash or /opt/homebrew/bin/bash." >&2
  exit 2
fi

POLL_INTERVAL=5
MAX_DURATION=14400
MIN_EVENTS=100
OUTPUT=""
ANALYZE_ONLY=""

usage() {
  sed -n '2,100p' "$0" | sed 's/^# \{0,1\}//' >&2
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --output) OUTPUT="$2"; shift 2 ;;
    --interval) POLL_INTERVAL="$2"; shift 2 ;;
    --duration) MAX_DURATION="$2"; shift 2 ;;
    --min-events) MIN_EVENTS="$2"; shift 2 ;;
    --analyze) ANALYZE_ONLY="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "error: unknown argument: $1" >&2; usage; exit 2 ;;
  esac
done

require() {
  local cmd="$1"
  if ! command -v "$cmd" >/dev/null 2>&1; then
    echo "error: required dependency not found: $cmd" >&2
    exit 2
  fi
}

require sqlite3
require curl
require jq
require awk

now_ms() {
  # Portable millisecond clock. macOS `date` lacks `%N`, so we fall back
  # to `python3` when GNU date isn't available, then to second precision.
  if date +%s%3N 2>/dev/null | grep -qE '^[0-9]+$'; then
    date +%s%3N
  elif command -v python3 >/dev/null 2>&1; then
    python3 -c 'import time; print(int(time.time() * 1000))'
  else
    echo "$(($(date +%s) * 1000))"
  fi
}

# ---- Percentile / summary helpers ------------------------------------------

summarize() {
  # Reads `lag_ms` (one integer per line) on stdin and prints the
  # min / p50 / p90 / p99 / max / count summary as a single tab-separated
  # row: count\tmin\tp50\tp90\tp99\tmax. An empty stream prints all
  # zeroes with count=0 so callers don't have to special-case it.
  awk '
    BEGIN { n = 0 }
    /^[0-9]+$/ { lags[n++] = $1 + 0 }
    END {
      if (n == 0) {
        print "0\t0\t0\t0\t0\t0"
        exit
      }
      # Sort ascending.
      for (i = 1; i < n; i++) {
        for (j = i; j > 0 && lags[j-1] > lags[j]; j--) {
          tmp = lags[j]; lags[j] = lags[j-1]; lags[j-1] = tmp
        }
      }
      p50 = lags[int(0.50 * (n - 1) + 0.5)]
      p90 = lags[int(0.90 * (n - 1) + 0.5)]
      p99 = lags[int(0.99 * (n - 1) + 0.5)]
      printf "%d\t%d\t%d\t%d\t%d\t%d\n", n, lags[0], p50, p90, p99, lags[n-1]
    }
  '
}

write_summary_json() {
  local csv="$1"
  local summary
  summary="$(awk -F, 'NR > 1 { print $4 }' "$csv" | summarize)"
  local count min p50 p90 p99 max
  IFS=$'\t' read -r count min p50 p90 p99 max <<<"$summary"

  local json="${csv%.csv}.summary.json"
  cat >"$json" <<EOF
{
  "csv": "$(basename "$csv")",
  "generated_at_utc": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "event_count": $count,
  "lag_ms": {
    "min": $min,
    "p50": $p50,
    "p90": $p90,
    "p99": $p99,
    "max": $max
  },
  "notes": [
    "lag_ms = first_seen_at_ms - event_timestamp_ms (clamped at 0).",
    "Sample is the operator's live Cursor session over the run window.",
    "See scripts/research/cursor_usage_api_lag.sh for methodology."
  ]
}
EOF

  printf "\n----- summary -----\n"
  printf "events:  %s\n" "$count"
  printf "min:     %s ms\n" "$min"
  printf "p50:     %s ms\n" "$p50"
  printf "p90:     %s ms\n" "$p90"
  printf "p99:     %s ms\n" "$p99"
  printf "max:     %s ms\n" "$max"
  printf "csv:     %s\n" "$csv"
  printf "summary: %s\n" "$json"
  printf "\n"
  printf "Recommendation hint:\n"
  printf "  - p99 < 60_000 (1 min)   --> §C.a accept the lag\n"
  printf "  - 60_000 <= p99 < 600_000 (10 min) --> consider §C.c (UX warning)\n"
  printf "  - p99 >= 600_000 or unbounded     --> reconsider §C.b (Cursor-only proxy passthrough)\n"
}

# ---- --analyze path --------------------------------------------------------

if [[ -n "$ANALYZE_ONLY" ]]; then
  if [[ ! -f "$ANALYZE_ONLY" ]]; then
    echo "error: --analyze CSV not found: $ANALYZE_ONLY" >&2
    exit 2
  fi
  write_summary_json "$ANALYZE_ONLY"
  exit 0
fi

# ---- Auth resolution -------------------------------------------------------

resolve_state_vscdb() {
  local candidates=(
    "$HOME/Library/Application Support/Cursor/User/globalStorage/state.vscdb"
    "$HOME/.config/Cursor/User/globalStorage/state.vscdb"
  )
  for path in "${candidates[@]}"; do
    if [[ -f "$path" ]]; then
      printf "%s\n" "$path"
      return 0
    fi
  done
  echo "error: could not find Cursor state.vscdb in known locations" >&2
  echo "       (tried: ${candidates[*]})" >&2
  return 1
}

base64url_decode() {
  # Pad to multiple of 4 then translate URL-safe alphabet back to standard.
  local s="$1"
  local pad=$(( (4 - ${#s} % 4) % 4 ))
  while (( pad-- > 0 )); do s="${s}="; done
  printf "%s" "$s" | tr '_-' '/+' | base64 -d 2>/dev/null
}

extract_user_id_from_jwt() {
  local jwt="$1"
  local payload="${jwt#*.}"
  payload="${payload%%.*}"
  local decoded
  decoded="$(base64url_decode "$payload")" || return 1
  local sub
  sub="$(printf "%s" "$decoded" | jq -r '.sub // empty')" || return 1
  if [[ -z "$sub" ]]; then
    echo "error: JWT payload missing 'sub' field" >&2
    return 1
  fi
  # `sub` is "auth0|<userId>" — Cursor's session cookie wants the trailing part.
  printf "%s\n" "${sub##*|}"
}

VSCDB="$(resolve_state_vscdb)" || exit 2
JWT="$(sqlite3 -readonly "$VSCDB" \
  "SELECT value FROM ItemTable WHERE key = 'cursorAuth/accessToken'" 2>/dev/null)"

if [[ -z "$JWT" ]]; then
  echo "error: no Cursor JWT found in $VSCDB (is Cursor signed in?)" >&2
  exit 2
fi

USER_ID="$(extract_user_id_from_jwt "$JWT")" || exit 2
COOKIE="WorkosCursorSessionToken=${USER_ID}%3A%3A${JWT}"

# ---- CSV bootstrap ---------------------------------------------------------

if [[ -z "$OUTPUT" ]]; then
  OUTPUT="cursor-usage-lag-$(date -u +%Y%m%dT%H%M%SZ).csv"
fi

if [[ ! -f "$OUTPUT" ]]; then
  echo "event_timestamp_ms,first_seen_at_ms,model,lag_ms,input_tokens,output_tokens,cache_read_tokens,total_cents,kind" >"$OUTPUT"
fi

# Replay any existing rows so a resumed run does not double-log events
# from the previous session if the operator passes --output pointing at
# a partial file.
declare -A SEEN
if [[ -f "$OUTPUT" ]]; then
  while IFS=, read -r ts _ model _ in_t out_t cache_t cents kind; do
    if [[ "$ts" == "event_timestamp_ms" ]]; then continue; fi
    SEEN["${ts}|${model}|${in_t}|${out_t}|${cache_t}|${cents}|${kind}"]=1
  done <"$OUTPUT"
fi

# ---- Polling loop ----------------------------------------------------------

START_MS="$(now_ms)"
END_MS=$(( START_MS + MAX_DURATION * 1000 ))
NEW_EVENTS=0

cleanup() {
  printf "\n[stopping] writing summary...\n" >&2
  write_summary_json "$OUTPUT" || true
}
trap cleanup INT TERM EXIT

printf "[cursor-usage-lag] polling every %ss, max duration %ss, target %s events\n" \
  "$POLL_INTERVAL" "$MAX_DURATION" "$MIN_EVENTS" >&2
printf "[cursor-usage-lag] writing to %s\n" "$OUTPUT" >&2

while :; do
  current_ms="$(now_ms)"
  if (( current_ms > END_MS )); then
    printf "[cursor-usage-lag] duration limit reached\n" >&2
    break
  fi

  body_file="$(mktemp -t cursor-usage-lag.XXXXXX)"
  err_file="$(mktemp -t cursor-usage-lag-err.XXXXXX)"
  http_code="$(curl --silent --show-error \
    --max-time 15 \
    --output "$body_file" \
    --write-out '%{http_code}' \
    --header "Cookie: ${COOKIE}" \
    --header "Origin: https://cursor.com" \
    --header "Referer: https://cursor.com/dashboard" \
    --header "Content-Type: application/json" \
    --header "User-Agent: budi-cursor-usage-lag/1.0 (+https://github.com/siropkin/budi/issues/321)" \
    --data '{}' \
    https://cursor.com/api/dashboard/get-filtered-usage-events 2>"$err_file")"
  curl_exit=$?
  observed_at="$(now_ms)"
  body_size="$(wc -c <"$body_file" 2>/dev/null | tr -d ' ')"
  body_snippet="$(head -c 200 "$body_file" 2>/dev/null | tr '\n\r\t' '   ')"
  curl_err="$(tr '\n' ' ' <"$err_file" 2>/dev/null)"

  if [[ "$curl_exit" -ne 0 ]]; then
    printf "[cursor-usage-lag] curl failed (exit=%d, http=%s): %s — retrying in %ss\n" \
      "$curl_exit" "${http_code:-000}" "${curl_err:-no stderr}" "$POLL_INTERVAL" >&2
    rm -f "$body_file" "$err_file"
    sleep "$POLL_INTERVAL"
    continue
  fi

  if [[ "$http_code" != "200" ]]; then
    printf "[cursor-usage-lag] HTTP %s (body %s bytes): %s — retrying in %ss\n" \
      "$http_code" "${body_size:-0}" "${body_snippet:-<empty>}" "$POLL_INTERVAL" >&2
    case "$http_code" in
      401|403)
        printf "[cursor-usage-lag] hint: HTTP %s usually means the JWT expired or the session was invalidated.\n" "$http_code" >&2
        printf "[cursor-usage-lag] hint: restart Cursor (it auto-refreshes the token in state.vscdb), then re-run.\n" >&2
        ;;
      429)
        printf "[cursor-usage-lag] hint: HTTP 429 = rate-limited; raise --interval.\n" >&2
        ;;
      000)
        printf "[cursor-usage-lag] hint: HTTP 000 = no HTTP response (network unreachable, DNS, or TLS handshake failure).\n" >&2
        ;;
    esac
    rm -f "$body_file" "$err_file"
    sleep "$POLL_INTERVAL"
    continue
  fi

  # 200 OK but body isn't JSON — surface the snippet so we can tell whether
  # it's an HTML Cloudflare challenge page or some other content.
  if ! jq -e . <"$body_file" >/dev/null 2>&1; then
    printf "[cursor-usage-lag] HTTP 200 but body is not JSON (%s bytes): %s — retrying in %ss\n" \
      "${body_size:-0}" "${body_snippet:-<empty>}" "$POLL_INTERVAL" >&2
    rm -f "$body_file" "$err_file"
    sleep "$POLL_INTERVAL"
    continue
  fi

  rows="$(jq -r '
    .usageEventsDisplay // []
    | .[]
    | [
        (.timestamp // "0"),
        (.model // "unknown"),
        ((.tokenUsage.inputTokens // 0)),
        ((.tokenUsage.outputTokens // 0)),
        ((.tokenUsage.cacheReadTokens // 0)),
        ((.tokenUsage.totalCents // 0)),
        (.kind // "")
      ]
    | @tsv
  ' <"$body_file" 2>/dev/null || true)"
  rm -f "$body_file" "$err_file"

  if [[ -z "$rows" ]]; then
    printf "[cursor-usage-lag] HTTP 200, valid JSON, no usage events on this page (waiting for new activity)\n" >&2
    sleep "$POLL_INTERVAL"
    continue
  fi

  while IFS=$'\t' read -r ts model in_t out_t cache_t cents kind; do
    [[ -z "$ts" || "$ts" == "0" ]] && continue
    key="${ts}|${model}|${in_t}|${out_t}|${cache_t}|${cents}|${kind}"
    if [[ -n "${SEEN[$key]:-}" ]]; then
      continue
    fi
    SEEN["$key"]=1

    lag=$(( observed_at - ts ))
    if (( lag < 0 )); then
      lag=0
    fi

    printf "%s,%s,%s,%s,%s,%s,%s,%s,%s\n" \
      "$ts" "$observed_at" "$model" "$lag" "$in_t" "$out_t" "$cache_t" "$cents" "$kind" \
      >>"$OUTPUT"

    NEW_EVENTS=$(( NEW_EVENTS + 1 ))
    printf "[cursor-usage-lag] +event lag=%sms model=%s (total new: %d)\n" \
      "$lag" "$model" "$NEW_EVENTS" >&2
  done <<<"$rows"

  if (( NEW_EVENTS >= MIN_EVENTS )); then
    printf "[cursor-usage-lag] reached --min-events=%d\n" "$MIN_EVENTS" >&2
    break
  fi

  sleep "$POLL_INTERVAL"
done
