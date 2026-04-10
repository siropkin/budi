# Issue #61 Review: CLI Commands, UX, and Daemon Orchestration

## Findings (highest severity first)

### P1 (fixed): most CLI commands could keep talking to an old daemon version after upgrades
- Area: `crates/budi-cli/src/client.rs` (`DaemonClient::connect`)
- Risk: when `/health` was green, `connect()` skipped `ensure_daemon_running()`. That bypassed the daemon version mismatch guard, so commands like `budi stats`, `budi sync`, `budi health`, and `budi repair` could hit stale daemon routes/schema behavior until users manually restarted.
- User impact: post-upgrade confusion and intermittent command failures despite a "healthy" daemon.
- Fix in this PR: `connect()` now always runs daemon readiness via `ensure_daemon_running()`, so healthy-but-stale daemons are restarted automatically before command traffic.

## Command-by-command weak spots reviewed

- `init`: strong setup flow and recoverability; hidden `--no-daemon`/`--repo-root` remain intentionally internal.
- `doctor`: good diagnostics depth; currently hook and integration checks are comprehensive but produce long output for mixed partial installs.
- `sync`: clear messaging for quick/full/force variants; no confirmation for `--force` is acceptable for a power-user command, but worth monitoring.
- `update`: robust preflight and rollback behavior; now better aligned with command reliability because post-update commands enforce daemon version readiness.
- `uninstall`: cleanup is broad and safe, but `--keep-data` currently preserves config as well (behavior is consistent in code, wording can be clearer in future docs).
- `statusline`: prompt-safe timeout behavior is good; fallback behavior is resilient when daemon is unavailable.
- `integrations`: install/list flows are clear; non-interactive semantics are permissive by design.

## Test gaps addressed in this PR

Added regression tests in `crates/budi-cli/src/client.rs`:
- `ensure_daemon_ready_checks_running_daemon_too`
- `ensure_daemon_ready_still_checks_when_daemon_is_down`
- `ensure_daemon_ready_uses_startup_error_context_when_unhealthy`
- `ensure_daemon_ready_uses_mismatch_error_context_when_healthy`

These lock in the readiness decision path so we don’t regress to "health-only" checks.

## Documentation drift corrected

- `SOUL.md`: updated upgrade behavior note to reflect automatic daemon version verification/restart on first CLI command, with manual restart as fallback.

## Follow-up candidates (not in this PR)

1. Add an integration test that launches mismatched CLI/daemon binaries and verifies auto-restart across a real command path (`budi stats` or `budi sync`).
2. Tighten `uninstall --keep-data` wording and/or behavior to clarify whether config is retained intentionally.
3. Consider a concise/verbose mode toggle for `budi doctor` output to improve UX for routine health checks.
