# Statusline status contract (provider-scoped)

**Status**: Active — shipped in 8.1 under [#224](https://github.com/siropkin/budi/issues/224); host-scoped multi-provider extension shipped in 8.4 under [#650](https://github.com/siropkin/budi/issues/650)
**Governance**: [ADR-0088](./adr/0088-8x-local-developer-first-product-contract.md) §4 + §7 (post-#648)

This document pins the shared status contract emitted by Budi's local status surfaces. It is the single JSON shape consumed by:

- the CLI statusline (`budi statusline --format json`)
- the Cursor / VS Code extensions ([#232](https://github.com/siropkin/budi/issues/232), surfaced as a single editor status bar item — no sidebar, no side panel)
- the cloud dashboard's provider-scoped views ([#235](https://github.com/siropkin/budi/issues/235))

Provider is an explicit filter rather than a family of per-surface shapes. Future agents added under [#294](https://github.com/siropkin/budi/issues/294) slot into the same shape — they do not each invent their own statusline schema.

The same endpoint serves two scopes (ADR-0088 §7 post-#648):

- **Provider-scoped** — `?provider=<single>`. Cloud dashboard, per-provider drill-downs, and the Claude Code statusline. No blending across providers.
- **Host-scoped** — `?provider=<a>,<b>,<c>`. The VS Code / Cursor extension status bar aggregates over the providers detected in that editor host. The response carries a `contributing_providers` list for tooltip rendering and click-through routing.

## Endpoint

```
GET http://127.0.0.1:7878/analytics/statusline
```

The daemon only accepts loopback connections. The contract is also available via `budi statusline --format json`, which spawns the CLI, resolves context (cwd, session, git branch, repo root), and renders the same response on stdout.

## Query parameters

All parameters are optional. Omit them to get unscoped, context-free totals.

| Parameter      | Type                  | Effect |
|----------------|-----------------------|--------|
| `provider`     | string \| comma-list  | Scopes every numeric field (`cost_1d` / `cost_7d` / `cost_30d`, `session_cost`, `branch_cost`, `project_cost`) and `active_provider`. **Single value** (`?provider=cursor`) — provider-scoped, byte-identical to the 8.1 contract. **Comma-list** (`?provider=cursor,copilot_chat`) — host-scoped, aggregates the listed providers and populates `contributing_providers`. Canonical values: `claude_code`, `cursor`, `codex`, `copilot_cli`, `copilot_chat`. Empty / absent → unscoped (sums every provider). Whitespace and duplicate names are normalized away. The repeated form `?provider=a&provider=b` is **not** supported (axum's default `Query` extractor takes the last value only) — callers that need multi-provider must use the comma-list form. |
| `session_id`   | string                | Additionally compute `session_cost` and session health (`health_state`, `health_tip`, `session_msg_cost`). |
| `branch`       | string                | Additionally compute `branch_cost` for this git branch. |
| `repo_id`      | string                | Scope `branch_cost` to `(repo_id, branch)` so developers who sit on `main` / `master` in multiple repos don't get a cross-repo sum. Only meaningful when `branch` is also set. Format matches `budi_core::repo_id` (e.g. `github.com/siropkin/budi`). |
| `project_dir`  | string                | Additionally compute `project_cost` for this directory. |

## Response shape

```jsonc
{
  "cost_1d": 2.34,              // rolling last 24h, dollars
  "cost_7d": 12.50,             // rolling last 7d, dollars
  "cost_30d": 48.10,            // rolling last 30d, dollars
  "provider_scope": "claude_code", // echoes the `provider` filter when exactly one provider was passed; omitted otherwise
  "contributing_providers": ["cursor", "copilot_chat"], // present iff multi-provider; tooltip + click-through source
  "today_cost": 2.34,           // deprecated alias for cost_1d; removed in 9.0
  "week_cost": 12.50,           // deprecated alias for cost_7d; removed in 9.0
  "month_cost": 48.10,          // deprecated alias for cost_30d; removed in 9.0
  "session_cost": 0.41,         // present iff session_id was passed
  "branch_cost": 8.70,          // present iff branch was passed
  "project_cost": 12.10,        // present iff project_dir was passed
  "active_provider": "claude_code", // most recent provider seen in 24h (filtered); under multi-provider, the most recent from the contributing set
  "health_state": "green",      // present iff session_id was passed
  "health_tip": "session healthy", // present iff session_id was passed
  "session_msg_cost": 0.03,     // present iff session_id was passed
  "cost_lag_hint": "..."        // present iff Cursor data is in the response (active or contributing); ~10 min Usage API lag
}
```

### Field semantics

- **Windows are rolling**, not calendar. `cost_1d` = spend in the last 24 hours. `cost_7d` = spend in the last 7 days. `cost_30d` = spend in the last 30 days. This is a deliberate shift from 8.0 (which used calendar today / Monday-of-week / first-of-month), governed by ADR-0088 §4. `budi stats` and the cloud dashboard's cost charts keep calendar semantics — the rolling windows live only on the statusline surface and the Cursor extension that renders this contract. See [README → Windows: rolling vs calendar](../README.md#windows-rolling-vs-calendar) for the user-facing explanation.
- **All `*_cost` fields are decimal dollars** (e.g. `0.08` for 8 cents), rounded to two decimal places at the rendering layer. This includes `cost_1d` / `cost_7d` / `cost_30d`, the deprecated `today_cost` / `week_cost` / `month_cost` aliases, `session_cost`, `branch_cost`, `project_cost`, **and `session_msg_cost`**. There are no per-field unit exceptions; the wire format is one unit, top to bottom. (Pre-8.4.2 / `api_version=1` daemons emitted `session_msg_cost` in cents — see #692. Host extensions that were compiled against the cents contract should require `api_version >= 2` and surface their existing remediation banner against older daemons.)
- **Provider scoping is strict for provider-scoped surfaces.** When `provider=claude_code`, a machine that also uses Cursor will not see Cursor spend in `cost_1d` / `cost_7d` / `cost_30d`. This is the fix for the 8.0 bug where Claude Code's statusline showed blended multi-provider totals (ADR-0088 §4, #224). Multi-provider requests (host-scoped) opt in by passing a comma-list and explicitly aggregate over the listed providers per ADR-0088 §7 (post-#648).
- **Empty window vs stalled data.** All three cost fields are always present and default to `0.0` when the DB has no matching rows. An empty 30d window with a healthy daemon means "you have not used this provider in 30 days", not "the daemon is broken".
- **`active_provider`** is the most recent `provider` value seen inside the 1d window, after the provider filter is applied. It exists so downstream surfaces can render "last touched" hints without a second API call. Under multi-provider, it is the most recent provider drawn from the contributing set — host-scoped click-through routes to its dashboard.
- **`provider_scope`** echoes back the filter the daemon applied **only when exactly one provider was passed**, preserving the 8.1 byte shape. For multi-provider requests it is omitted; the active scope is advertised via `contributing_providers` instead.
- **`contributing_providers`** is populated only for multi-provider requests (the comma-list form). It is the deduplicated, normalized list of providers the response sums over, in the input order. Single-provider requests omit the field. Consumers that render a tooltip should join the entries with their own separator (e.g. `Cursor + Copilot Chat`).
- **Unknown provider names are not errors.** A name with no matching rows contributes `0.0` to every numeric field and survives in `contributing_providers`. This keeps the host-scoped rollup forgiving when a host-detector advertises a provider that hasn't ingested any messages yet.
- **`cost_lag_hint`** fires when Cursor data is in the response — either as the active provider or as a member of the contributing set — because the Cursor Usage API can lag up to ~10 minutes (ADR-0090).
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
- Config + slot vocabulary: `crates/budi-core/src/config.rs` (`STATUSLINE_SLOTS`, `normalize_statusline_slot`)

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

### VS Code / multi-provider host extensions ([#650](https://github.com/siropkin/budi/issues/650))

The VS Code extension calls the daemon directly with the comma-list form so a single editor window that runs Copilot Chat, Continue, and Cline is summed honestly:

```
GET http://127.0.0.1:7878/analytics/statusline?provider=cursor,copilot_chat
```

The response shape is identical to the single-provider response except that `provider_scope` is omitted and `contributing_providers` lists the requested providers. Render the totals from `cost_1d` / `cost_7d` / `cost_30d` and the tooltip from `contributing_providers`; click-through routes to `active_provider`'s dashboard.

### Cloud dashboard provider-scoped tiles ([#235](https://github.com/siropkin/budi/issues/235))

The dashboard calls the ingest service, not the local daemon directly, but it MUST present windows labeled `1d` / `7d` / `30d` and MUST filter by a single provider in provider-scoped tiles. Unscoped rollups ("all agents combined") are allowed only in the multi-agent summary view.
