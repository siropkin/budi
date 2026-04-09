# Issue #59 Review: Configuration, Migrations, Privacy, and Compatibility

## Findings (highest severity first)

### P1 (fixed): README pointed daemon config to the wrong path
- Area: `README.md` install/init guidance
- Risk: docs said to edit `~/.config/budi/config.toml` for `daemon_port`, but daemon host/port config is repo-local (`<budi-home>/repos/<repo-id>/config.toml`). This could send users to the wrong file and cause failed port changes, confusing daemon restart behavior, and upgrade troubleshooting noise.
- Fix in this PR: updated README to describe repo-local config location and how to discover the exact file with `budi doctor`.

### P2 (fixed): Empty env overrides could silently redirect config/data paths
- Area: `crates/budi-core/src/config.rs` (`home_dir`, `budi_home_dir`)
- Risk: empty/whitespace values for `HOME` or `BUDI_HOME` were accepted as valid paths (`PathBuf::from("")`), which can resolve to cwd-relative locations and unintentionally relocate data/config behavior.
- Fix in this PR: normalize env-based path inputs, reject blank values, and fall back to standard path resolution.

## Migration/config scenarios that need explicit automated coverage

1. `repair` on schema-v21 DBs with dropped/recreated indexes that keep legacy names but wrong definitions.
2. Upgrade path validation for versions `<10` with explicit user-facing backup/restore expectations around destructive rebuild.
3. Mixed-version CLI/daemon startup behavior when repo-local daemon port differs from default and commands run outside a repo.
4. Explicit compatibility checks for `BUDI_HOME`, `HOME`, and Windows path env variants when values are missing, blank, or whitespace-padded.
5. Retention policy boundaries around session timestamps exactly at cutoff windows (`julianday('now', '-N days')` edge behavior).

## Test coverage added

- `parse_env_path_rejects_blank_values` in `crates/budi-core/src/config.rs`
- `parse_env_path_trims_whitespace` in `crates/budi-core/src/config.rs`

## Documentation drift corrected

- `README.md`
  - Corrected daemon config location for `daemon_port` to repo-local storage (`<budi-home>/repos/<repo-id>/config.toml`), plus discoverability guidance via `budi doctor`.
- `SOUL.md`
  - Corrected migration key-file note from schema v20 to schema v21.

## Follow-up candidates (not in this PR)

- Add a `budi config path` command to print active repo-local config and eliminate path ambiguity.
- Add migration-integrity assertions that validate critical index definitions (not just index-name existence) during `repair`.
- Expand privacy retention tests for exact-cutoff behavior and disabled (`off`) windows through environment-driven integration tests.
