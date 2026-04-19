#!/usr/bin/env bash
# End-to-end regression for issue #357: verify 8.2 can detect and remove
# legacy 8.0/8.1 proxy config residue with explicit cleanup, and that
# `budi uninstall` applies the same managed-block removal on upgraded machines.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BUDI="$ROOT/target/release/budi"

if [[ ! -x "$BUDI" ]]; then
  echo "error: release binary not built. run \`cargo build --release\` first." >&2
  exit 2
fi

TMPDIR_ROOT="$(mktemp -d -t budi-e2e-357-XXXXXX)"
export HOME="$TMPDIR_ROOT"
export BUDI_HOME="$HOME/.local/share/budi"
export CODEX_HOME="$HOME/custom-codex"

SHELL_FILE="$HOME/.zshrc"
CURSOR_FILE="$HOME/Library/Application Support/Cursor/User/settings.json"
CODEX_FILE="$CODEX_HOME/config.toml"
MANUAL_FILE="$HOME/.bash_profile"

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

assert_contains() {
  local file="$1"
  local needle="$2"
  if ! grep -Fq "$needle" "$file"; then
    echo "[e2e] FAIL: expected '$needle' in $file" >&2
    cat "$file" >&2 || true
    exit 1
  fi
}

assert_not_contains() {
  local file="$1"
  local needle="$2"
  if grep -Fq "$needle" "$file"; then
    echo "[e2e] FAIL: did not expect '$needle' in $file" >&2
    cat "$file" >&2 || true
    exit 1
  fi
}

seed_managed_blocks() {
  mkdir -p "$(dirname "$CURSOR_FILE")" "$CODEX_HOME"

  cat >"$SHELL_FILE" <<'EOF'
export PATH="/usr/local/bin:$PATH"

# >>> budi >>>
export ANTHROPIC_BASE_URL="http://localhost:9878"
export CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1
export OPENAI_BASE_URL="http://localhost:9878"
export COPILOT_PROVIDER_BASE_URL="http://localhost:9878"
export COPILOT_PROVIDER_TYPE="openai"
# <<< budi <<<
EOF

  cat >"$CURSOR_FILE" <<'EOF'
{
  "editor.fontSize": 13,
  // >>> budi >>>
  "openai.baseUrl": "http://localhost:9878"
  // <<< budi <<<
}
EOF

  cat >"$CODEX_FILE" <<'EOF'
# >>> budi >>>
openai_base_url = "http://localhost:9878"
# <<< budi <<<
EOF
}

assert_managed_blocks_removed() {
  assert_not_contains "$SHELL_FILE" "# >>> budi >>>"
  assert_not_contains "$SHELL_FILE" "ANTHROPIC_BASE_URL"
  assert_not_contains "$CURSOR_FILE" "// >>> budi >>>"
  assert_not_contains "$CURSOR_FILE" "openai.baseUrl"
  assert_not_contains "$CODEX_FILE" "# >>> budi >>>"
  assert_not_contains "$CODEX_FILE" "openai_base_url"

  python3 - "$CURSOR_FILE" <<'PY'
import json
import sys

with open(sys.argv[1], "r", encoding="utf-8") as f:
    data = json.load(f)

assert data["editor.fontSize"] == 13, data
assert "openai.baseUrl" not in data, data
PY
}

echo "[e2e] seed 8.1-style managed blocks"
seed_managed_blocks

echo "[e2e] explicit cleanup removes managed blocks"
INIT_LOG="$TMPDIR_ROOT/init-cleanup.log"
"$BUDI" init --cleanup --yes --no-daemon >"$INIT_LOG" 2>&1 || {
  cat "$INIT_LOG" >&2 || true
  echo "[e2e] FAIL: init --cleanup failed" >&2
  exit 1
}
assert_contains "$INIT_LOG" "Cleanup summary: removed 3 file(s), skipped 0."
assert_managed_blocks_removed

echo "[e2e] second cleanup is idempotent"
INIT_LOG_2="$TMPDIR_ROOT/init-cleanup-2.log"
"$BUDI" init --cleanup --yes --no-daemon >"$INIT_LOG_2" 2>&1 || {
  cat "$INIT_LOG_2" >&2 || true
  echo "[e2e] FAIL: second init --cleanup failed" >&2
  exit 1
}
assert_contains "$INIT_LOG_2" "Nothing to clean."

echo "[e2e] reseed managed blocks and add manual shell residue"
seed_managed_blocks
cat >"$MANUAL_FILE" <<'EOF'
export OPENAI_BASE_URL="http://localhost:9878"
EOF

echo "[e2e] uninstall removes managed blocks but leaves manual edits alone"
UNINSTALL_LOG="$TMPDIR_ROOT/uninstall.log"
"$BUDI" uninstall --yes --keep-data >"$UNINSTALL_LOG" 2>&1 || {
  cat "$UNINSTALL_LOG" >&2 || true
  echo "[e2e] FAIL: uninstall failed" >&2
  exit 1
}
assert_contains "$UNINSTALL_LOG" "Removing legacy 8.0/8.1 proxy residue..."
assert_contains "$UNINSTALL_LOG" "manual edits still reference the old proxy"
assert_managed_blocks_removed
assert_contains "$MANUAL_FILE" "OPENAI_BASE_URL"

echo "[e2e] PASS"
