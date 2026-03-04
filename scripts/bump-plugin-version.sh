#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

PLUGIN_NAME="budi-hooks"
PLUGIN_JSON="$REPO_ROOT/plugins/budi-hooks/.claude-plugin/plugin.json"
MARKETPLACE_JSON="$REPO_ROOT/.claude-plugin/marketplace.json"

usage() {
  cat <<'EOF'
Usage: scripts/bump-plugin-version.sh <version>

Bumps Claude plugin version in:
  - plugins/budi-hooks/.claude-plugin/plugin.json
  - .claude-plugin/marketplace.json (budi-hooks entry)

Example:
  ./scripts/bump-plugin-version.sh 2.0.0
EOF
}

fail() {
  printf '[budi-plugin-version] ERROR: %s\n' "$*" >&2
  exit 1
}

log() {
  printf '[budi-plugin-version] %s\n' "$*"
}

VERSION="${1:-}"
if [[ -z "$VERSION" ]]; then
  usage
  fail "Version is required"
fi

if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  fail "Version must use semantic version format X.Y.Z (got: $VERSION)"
fi

[[ -f "$PLUGIN_JSON" ]] || fail "Missing plugin manifest: $PLUGIN_JSON"
[[ -f "$MARKETPLACE_JSON" ]] || fail "Missing marketplace manifest: $MARKETPLACE_JSON"

python3 - "$VERSION" "$PLUGIN_NAME" "$PLUGIN_JSON" "$MARKETPLACE_JSON" <<'PY'
import json
import sys
from pathlib import Path

version, plugin_name, plugin_json_path, marketplace_json_path = sys.argv[1:]
plugin_json_path = Path(plugin_json_path)
marketplace_json_path = Path(marketplace_json_path)

plugin_manifest = json.loads(plugin_json_path.read_text(encoding="utf-8"))
if plugin_manifest.get("name") != plugin_name:
    raise SystemExit(
        f"Plugin manifest name mismatch: expected {plugin_name}, got {plugin_manifest.get('name')}"
    )
plugin_manifest["version"] = version

marketplace = json.loads(marketplace_json_path.read_text(encoding="utf-8"))
entries = [p for p in marketplace.get("plugins", []) if p.get("name") == plugin_name]
if len(entries) != 1:
    raise SystemExit(
        f"Expected exactly one '{plugin_name}' entry in marketplace.json, found {len(entries)}"
    )
entries[0]["version"] = version

plugin_json_path.write_text(json.dumps(plugin_manifest, indent=2) + "\n", encoding="utf-8")
marketplace_json_path.write_text(json.dumps(marketplace, indent=2) + "\n", encoding="utf-8")
PY

log "Updated plugin and marketplace versions to $VERSION"

if command -v claude >/dev/null 2>&1; then
  log "Running plugin validation"
  claude plugin validate "$REPO_ROOT"
  claude plugin validate "$REPO_ROOT/plugins/budi-hooks"
fi

cat <<EOF
[budi-plugin-version] Done.
[budi-plugin-version] Next release steps:
  1) Ensure workspace Cargo version matches: $VERSION
  2) Commit changes
  3) Tag and push: git tag v$VERSION && git push origin v$VERSION
EOF
