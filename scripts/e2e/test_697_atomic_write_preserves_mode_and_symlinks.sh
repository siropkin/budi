#!/usr/bin/env bash
# Smoke gate for #697 — `atomic_write_json` must preserve POSIX mode bits
# AND symlinks on the files it rewrites under ~/.claude.
#
# Boots a release-built `budi` against a throwaway HOME and asserts:
#
#   1. `chmod 600 ~/.claude/settings.json` survives a
#      `budi integrations install --with claude-code-statusline --yes`.
#      (Privacy regression — the file holds env vars, OTEL endpoints,
#      sometimes anthropic keys; relaxing it to 644 by umask is silent
#      data exposure.)
#
#   2. A symlinked `~/.claude/settings.json -> ~/dotfiles/...` survives
#      the same install: the symlink stays, the real (canonicalized)
#      target gets the updated content. (Correctness regression for
#      chezmoi / stow / yadm users who manage their config from a
#      Git-tracked dotfiles repo.)
#
# Negative-prove the gate: revert the mode/symlink preservation in
# `crates/budi-cli/src/commands/mod.rs::atomic_write_json` and rerun
# this script — both assertions should flip from PASS to FAIL.
#
# Run:
#   cargo build --release
#   bash scripts/e2e/test_697_atomic_write_preserves_mode_and_symlinks.sh
#   KEEP_TMP=1 bash scripts/e2e/test_697_atomic_write_preserves_mode_and_symlinks.sh
set -euo pipefail

export NO_COLOR="${NO_COLOR:-1}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BUDI="$ROOT/target/release/budi"

if [[ ! -x "$BUDI" ]]; then
  echo "error: release binary not built. run \`cargo build --release\` first." >&2
  exit 2
fi

TMPDIR_ROOT="$(mktemp -d -t budi-e2e-697-XXXXXX)"
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

step() {
  echo
  echo "=================================================================="
  echo "[e2e] $*"
  echo "=================================================================="
}

# Portable mode reader — `stat -c` on Linux, `stat -f` on macOS / BSD.
read_mode() {
  local path="$1"
  if stat -c '%a' "$path" >/dev/null 2>&1; then
    stat -c '%a' "$path"
  else
    stat -f '%Lp' "$path"
  fi
}

echo "[e2e] HOME=$HOME"

# ------------------------------------------------------------------
# Scenario 1 — chmod 600 settings.json must survive the install.
# ------------------------------------------------------------------
step "scenario 1: chmod 600 ~/.claude/settings.json must survive integrations install"

mkdir -p "$HOME/.claude"
SETTINGS="$HOME/.claude/settings.json"
echo '{}' > "$SETTINGS"
chmod 600 "$SETTINGS"

before_mode="$(read_mode "$SETTINGS")"
if [[ "$before_mode" != "600" ]]; then
  echo "[e2e] FAIL: pre-install mode was $before_mode, expected 600" >&2
  exit 1
fi
echo "[e2e] pre-install mode: $before_mode"

"$BUDI" integrations install --with claude-code-statusline --yes \
  >"$TMPDIR_ROOT/install-1.log" 2>&1 || {
  cat "$TMPDIR_ROOT/install-1.log" >&2
  echo "[e2e] FAIL: \`integrations install\` exited non-zero" >&2
  exit 1
}

if [[ ! -f "$SETTINGS" ]]; then
  echo "[e2e] FAIL: $SETTINGS missing after install" >&2
  exit 1
fi

after_mode="$(read_mode "$SETTINGS")"
echo "[e2e] post-install mode: $after_mode"
if [[ "$after_mode" != "600" ]]; then
  echo "[e2e] FAIL: mode bits were not preserved (expected 600, got $after_mode)" >&2
  ls -l "$SETTINGS" >&2
  exit 1
fi

if ! grep -q '"statusLine"' "$SETTINGS"; then
  echo "[e2e] FAIL: install ran but did not write the statusLine entry" >&2
  cat "$SETTINGS" >&2
  exit 1
fi
echo "[e2e] OK: mode 600 preserved AND statusLine installed"

# ------------------------------------------------------------------
# Scenario 2 — symlinked settings.json must stay a symlink, content
# must land at the canonical target (dotfile-manager contract).
# ------------------------------------------------------------------
step "scenario 2: symlinked settings.json must stay a symlink, content updates at the canonical target"

# Reset HOME state — the previous scenario already wrote the file, and
# we want a clean slate for the symlink test.
rm -f "$SETTINGS"

DOTFILES_DIR="$HOME/dotfiles/claude"
DOTFILES_TARGET="$DOTFILES_DIR/settings.json"
mkdir -p "$DOTFILES_DIR"
echo '{}' > "$DOTFILES_TARGET"

ln -s "$DOTFILES_TARGET" "$SETTINGS"

if [[ ! -L "$SETTINGS" ]]; then
  echo "[e2e] FAIL: pre-install $SETTINGS is not a symlink" >&2
  exit 1
fi
echo "[e2e] pre-install symlink target: $(readlink "$SETTINGS")"

"$BUDI" integrations install --with claude-code-statusline --yes \
  >"$TMPDIR_ROOT/install-2.log" 2>&1 || {
  cat "$TMPDIR_ROOT/install-2.log" >&2
  echo "[e2e] FAIL: \`integrations install\` exited non-zero in symlink scenario" >&2
  exit 1
}

if [[ ! -L "$SETTINGS" ]]; then
  echo "[e2e] FAIL: post-install $SETTINGS is no longer a symlink" >&2
  ls -l "$SETTINGS" >&2
  exit 1
fi

post_target="$(readlink "$SETTINGS")"
if [[ "$post_target" != "$DOTFILES_TARGET" ]]; then
  echo "[e2e] FAIL: symlink target changed (expected $DOTFILES_TARGET, got $post_target)" >&2
  exit 1
fi
echo "[e2e] post-install symlink target: $post_target"

if ! grep -q '"statusLine"' "$DOTFILES_TARGET"; then
  echo "[e2e] FAIL: canonical target $DOTFILES_TARGET did not receive the statusLine update" >&2
  cat "$DOTFILES_TARGET" >&2
  exit 1
fi
echo "[e2e] OK: symlink preserved, content landed at $DOTFILES_TARGET"

step "all assertions passed"
exit 0
