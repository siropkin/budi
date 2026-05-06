#!/usr/bin/env bash
# End-to-end regression for issue #600: verify `budi init` seeds
# `~/.config/budi/statusline.toml` so users can actually customize the
# statusline. Pre-fix, the file only existed when `--statusline-preset`
# was passed explicitly, leaving a fresh install with nothing to edit
# despite the README pointing users at the file.
#
# Acceptance contract pinned:
# - Fresh `budi init` on a system with `~/.claude` produces
#   `~/.config/budi/statusline.toml` with the default-cost-preset
#   template content.
# - Repeat `budi init` does not overwrite an existing user-edited file
#   (asserted byte-stable).
# - `budi init --no-integrations` does not produce the file.
# - `budi init` on a system without `~/.claude` does not produce the
#   file (same gate as the statusline install itself).
set -euo pipefail

export NO_COLOR="${NO_COLOR:-1}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BUDI="$ROOT/target/release/budi"

if [[ ! -x "$BUDI" ]]; then
  echo "error: release binary not built. run \`cargo build --release\` first." >&2
  exit 2
fi

TMPDIR_ROOT="$(mktemp -d -t budi-e2e-600-XXXXXX)"
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

STATUSLINE_TOML="$HOME/.config/budi/statusline.toml"

# Scenario 1: ~/.claude absent — init must not create the statusline
# file because the statusline install itself is gated on Claude Code
# being present. Mirrors the gate from `install_default_integrations`.
echo "[e2e] scenario 1: ~/.claude absent — init does not seed statusline.toml"
rm -rf "$HOME/.claude" "$HOME/.config"
"$BUDI" init --no-daemon >"$TMPDIR_ROOT/init-1.log" 2>&1 || {
  cat "$TMPDIR_ROOT/init-1.log" >&2
  echo "[e2e] FAIL: init in scenario 1 failed" >&2
  exit 1
}
if [[ -e "$STATUSLINE_TOML" ]]; then
  echo "[e2e] FAIL: init seeded $STATUSLINE_TOML when ~/.claude was absent" >&2
  cat "$STATUSLINE_TOML" >&2
  exit 1
fi
echo "[e2e] OK: scenario 1 — statusline.toml not seeded without ~/.claude"

# Scenario 2: ~/.claude present, no prior config — init must seed
# `~/.config/budi/statusline.toml` with the default slot layout and
# the discoverability comments for example slot combos / custom format.
echo "[e2e] scenario 2: ~/.claude present — init seeds statusline.toml"
rm -rf "$HOME/.config"
mkdir -p "$HOME/.claude"
"$BUDI" init --no-daemon >"$TMPDIR_ROOT/init-2.log" 2>&1 || {
  cat "$TMPDIR_ROOT/init-2.log" >&2
  echo "[e2e] FAIL: init in scenario 2 failed" >&2
  exit 1
}
if [[ ! -f "$STATUSLINE_TOML" ]]; then
  echo "[e2e] FAIL: expected $STATUSLINE_TOML to exist after init" >&2
  cat "$TMPDIR_ROOT/init-2.log" >&2
  exit 1
fi
# Pin the active layout line — this is what the user sees first when
# they `cat` the file. Shifts in this string are user-visible drift.
if ! grep -qE '^slots = \["1d", "7d", "30d"\]$' "$STATUSLINE_TOML"; then
  echo "[e2e] FAIL: $STATUSLINE_TOML missing default slots line" >&2
  cat "$STATUSLINE_TOML" >&2
  exit 1
fi
# Pin the discoverability comments so users hunting for example slot
# combos and the custom `format =` template can find them in the same
# place the README points at. The legacy `preset = "coach" / "full"`
# markers were removed when the `preset` machinery was retired in
# 8.3.18 (#632) and the vestigial `health` slot was dropped (#640).
for marker in '# slots = ["session", "message"]' '# slots = ["session", "message", "1d"]' 'format = ' 'Available slots:'; do
  if ! grep -qF "$marker" "$STATUSLINE_TOML"; then
    echo "[e2e] FAIL: $STATUSLINE_TOML missing marker: $marker" >&2
    cat "$STATUSLINE_TOML" >&2
    exit 1
  fi
done
# Pin the one-line confirmation in init output (init.rs prints this
# after the seed runs). Without it, the user can't tell what just
# happened. NO_COLOR=1 is set above, so no ANSI codes leak in.
if ! grep -qF "$STATUSLINE_TOML" "$TMPDIR_ROOT/init-2.log"; then
  echo "[e2e] FAIL: init did not print statusline.toml path in confirmation" >&2
  cat "$TMPDIR_ROOT/init-2.log" >&2
  exit 1
fi
if ! grep -qF "(edit to customize)" "$TMPDIR_ROOT/init-2.log"; then
  echo "[e2e] FAIL: init did not print the ((edit to customize)) hint" >&2
  cat "$TMPDIR_ROOT/init-2.log" >&2
  exit 1
fi
echo "[e2e] OK: scenario 2 — statusline.toml seeded with template + confirmation printed"

# Scenario 2b: repeat init — must not clobber user edits. Simulate a
# user customization first, then run init again and assert byte-stable.
echo "[e2e] scenario 2b: repeat init must be byte-stable on user-edited file"
USER_EDIT='# my edit
slots = ["session", "branch"]
'
printf '%s' "$USER_EDIT" >"$STATUSLINE_TOML"
SHA_BEFORE="$(shasum -a 256 "$STATUSLINE_TOML" | awk '{print $1}')"
"$BUDI" init --no-daemon >"$TMPDIR_ROOT/init-2b.log" 2>&1 || {
  cat "$TMPDIR_ROOT/init-2b.log" >&2
  echo "[e2e] FAIL: repeat init in scenario 2b failed" >&2
  exit 1
}
SHA_AFTER="$(shasum -a 256 "$STATUSLINE_TOML" | awk '{print $1}')"
if [[ "$SHA_BEFORE" != "$SHA_AFTER" ]]; then
  echo "[e2e] FAIL: repeat init clobbered user-edited statusline.toml" >&2
  echo "  before: $SHA_BEFORE" >&2
  echo "  after:  $SHA_AFTER" >&2
  cat "$STATUSLINE_TOML" >&2
  exit 1
fi
# The confirmation line should *not* fire on the repeat run — that
# would nag the user every `budi init` after the first.
if grep -qF "(edit to customize)" "$TMPDIR_ROOT/init-2b.log"; then
  echo "[e2e] FAIL: repeat init re-printed the seeding hint" >&2
  cat "$TMPDIR_ROOT/init-2b.log" >&2
  exit 1
fi
echo "[e2e] OK: scenario 2b — repeat init byte-stable, no nag"

# Scenario 3: --no-integrations must skip the seed entirely. Even with
# ~/.claude present, the user opted out of integrations, so there's no
# statusline being installed and therefore no file to seed.
echo "[e2e] scenario 3: --no-integrations does not seed statusline.toml"
TMPDIR_ROOT2="$(mktemp -d -t budi-e2e-600b-XXXXXX)"
OLD_HOME="$HOME"
export HOME="$TMPDIR_ROOT2"
export BUDI_HOME="$HOME/.local/share/budi"
mkdir -p "$HOME/.claude"
"$BUDI" init --no-daemon --no-integrations >"$TMPDIR_ROOT2/init-3.log" 2>&1 || {
  cat "$TMPDIR_ROOT2/init-3.log" >&2
  echo "[e2e] FAIL: --no-integrations init failed" >&2
  rm -rf "$TMPDIR_ROOT2"
  exit 1
}
if [[ -e "$HOME/.config/budi/statusline.toml" ]]; then
  echo "[e2e] FAIL: --no-integrations seeded statusline.toml anyway" >&2
  cat "$HOME/.config/budi/statusline.toml" >&2
  rm -rf "$TMPDIR_ROOT2"
  exit 1
fi
rm -rf "$TMPDIR_ROOT2"
export HOME="$OLD_HOME"
export BUDI_HOME="$HOME/.local/share/budi"
echo "[e2e] OK: scenario 3 — --no-integrations leaves statusline.toml absent"

echo "[e2e] PASS"
