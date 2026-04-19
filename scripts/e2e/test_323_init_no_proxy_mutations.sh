#!/usr/bin/env bash
# End-to-end regression for issue #323: verify `budi init` no longer creates
# legacy proxy-routing mutation files (shell profile, Cursor settings.json, or
# Codex config.toml), and remains idempotent on a second run.
#
# Contract pinned:
# - #316 R2.2 (stop new writes)
# - #323 acceptance ("fresh init touches no legacy proxy mutation files")
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BUDI="$ROOT/target/release/budi"

if [[ ! -x "$BUDI" ]]; then
  echo "error: release binary not built. run \`cargo build --release\` first." >&2
  exit 2
fi

TMPDIR_ROOT="$(mktemp -d -t budi-e2e-323-XXXXXX)"
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

assert_absent() {
  local path="$1"
  if [[ -e "$path" ]]; then
    echo "[e2e] FAIL: expected no file at $path" >&2
    exit 1
  fi
}

run_init_once() {
  "$BUDI" init \
    --yes \
    --integrations none \
    --no-daemon \
    --no-open \
    --no-sync
}

assert_no_legacy_proxy_mutations() {
  assert_absent "$HOME/.zshrc"
  assert_absent "$HOME/.bashrc"
  assert_absent "$HOME/.bash_profile"
  assert_absent "$HOME/.config/fish/config.fish"
  assert_absent "$HOME/.cursor/settings.json"
  assert_absent "$HOME/.codex/config.toml"
}

echo "[e2e] first init run"
LOG1="$TMPDIR_ROOT/init-1.log"
run_init_once >"$LOG1" 2>&1 || {
  cat "$LOG1" >&2 || true
  echo "[e2e] FAIL: first init run failed" >&2
  exit 1
}
assert_no_legacy_proxy_mutations
echo "[e2e] OK: first init created no legacy proxy mutation files"

echo "[e2e] second init run (idempotence)"
LOG2="$TMPDIR_ROOT/init-2.log"
run_init_once >"$LOG2" 2>&1 || {
  cat "$LOG2" >&2 || true
  echo "[e2e] FAIL: second init run failed" >&2
  exit 1
}
assert_no_legacy_proxy_mutations
echo "[e2e] OK: second init remains free of legacy proxy mutation files"

echo "[e2e] PASS"
