# Statusline status contract (provider-scoped)

**Status**: Active — shipped in 8.1 under [#224](https://github.com/siropkin/budi/issues/224)
**Governance**: [ADR-0088](./adr/0088-8x-local-developer-first-product-contract.md) §4

This document pins the shared provider-scoped status contract emitted by Budi's local status surfaces. It is the single JSON shape consumed by:

- the CLI statusline (`budi statusline --format json`)
- the Cursor extension ([#232](https://github.com/siropkin/budi/issues/232), surfaced as a single Cursor status bar item — no sidebar, no side panel)
- the cloud dashboard's provider-scoped views ([#235](https://github.com/siropkin/budi/issues/235))

Provider is an explicit filter rather than a family of per-surface shapes. Future agents added under [#294](https://github.com/siropkin/budi/issues/294) slot into the same shape — they do not each invent their own statusline schema.

## Endpoint

```
GET http://127.0.0.1:7878/analytics/statusline
```

The daemon only accepts loopback connections. The contract is also available via `budi statusline --format json`, which spawns the CLI, resolves context (cwd, session, git branch, repo root), and renders the same response on stdout.

## Query parameters

All parameters are optional. Omit them to get unscoped, context-free totals.

| Parameter      | Type   | Effect |
|----------------|--------|--------|
| `provider`     | string | Scopes every numeric field (`cost_1d` / `cost_7d` / `cost_30d`, `session_cost`, `branch_cost`, `project_cost`) and `active_provider` to one provider. Canonical values: `claude_code`, `cursor`, `codex`, `copilot_cli`. Provider-scoped surfaces (the Claude Code statusline, the Cursor extension) **must** set this. |
| `session_id`   | string | Additionally compute `session_cost` and session health (`health_state`, `health_tip`, `session_msg_cost`). |
| `branch`       | string | Additionally compute `branch_cost` for this git branch. |
| `repo_id`      | string | Scope `branch_cost` to `(repo_id, branch)` so developers who sit on `main` / `master` in multiple repos don't get a cross-repo sum. Only meaningful when `branch` is also set. Format matches `budi_core::repo_id` (e.g. `github.com/siropkin/budi`). |
| `project_dir`  | string | Additionally compute `project_cost` for this directory. |

## Response shape

```jsonc
{
  "cost_1d": 2.34,              // rolling last 24h, dollars
  "cost_7d": 12.50,             // rolling last 7d, dollars
  "cost_30d": 48.10,            // rolling last 30d, dollars
  "provider_scope": "claude_code", // echoes the `provider` filter, if set
  "today_cost": 2.34,           // deprecated alias for cost_1d; removed in 9.0
  "week_cost": 12.50,           // deprecated alias for cost_7d; removed in 9.0
  "month_cost": 48.10,          // deprecated alias for cost_30d; removed in 9.0
  "session_cost": 0.41,         // present iff session_id was passed
  "branch_cost": 8.70,          // present iff branch was passed
  "project_cost": 12.10,        // present iff project_dir was passed
  "active_provider": "claude_code", // most recent provider seen in 24h (filtered)
  "health_state": "green",      // present iff session_id was passed
  "health_tip": "session healthy", // present iff session_id was passed
  "session_msg_cost": 0.03      // present iff session_id was passed
}
```

### Field semantics

- **Windows are rolling**, not calendar. `cost_1d` = spend in the last 24 hours. `cost_7d` = spend in the last 7 days. `cost_30d` = spend in the last 30 days. This is a deliberate shift from 8.0 (which used calendar today / Monday-of-week / first-of-month), governed by ADR-0088 §4. `budi stats` keeps its calendar semantics — the rolling windows live only on the statusline surface.
- **Costs are in dollars**, rounded to two decimal places at the rendering layer.
- **Provider scoping is strict.** When `provider=claude_code`, a machine that also uses Cursor will not see Cursor spend in `cost_1d` / `cost_7d` / `cost_30d`. This is the fix for the 8.0 bug where Claude Code's statusline showed blended multi-provider totals (ADR-0088 §4, #224).
- **Empty window vs stalled data.** All three cost fields are always present and default to `0.0` when the DB has no matching rows. An empty 30d window with a healthy daemon means "you have not used this provider in 30 days", not "the daemon is broken".
- **`active_provider`** is the most recent `provider` value seen inside the 1d window, after the provider filter is applied. It exists so downstream surfaces can render "last touched" hints without a second API call.
- **`provider_scope`** echoes back the filter the daemon applied. Consumers should display the scope alongside totals when the filter is active.
- **Deprecated fields** (`today_cost`, `week_cost`, `month_cost`) are populated with the same rolling values as `cost_1d` / `cost_7d` / `cost_30d` for one release of backward compatibility with 8.0 consumers that predate this contract. They are removed in 9.0. New consumers MUST read the `cost_1d` / `cost_7d` / `cost_30d` fields.
- **`branch_cost` is repo-scoped when `repo_id` is passed.** Consumers that can resolve a repo identity (the CLI does this via `budi_core::repo_id` when it has a cwd) should pass `repo_id` alongside `branch` so `branch_cost` reflects "this branch in this repo" rather than "this branch name across every repo on the machine". Omitting `repo_id` preserves the pre-8.2.1 behavior, which sums across all repos that share the branch name (#347).

## Stability guarantees

- Field names in this contract are stable across 8.x minor releases. New optional fields may be added; existing fields are not renamed or re-typed.
- When a field is deprecated, it is kept populated for at least one minor release and removed no sooner than the next major release. Deprecations are noted inline in the struct documentation and in the CHANGELOG.
- Breaking changes to this contract require an ADR update.

## Reference implementations

- Rust types: `budi_core::analytics::queries::{StatuslineStats, StatuslineParams}` in `crates/budi-core/src/analytics/queries.rs`
- Daemon handler: `analytics_statusline` in `crates/budi-daemon/src/routes/analytics.rs`
- CLI consumer: `cmd_statusline` in `crates/budi-cli/src/commands/statusline.rs`
- Config + slot vocabulary: `crates/budi-core/src/config.rs` (`STATUSLINE_SLOTS`, `STATUSLINE_PRESETS`, `normalize_statusline_slot`)

## Consumer playbook

### Claude Code statusline (CLI)

```sh
# Default — no arguments needed; ADR-0088 §4 compliant:
budi statusline

# Under the hood this is:
# budi statusline --format claude --provider claude_code
```

### Cursor extension ([#232](https://github.com/siropkin/budi/issues/232))

```sh
budi statusline --format json --provider cursor
```

### Cloud dashboard provider-scoped tiles ([#235](https://github.com/siropkin/budi/issues/235))

The dashboard calls the ingest service, not the local daemon directly, but it MUST present windows labeled `1d` / `7d` / `30d` and MUST filter by a single provider in provider-scoped tiles. Unscoped rollups ("all agents combined") are allowed only in the multi-agent summary view.
