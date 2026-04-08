#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
FRONTEND_DIR="$ROOT_DIR/frontend/dashboard"

if [[ ! -d "$FRONTEND_DIR" ]]; then
  echo "dashboard frontend directory not found: $FRONTEND_DIR" >&2
  exit 1
fi

echo "Building dashboard frontend..."
cd "$FRONTEND_DIR"
if [[ -f package-lock.json ]]; then
  npm ci --silent --no-audit --no-fund
else
  npm install --silent --no-audit --no-fund
fi
npm run build

echo "dashboard build complete."
