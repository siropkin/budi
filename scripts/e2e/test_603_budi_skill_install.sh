#!/usr/bin/env bash
# End-to-end regression for issue #603: verify `budi init` auto-installs
# the `/budi` Claude Code skill at ~/.claude/skills/budi/SKILL.md, repeat
# installs are byte-stable, `budi uninstall` removes the file, and
# `budi init --no-integrations` never writes it.
#
# Acceptance contract pinned (see #603):
# - Fresh `budi init` on a system with ~/.claude produces
#   ~/.claude/skills/budi/SKILL.md.
# - `budi init --no-integrations` does NOT produce the skill file.
# - Repeat `budi init` is byte-stable — the second install must not
#   rewrite the file (otherwise user edits would be silently clobbered).
# - `budi uninstall` removes the skill file.
set -euo pipefail

export NO_COLOR="${NO_COLOR:-1}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BUDI="$ROOT/target/release/budi"

if [[ ! -x "$BUDI" ]]; then
  echo "error: release binary not built. run \`cargo build --release\` first." >&2
  exit 2
fi

TMPDIR_ROOT="$(mktemp -d -t budi-e2e-603-skill-XXXXXX)"
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

SKILL_PATH="$HOME/.claude/skills/budi/SKILL.md"

# Scenario 1: ~/.claude absent — init must NOT create the directory or
# the skill file. This mirrors the statusline gate (#454): without
# Claude Code installed, budi has no business creating ~/.claude.
echo "[e2e] scenario 1: ~/.claude absent — init must not create the skill"
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

# Scenario 2: ~/.claude present — init installs the skill.
echo "[e2e] scenario 2: ~/.claude present — init installs the /budi skill"
mkdir -p "$HOME/.claude"
"$BUDI" init --no-daemon >"$TMPDIR_ROOT/init-2.log" 2>&1 || {
  cat "$TMPDIR_ROOT/init-2.log" >&2
  echo "[e2e] FAIL: init run in scenario 2 failed" >&2
  exit 1
}
if [[ ! -f "$SKILL_PATH" ]]; then
  echo "[e2e] FAIL: expected $SKILL_PATH to exist after init" >&2
  cat "$TMPDIR_ROOT/init-2.log" >&2
  exit 1
fi
if ! grep -q '^name: budi$' "$SKILL_PATH"; then
  echo "[e2e] FAIL: skill file missing canonical YAML name field" >&2
  cat "$SKILL_PATH" >&2
  exit 1
fi
if ! grep -q 'budi sessions current' "$SKILL_PATH"; then
  echo "[e2e] FAIL: skill file does not invoke \`budi sessions current\`" >&2
  cat "$SKILL_PATH" >&2
  exit 1
fi
echo "[e2e] OK: skill file written to $SKILL_PATH"

# Scenario 2b: idempotence at the byte level. The acceptance criteria
# pin that user edits to SKILL.md must not be silently clobbered, so
# the second install path must not rewrite the file when the bytes
# already match the canonical template.
echo "[e2e] scenario 2b: second init run is byte-stable"
COPY_BEFORE="$TMPDIR_ROOT/skill-before-2b.md"
cp "$SKILL_PATH" "$COPY_BEFORE"
# Capture mtime to a high-resolution string so a same-second write is
# still detected. macOS stat uses -f, GNU stat uses -c — try both.
stat_mtime() {
  if stat -f "%m" "$1" >/dev/null 2>&1; then
    stat -f "%m" "$1"
  else
    stat -c "%Y" "$1"
  fi
}
MTIME_BEFORE="$(stat_mtime "$SKILL_PATH")"
# Sleep a bit so a re-write would produce a different mtime even on
# 1-second-resolution filesystems.
sleep 1
"$BUDI" init --no-daemon >"$TMPDIR_ROOT/init-2b.log" 2>&1 || {
  cat "$TMPDIR_ROOT/init-2b.log" >&2
  echo "[e2e] FAIL: second init run failed" >&2
  exit 1
}
if ! cmp -s "$COPY_BEFORE" "$SKILL_PATH"; then
  echo "[e2e] FAIL: skill file changed across idempotent runs" >&2
  diff "$COPY_BEFORE" "$SKILL_PATH" >&2 || true
  exit 1
fi
MTIME_AFTER="$(stat_mtime "$SKILL_PATH")"
if [[ "$MTIME_BEFORE" != "$MTIME_AFTER" ]]; then
  echo "[e2e] FAIL: idempotent install rewrote the file (mtime changed: $MTIME_BEFORE → $MTIME_AFTER)" >&2
  exit 1
fi
echo "[e2e] OK: second init left skill file byte- and mtime-stable"

# Scenario 2c: integrations list reports the skill as installed.
echo "[e2e] scenario 2c: integrations list reflects installed state"
"$BUDI" integrations list >"$TMPDIR_ROOT/integrations-2c.log" 2>&1 || {
  cat "$TMPDIR_ROOT/integrations-2c.log" >&2
  echo "[e2e] FAIL: integrations list failed" >&2
  exit 1
}
if ! grep -E 'Claude Code /budi skill\s+installed' "$TMPDIR_ROOT/integrations-2c.log" >/dev/null; then
  echo "[e2e] FAIL: integrations list did not report the /budi skill as installed" >&2
  cat "$TMPDIR_ROOT/integrations-2c.log" >&2
  exit 1
fi
echo "[e2e] OK: integrations list reports the /budi skill as installed"

# Scenario 3: --no-integrations must NOT write the skill file even when
# ~/.claude is present. Use a fresh HOME so there is no chance of
# carrying state from scenario 2.
echo "[e2e] scenario 3: --no-integrations leaves the skill file absent"
TMPDIR_ROOT3="$(mktemp -d -t budi-e2e-603b-XXXXXX)"
OLD_HOME="$HOME"
export HOME="$TMPDIR_ROOT3"
export BUDI_HOME="$HOME/.local/share/budi"
mkdir -p "$HOME/.claude"
"$BUDI" init --no-daemon --no-integrations >"$TMPDIR_ROOT3/init-3.log" 2>&1 || {
  cat "$TMPDIR_ROOT3/init-3.log" >&2
  echo "[e2e] FAIL: --no-integrations init failed" >&2
  rm -rf "$TMPDIR_ROOT3"
  exit 1
}
if [[ -e "$HOME/.claude/skills/budi/SKILL.md" ]]; then
  echo "[e2e] FAIL: --no-integrations still wrote the skill file" >&2
  cat "$HOME/.claude/skills/budi/SKILL.md" >&2
  rm -rf "$TMPDIR_ROOT3"
  exit 1
fi
echo "[e2e] OK: --no-integrations left the skill file absent"
rm -rf "$TMPDIR_ROOT3"
export HOME="$OLD_HOME"
export BUDI_HOME="$HOME/.local/share/budi"

# Scenario 4: `budi uninstall` removes the skill file and the empty
# parent dir. Run with `--keep-data --yes` so the uninstall is
# non-interactive and we don't drop the daemon's data directory mid-run
# (the daemon is not running here, but `--keep-data` keeps the test
# focused on the integration removal).
echo "[e2e] scenario 4: \`budi uninstall\` removes the skill file"
"$BUDI" uninstall --keep-data --yes >"$TMPDIR_ROOT/uninstall.log" 2>&1 || {
  cat "$TMPDIR_ROOT/uninstall.log" >&2
  echo "[e2e] FAIL: uninstall failed" >&2
  exit 1
}
if [[ -e "$SKILL_PATH" ]]; then
  echo "[e2e] FAIL: uninstall did not remove $SKILL_PATH" >&2
  exit 1
fi
echo "[e2e] OK: uninstall removed the skill file"

echo "[e2e] PASS"
