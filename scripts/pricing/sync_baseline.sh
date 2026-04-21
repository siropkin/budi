#!/usr/bin/env bash
# [budi-pricing] refresh embedded LiteLLM pricing baseline
#
# Fetches the current `model_prices_and_context_window.json` from the
# community-maintained BerriAI/litellm repo and drops it into the Rust
# source tree at `crates/budi-core/src/pricing/manifest.embedded.json`,
# where `include_str!` picks it up at the next `cargo build`.
#
# ADR-0091 §10 (refresh discipline): every Budi release runs this script
# once as part of the release checklist. The commit message convention
# is `chore: refresh LiteLLM pricing baseline for v8.X.Y`. No
# hand-editing of the JSON is permitted; if the upstream is
# unreachable on release day, block the release.
#
# Invocation:
#
#   bash scripts/pricing/sync_baseline.sh
#
# After this runs, inspect the diff with `git diff --stat` and sanity
# check the known_model_count moves in the right direction, then
# commit. The CI guard referenced in ADR-0091 §10 refuses a PR where
# the new baseline has fewer models than the previous one — upstream
# is not expected to prune well-known model ids.
#
# Previous upstream snapshot reference (for auditability):
#   upstream: https://github.com/BerriAI/litellm
#   commit:   26fcbc93e52d8f212f818d53c6922a0fdddb4d48
#   date:     2026-04-19
#
# See ADR-0091: docs/adr/0091-model-pricing-manifest-source-of-truth.md

set -euo pipefail

UPSTREAM_URL="https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json"
REPO_ROOT="$(git rev-parse --show-toplevel)"
TARGET="${REPO_ROOT}/crates/budi-core/src/pricing/manifest.embedded.json"
TMP="$(mktemp -t budi-pricing-baseline.XXXXXX.json)"
trap 'rm -f "${TMP}"' EXIT

echo "[sync_baseline] fetching ${UPSTREAM_URL}"
curl --fail --silent --show-error --location --output "${TMP}" "${UPSTREAM_URL}"

# Reject anything that does not parse as a JSON object. Guards against
# a mid-rewrite upstream commit landing us a half-valid payload.
python3 - "${TMP}" <<'PY'
import json, sys
path = sys.argv[1]
with open(path, 'rb') as f:
    raw = f.read()
data = json.loads(raw)
if not isinstance(data, dict):
    raise SystemExit(f"baseline is not a JSON object (got {type(data).__name__})")
count = sum(1 for k in data if k != 'sample_spec')
print(f"[sync_baseline] parsed {count} model entries")
PY

mv "${TMP}" "${TARGET}"
trap - EXIT

echo "[sync_baseline] wrote ${TARGET}"
echo "[sync_baseline] next: git add the file and commit"
