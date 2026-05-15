#!/usr/bin/env bash
# Run llvm-cov coverage across the workspace.
#
# Usage:
#   scripts/coverage.sh              # text summary
#   scripts/coverage.sh --html       # also write HTML report to target/coverage
#   scripts/coverage.sh --lcov FILE  # also write LCOV file
#
# Tests are forced single-threaded because a handful of pricing/team-pricing
# tests mutate process-wide env vars under a mutex; parallel test threads
# trip a PoisonError when one panics first. `cargo test` runs them serially
# in CI today via the same flag implicitly via test-level locks, but llvm-cov
# re-execs the instrumented binary and needs the flag spelled out.

set -euo pipefail

EMIT_HTML=0
LCOV_PATH=""
while [ $# -gt 0 ]; do
  case "$1" in
    --html) EMIT_HTML=1 ;;
    --lcov) LCOV_PATH="${2:?--lcov requires a path}"; shift ;;
    -h|--help)
      sed -n '2,11p' "$0"
      exit 0
      ;;
    *)
      echo "unknown flag: $1" >&2
      exit 2
      ;;
  esac
  shift
done

if ! command -v cargo-llvm-cov >/dev/null 2>&1; then
  echo "cargo-llvm-cov not installed. Install with:" >&2
  echo "    cargo install cargo-llvm-cov --locked" >&2
  echo "    rustup component add llvm-tools-preview" >&2
  exit 1
fi

cargo llvm-cov --workspace --summary-only -- --test-threads=1

if [ "$EMIT_HTML" = "1" ]; then
  cargo llvm-cov --workspace --html --output-dir target/coverage -- --test-threads=1
  echo "HTML report: target/coverage/html/index.html"
fi

if [ -n "$LCOV_PATH" ]; then
  mkdir -p "$(dirname "$LCOV_PATH")"
  cargo llvm-cov --workspace --lcov --output-path "$LCOV_PATH" -- --test-threads=1
  echo "LCOV report: $LCOV_PATH"
fi
