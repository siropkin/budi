# Local end-to-end tests

This directory holds shell-driven end-to-end tests for the Budi binaries
(`budi` + `budi-daemon`). They are intentionally kept in bash rather than
Rust integration tests so they can:

- exercise the real release binaries (not a test harness compiled against
  the crates),
- boot the daemon + CLI surface against the live filesystem tailer path,
- drive the system over HTTP exactly the way an agent or user would,
- be run locally, in CI, or during PR review without extra tooling beyond
  `bash`, `curl`, `sqlite3`, and `python3`.

## Running

```bash
# Build once per change.
cargo build --release

# Run one test:
bash scripts/e2e/test_302_sessions_visibility.sh

# Keep the temp HOME around for post-mortem inspection:
KEEP_TMP=1 bash scripts/e2e/test_302_sessions_visibility.sh
```

Every test exits non-zero on the first failure, prints the daemon log, and
cleans up its temp directory on exit (unless `KEEP_TMP=1`).

## Conventions

1. **Name.** `test_<issue>_<short_slug>.sh`, e.g.
   `test_302_sessions_visibility.sh` pins the fix for
   [siropkin/budi#302](https://github.com/siropkin/budi/issues/302).
2. **Header.** Start each script with a comment block that states:
   - what bug or contract it guards,
   - the repro recipe in one paragraph,
   - which `SOUL.md` / ADR sections it enforces.
3. **Isolation.** Always:
   - `export HOME="$(mktemp -d …)"` so the DB under
     `$HOME/.local/share/budi/analytics.db` is fresh;
   - allocate non-default daemon ports so multiple
     scripts can run in parallel;
   - kill children via a `trap cleanup EXIT INT TERM`.
4. **No real network.** Tailer tests should append transcript fixtures under
   provider watch roots and assert daemon/API behavior from local data only.
5. **Assertions.** Prefer explicit, easy-to-read assertions over clever
   one-liners. Fail with a clear `[e2e] FAIL: …` message and include the
   most recent daemon log tail.
6. **Negative-path proof.** Before merging a new regression test, revert
   the fix it guards locally and confirm the script fails. Restore the
   fix; only then land the script.

## Environment overrides used by these scripts

| Variable | Purpose |
|---|---|
| `HOME` | Isolates the Budi data dir (`$HOME/.local/share/budi/`) and repo config (`$HOME/repo/.budi/budi.toml`). |
| `KEEP_TMP=1` | Tells the cleanup trap to leave the temp HOME in place for debugging. |

## Adding a new script

1. Copy an existing script as a starting point.
2. Update the header comment, the ports, and the assertions.
3. Run it locally (it should pass).
4. Revert the fix under test, re-run (it should fail with a clear message).
5. Restore the fix and commit both the script and any supporting changes.
