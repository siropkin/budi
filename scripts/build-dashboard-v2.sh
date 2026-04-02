#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
FRONTEND_DIR="$ROOT_DIR/frontend/dashboard-v2"

if [[ ! -d "$FRONTEND_DIR" ]]; then
  echo "dashboard-v2 frontend directory not found: $FRONTEND_DIR" >&2
  exit 1
fi

echo "Building dashboard-v2 frontend..."
cd "$FRONTEND_DIR"
npm install
npm run build

echo "dashboard-v2 build complete."
