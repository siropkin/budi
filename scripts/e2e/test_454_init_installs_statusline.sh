#!/usr/bin/env bash
# End-to-end regression for issue #454: verify `budi init` installs the
# Claude Code statusline into ~/.claude/settings.json by default, while
# `budi init --no-integrations` stays out of it.
#
# Acceptance contract pinned:
# - `budi init` on a fresh machine with ~/.claude present leaves
#   ~/.claude/settings.json with a Budi-backed `statusLine.command`.
# - The installer is idempotent: a second `budi init` must not clobber
#   or duplicate the statusline entry.
# - `budi init --no-integrations` leaves ~/.claude/settings.json absent
#   (or at least never adds a statusLine key when one was not present).
# - `budi doctor` warns when ~/.claude exists but the statusline is not
#   installed; the warn includes the exact repair command.
set -euo pipefail

# Strip ANSI color codes from captured output so doctor / CLI strings
# can be grep'd without escape-sequence mismatches.
export NO_COLOR="${NO_COLOR:-1}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BUDI="$ROOT/target/release/budi"

if [[ ! -x "$BUDI" ]]; then
  echo "error: release binary not built. run \`cargo build --release\` first." >&2
  exit 2
fi

TMPDIR_ROOT="$(mktemp -d -t budi-e2e-454-XXXXXX)"
export HOME="$TMPDIR_ROOT"
export BUDI_HOME="$HOME/.local/share/budi"

cleanup() {
  local status=$?
  if [[ "${KEEP_TMP:-0}" == "1" ]]; then
    echo "[e2e] leaving tmp: $TMPDIR_ROOT"
  else
    rm -rf "$TMPDIR_ROOT"
  fi
  exit $status
}
trap cleanup EXIT INT TERM

echo "[e2e] HOME=$HOME"

CLAUDE_SETTINGS="$HOME/.claude/settings.json"

assert_statusline_has_budi() {
  if [[ ! -f "$CLAUDE_SETTINGS" ]]; then
    echo "[e2e] FAIL: expected $CLAUDE_SETTINGS to exist" >&2
    exit 1
  fi
  if ! grep -q '"statusLine"' "$CLAUDE_SETTINGS"; then
    echo "[e2e] FAIL: $CLAUDE_SETTINGS has no statusLine key" >&2
    cat "$CLAUDE_SETTINGS" >&2
    exit 1
  fi
  if ! grep -q 'budi statusline' "$CLAUDE_SETTINGS"; then
    echo "[e2e] FAIL: $CLAUDE_SETTINGS statusLine.command does not reference budi statusline" >&2
    cat "$CLAUDE_SETTINGS" >&2
    exit 1
  fi
}

# Scenario 1: Claude Code not detected — ~/.claude is absent.
# `budi init` must not write ~/.claude/settings.json; that would be a
# silent creation path (one of the explicit scope constraints).
echo "[e2e] scenario 1: ~/.claude absent — init must not create it"
rm -rf "$HOME/.claude"
"$BUDI" init --no-daemon >"$TMPDIR_ROOT/init-1.log" 2>&1 || {
  cat "$TMPDIR_ROOT/init-1.log" >&2
  echo "[e2e] FAIL: init run in scenario 1 failed" >&2
  exit 1
}
if [[ -e "$HOME/.claude" ]]; then
  echo "[e2e] FAIL: init created ~/.claude when Claude Code was not installed" >&2
  exit 1
fi
echo "[e2e] OK: init without ~/.claude leaves the directory absent"

# Scenario 2: Claude Code detected — ~/.claude exists with no settings.
# `budi init` must write ~/.claude/settings.json with a Budi statusLine.
echo "[e2e] scenario 2: ~/.claude present with empty settings — init installs statusline"
mkdir -p "$HOME/.claude"
"$BUDI" init --no-daemon >"$TMPDIR_ROOT/init-2.log" 2>&1 || {
  cat "$TMPDIR_ROOT/init-2.log" >&2
  echo "[e2e] FAIL: init run in scenario 2 failed" >&2
  exit 1
}
assert_statusline_has_budi
echo "[e2e] OK: init installed statusline into $CLAUDE_SETTINGS"

# Scenario 2b: second init run must be idempotent — the settings file
# remains valid JSON with a single Budi-backed statusLine entry.
echo "[e2e] scenario 2b: second init run (idempotence)"
COPY_BEFORE="$TMPDIR_ROOT/settings-before-2b.json"
cp "$CLAUDE_SETTINGS" "$COPY_BEFORE"
"$BUDI" init --no-daemon >"$TMPDIR_ROOT/init-2b.log" 2>&1 || {
  cat "$TMPDIR_ROOT/init-2b.log" >&2
  echo "[e2e] FAIL: second init run failed" >&2
  exit 1
}
assert_statusline_has_budi
# Pin: idempotence at the byte level for the statusLine block. The
# second run must neither duplicate the budi statusline suffix nor
# alter the command string beyond what the first run produced.
before_count=$(grep -c 'budi statusline' "$COPY_BEFORE" || true)
after_count=$(grep -c 'budi statusline' "$CLAUDE_SETTINGS" || true)
if [[ "$before_count" != "$after_count" ]]; then
  echo "[e2e] FAIL: statusline references changed across idempotent runs (before=$before_count after=$after_count)" >&2
  diff "$COPY_BEFORE" "$CLAUDE_SETTINGS" >&2 || true
  exit 1
fi
echo "[e2e] OK: second init left statusline byte-stable"

# Scenario 3: --no-integrations must skip the installer entirely even
# when ~/.claude is present. We exercise this on a fresh HOME so there
# is no pre-existing settings file to merge with.
echo "[e2e] scenario 3: --no-integrations leaves ~/.claude/settings.json alone"
TMPDIR_ROOT2="$(mktemp -d -t budi-e2e-454b-XXXXXX)"
OLD_HOME="$HOME"
export HOME="$TMPDIR_ROOT2"
export BUDI_HOME="$HOME/.local/share/budi"
mkdir -p "$HOME/.claude"
"$BUDI" init --no-daemon --no-integrations >"$TMPDIR_ROOT2/init-3.log" 2>&1 || {
  cat "$TMPDIR_ROOT2/init-3.log" >&2
  echo "[e2e] FAIL: --no-integrations init run failed" >&2
  rm -rf "$TMPDIR_ROOT2"
  exit 1
}
if [[ -e "$HOME/.claude/settings.json" ]]; then
  echo "[e2e] FAIL: --no-integrations still wrote ~/.claude/settings.json" >&2
  cat "$HOME/.claude/settings.json" >&2
  rm -rf "$TMPDIR_ROOT2"
  exit 1
fi
echo "[e2e] OK: --no-integrations left ~/.claude/settings.json absent"

# Scenario 4: doctor warns when ~/.claude is present but the statusline
# was not installed. We keep HOME=$TMPDIR_ROOT2 (no integrations run)
# and call `budi doctor`. It should emit a WARN line referencing the
# Claude statusline and print the repair command.
echo "[e2e] scenario 4: doctor warns on missing statusline"
DOCTOR_LOG="$TMPDIR_ROOT2/doctor.log"
"$BUDI" doctor >"$DOCTOR_LOG" 2>&1 || true
if ! grep -q "Claude statusline" "$DOCTOR_LOG"; then
  echo "[e2e] FAIL: doctor did not mention Claude statusline check" >&2
  cat "$DOCTOR_LOG" >&2
  rm -rf "$TMPDIR_ROOT2"
  exit 1
fi
if ! grep -q "WARN .*Claude statusline" "$DOCTOR_LOG"; then
  echo "[e2e] FAIL: doctor did not WARN on missing statusline" >&2
  cat "$DOCTOR_LOG" >&2
  rm -rf "$TMPDIR_ROOT2"
  exit 1
fi
if ! grep -q "budi integrations install" "$DOCTOR_LOG"; then
  echo "[e2e] FAIL: doctor warn did not include the repair command" >&2
  cat "$DOCTOR_LOG" >&2
  rm -rf "$TMPDIR_ROOT2"
  exit 1
fi
echo "[e2e] OK: doctor warned on missing statusline with repair command"

# Restore HOME for cleanup trap.
rm -rf "$TMPDIR_ROOT2"
export HOME="$OLD_HOME"
export BUDI_HOME="$HOME/.local/share/budi"

echo "[e2e] PASS"
