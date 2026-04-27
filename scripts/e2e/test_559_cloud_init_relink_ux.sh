#!/usr/bin/env bash
# End-to-end regression for issue #559: re-running `budi cloud init
# --api-key NEW_KEY` against an existing `cloud.toml` should NOT print
# the bare "already exists. Pass --force to overwrite" wording. After
# the fix:
#   - non-interactive callers (no TTY) see a rotation-aware error that
#     names the existing org and explicitly mentions `--force` as the
#     escape hatch for the "switching orgs / rotating API key" case;
#   - `--force` still works for the rotation path (with the existing
#     `--yes` requirement to silence the interactive overwrite prompt).
#
# Repro: `budi cloud init --api-key KEY` against a `cloud.toml` left
# over from a previous org link, running under a non-TTY (e.g. piped
# stdin in CI). Pre-fix output: "X already exists. Pass --force to
# overwrite (existing settings will be replaced)." — readable as "you
# did something wrong" and forces the user to rediscover --force every
# time the cloud rotates a key.
#
# Acceptance contract:
# - Error message names the org currently linked in cloud.toml.
# - Error message points at `--force` as the right escape hatch and
#   names the rotation/switch case explicitly.
# - The pre-fix bare wording ("Pass --force to overwrite") is gone.
# - `--force --yes` still rewrites the file with the new key.
set -euo pipefail

export NO_COLOR="${NO_COLOR:-1}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BUDI="$ROOT/target/release/budi"

if [[ ! -x "$BUDI" ]]; then
  echo "error: release binary not built. run \`cargo build --release\` first." >&2
  exit 2
fi

TMPDIR_ROOT="$(mktemp -d -t budi-e2e-559-XXXXXX)"
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

CLOUD_TOML="$HOME/.config/budi/cloud.toml"
mkdir -p "$(dirname "$CLOUD_TOML")"

# Test placeholders are kept obviously synthetic and routed through
# shell variables so secret-scanners (GitGuardian, gitleaks) don't read
# them as a hardcoded credential next to a `--api-key` flag.
OLD_KEY_PLACEHOLDER="placeholder-old-key-not-a-real-credential"
NEW_KEY_PLACEHOLDER="placeholder-new-key-not-a-real-credential"

# Seed an existing cloud.toml that links to org "org_old" with a real
# (non-stub) api_key — i.e. the same shape `budi cloud init --api-key`
# would have written on first install.
cat >"$CLOUD_TOML" <<TOML
[cloud]
enabled = true
api_key = "${OLD_KEY_PLACEHOLDER}"
endpoint = "https://app.getbudi.dev"
device_id = "00000000-0000-4000-8000-000000000001"
org_id = "org_old"

[cloud.sync]
interval_seconds = 300
retry_max_seconds = 300
TOML

# Scenario 1: non-interactive `budi cloud init --api-key NEW_KEY`
# should produce a rotation-aware error rather than the bare
# "already exists" wording. Pipe `</dev/null` so stdin is not a TTY,
# matching the CI / scripted callers the new behaviour preserves.
echo "[e2e] scenario 1: non-interactive --api-key against existing cloud.toml"
ERR_LOG="$TMPDIR_ROOT/init-err-1.log"
set +e
"$BUDI" cloud init --api-key "$NEW_KEY_PLACEHOLDER" \
    </dev/null >"$ERR_LOG" 2>&1
status=$?
set -e
if [[ "$status" -eq 0 ]]; then
  echo "[e2e] FAIL: scenario 1 expected non-zero exit, got 0" >&2
  cat "$ERR_LOG" >&2
  exit 1
fi

if ! grep -q 'org "org_old"' "$ERR_LOG"; then
  echo "[e2e] FAIL: error must name the existing org (org_old)" >&2
  cat "$ERR_LOG" >&2
  exit 1
fi
if ! grep -q -- "--force" "$ERR_LOG"; then
  echo "[e2e] FAIL: error must point at --force as the escape hatch" >&2
  cat "$ERR_LOG" >&2
  exit 1
fi
if ! grep -Eq "switching orgs|rotating" "$ERR_LOG"; then
  echo "[e2e] FAIL: error must call out the rotation/switch case" >&2
  cat "$ERR_LOG" >&2
  exit 1
fi
# Pre-fix wording must NOT leak through — that's the bug we're fixing.
if grep -q 'Pass --force to overwrite' "$ERR_LOG"; then
  echo "[e2e] FAIL: bare pre-fix error wording is still present" >&2
  cat "$ERR_LOG" >&2
  exit 1
fi
# cloud.toml must be unchanged — error path must not write.
if ! grep -q 'org_old' "$CLOUD_TOML"; then
  echo "[e2e] FAIL: error path must not modify cloud.toml" >&2
  cat "$CLOUD_TOML" >&2
  exit 1
fi
if ! grep -qF "$OLD_KEY_PLACEHOLDER" "$CLOUD_TOML"; then
  echo "[e2e] FAIL: error path must preserve the old api_key" >&2
  cat "$CLOUD_TOML" >&2
  exit 1
fi
echo "[e2e] OK: scenario 1 — rotation-aware error, file unchanged"

# Scenario 2: `--force --yes` is the documented non-interactive escape
# hatch for the rotation case. It must replace the api_key without a
# prompt and leave the file in a parseable shape.
echo "[e2e] scenario 2: --force --yes rewrites cloud.toml with the new key"
"$BUDI" cloud init --api-key "$NEW_KEY_PLACEHOLDER" --force --yes \
    --org-id "org_new" --device-id "11111111-1111-4111-8111-111111111111" \
    </dev/null >"$TMPDIR_ROOT/init-2.log" 2>&1 || {
  echo "[e2e] FAIL: --force --yes rotation path failed" >&2
  cat "$TMPDIR_ROOT/init-2.log" >&2
  exit 1
}

if ! grep -qF "$NEW_KEY_PLACEHOLDER" "$CLOUD_TOML"; then
  echo "[e2e] FAIL: --force --yes must write the new api_key into cloud.toml" >&2
  cat "$CLOUD_TOML" >&2
  exit 1
fi
if grep -qF "$OLD_KEY_PLACEHOLDER" "$CLOUD_TOML"; then
  echo "[e2e] FAIL: --force --yes must remove the old api_key" >&2
  cat "$CLOUD_TOML" >&2
  exit 1
fi
if ! grep -q 'org_id = "org_new"' "$CLOUD_TOML"; then
  echo "[e2e] FAIL: --force --yes must seed the new org_id" >&2
  cat "$CLOUD_TOML" >&2
  exit 1
fi
echo "[e2e] OK: scenario 2 — --force --yes rotates cleanly"

echo "[e2e] PASS: cloud init relink UX (#559)"
