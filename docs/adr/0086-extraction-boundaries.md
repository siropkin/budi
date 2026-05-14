# ADR-0086: Extraction Boundaries for budi-cursor and budi-cloud

- **Date**: 2026-04-10
- **Status**: Accepted
- **Issue**: [#86](https://github.com/siropkin/budi/issues/86)
- **Milestone**: 8.0.0
- **Depends on**: [ADR-0081](./0081-product-contract-and-deprecation-policy.md), [ADR-0082](./0082-proxy-compatibility-matrix-and-gateway-contract.md), [ADR-0083](./0083-cloud-ingest-identity-and-privacy-contract.md)

## Context

The budi monorepo currently contains three logical products that ADR-0081 identifies for extraction in R4.4:

| Product | Current location | Post-extraction repo |
|---------|-----------------|---------------------|
| **budi-core** | `crates/`, `scripts/`, `homebrew/`, `.github/` | `siropkin/budi` (stays) |
| **budi-cursor** | `extensions/cursor-budi/` | `siropkin/budi-cursor` (new) |
| **budi-cloud** | `frontend/dashboard/` + future cloud ingest API | `siropkin/budi-cloud` (new) |

Before extraction happens (R4.4), the boundaries, API contracts, and packaging dependencies must be clearly identified so that:

1. R2–R4 implementation does not introduce new coupling.
2. Extraction is a mechanical directory move, not a redesign.
3. The monorepo continues shipping normally until extraction day.

## Decision

### 1. Product Boundary Definitions

#### budi-core (stays in `siropkin/budi`)

**Owns:**
- Rust workspace: `crates/budi-core/`, `crates/budi-cli/`, `crates/budi-daemon/`
- Build and install scripts: `scripts/`
- Homebrew formula template: `homebrew/`
- CI/CD workflows: `.github/workflows/`
- Top-level docs: `README.md`, `SOUL.md`, `CONTRIBUTING.md`, `AGENTS.md`, `CLAUDE.md`
- ADR documents: `docs/adr/`

**Release artifacts:**
- `budi` binary (CLI)
- `budi-daemon` binary (daemon)
- Platform tarballs/zips published to GitHub Releases
- Homebrew formula in `siropkin/homebrew-budi`

**Config contracts (owned):**
- `~/.config/budi/config.toml` — daemon host/port
- `~/.config/budi/agents.toml` — per-agent enablement
- `~/.config/budi/statusline.toml` — statusline layout
- `~/.config/budi/tags.toml` — custom tag rules
- `~/.config/budi/cloud.toml` — cloud sync credentials (added in R4)
- `~/.local/share/budi/` — data directory (SQLite, logs, repo state)

**Stable APIs (consumed by budi-cursor and budi-cloud):**
- Daemon HTTP API on `127.0.0.1:7878` (see Section 2)
- `budi statusline --format json` CLI command (stdout JSON)
- `~/.local/share/budi/cursor-sessions.json` — session tracking file

#### budi-cursor (extracted to `siropkin/budi-cursor`)

**Owns:**
- VS Code/Cursor extension source: currently `extensions/cursor-budi/`
- Extension package.json, tsconfig, ESLint/Prettier config
- Extension tests

**Release artifacts:**
- `cursor-budi.vsix` — published to VS Code Marketplace (or Open VSX)

**Dependencies on budi-core:**
- Daemon HTTP API (read-only consumer)
- `budi` CLI binary on PATH (spawns `budi statusline --format json`)
- `~/.local/share/budi/cursor-sessions.json` (file watch for active session)
- Daemon port default: `127.0.0.1:7878` (configurable via extension setting `budi.daemonUrl`)

**No compile-time dependencies on budi-core.** The extension is pure TypeScript and communicates over HTTP and process spawn.

#### budi-cloud (extracted to `siropkin/budi-cloud`)

**Owns:**
- Dashboard frontend: currently `frontend/dashboard/`
- Cloud ingest API (Supabase, built in R4.1)
- Cloud dashboard (evolves from local dashboard in R4.3)
- Supabase schema and migrations
- Cloud auth/identity (users, orgs, devices, API keys)

**Release artifacts:**
- Cloud ingest API (deployed service)
- Cloud dashboard (deployed web app)

**Dependencies on budi-core:**
- Sync payload contract (ADR-0083 Section 2): daily rollup records and session summaries
- Ingest API contract: `POST /v1/ingest`, `GET /v1/ingest/status` (ADR-0083 Section 7)
- No code dependency — the cloud consumes JSON payloads pushed by the daemon

### 2. Daemon HTTP API Contract

The daemon API is the primary interface between budi-core and its consumers (budi-cursor, budi-cloud sync worker, Rich CLI). The following endpoints are consumed by budi-cursor today and must remain stable through extraction:

| Endpoint | Method | Consumer | Purpose |
|----------|--------|----------|---------|
| `/analytics/statusline` | GET | budi-cursor, CLI | Statusline data (costs, health) |
| `/analytics/session-health` | GET | budi-cursor | Session vitals and tips |
| `/analytics/sessions` | GET | budi-cursor | Session list with health state |
| `/health` | GET | budi-cursor, CLI | Daemon health + version |

**Versioning policy:** These endpoints are unversioned today (no `/v1/` prefix on the management API). Before extraction, the management API must be versioned so that the extension can detect incompatible daemon versions. Recommended approach:

- Add a `min_api_version` field to the `/health` response.
- The extension checks this field on startup and warns if its expected API version is unsupported.
- This avoids needing to version every endpoint path.

### 3. Current Coupling Points and Untangling Plan

#### 3.1 VSIX Embedding in budi-cli

**Current state:**
- `crates/budi-cli/build.rs` references `../../extensions/cursor-budi/cursor-budi.vsix`
- `crates/budi-cli/src/commands/integrations.rs` uses `include_bytes!("../../../../extensions/cursor-budi/cursor-budi.vsix")` to embed the vsix into the CLI binary
- `budi init` auto-installs the extension into Cursor by writing the embedded vsix to a temp file and running `cursor --install-extension`

**Untangling plan:**
1. **Before extraction (R3.2):** The minimal Cursor bootstrap flow replaces embedded vsix auto-install. Instead of `include_bytes!`, the CLI downloads the latest vsix from the budi-cursor release on GitHub (or VS Code Marketplace) at install time.
2. **At extraction:** Remove `extensions/cursor-budi/` directory. Remove `build.rs` vsix reference. Remove `include_bytes!` from integrations.rs. The CLI's Cursor setup command fetches the vsix from the published release.
3. **CI impact:** The release workflow no longer needs to build the extension before building Rust binaries. The extension has its own release workflow in `siropkin/budi-cursor`.

#### 3.2 Dashboard Embedding in budi-daemon

**Current state:**
- `scripts/build-dashboard.sh` builds `frontend/dashboard/` and outputs to `crates/budi-daemon/static/dashboard-dist/`
- `budi-daemon` uses `include_dir!` to embed the built dashboard and serves it at `/dashboard`
- CI does not build the dashboard (it's pre-committed to `static/dashboard-dist/`)

**Untangling plan:**
1. **Before extraction (R3.3):** The Rich CLI ships as the primary local UX. The dashboard is no longer the primary local interface.
2. **At extraction (R4.4):** Move `frontend/dashboard/` to `siropkin/budi-cloud`. Remove `crates/budi-daemon/static/dashboard-dist/`. Remove the dashboard-serving routes from the daemon. Remove `scripts/build-dashboard.sh`.
3. **After extraction:** The dashboard lives in budi-cloud and is deployed as a web app, not embedded in the daemon binary.

#### 3.3 CI/CD Workflows

**Current state:** Single CI workflow (`.github/workflows/ci.yml`) builds and tests everything:
- Cursor extension: `npm ci`, lint, format check, test, build, package vsix
- Rust: fmt, clippy, test, build (with `BUDI_REQUIRE_CURSOR_VSIX=1`)
- Integration smoke tests (daemon endpoints including dashboard)
- Install/uninstall script tests

Release workflow (`.github/workflows/release.yml`) also builds the extension before the Rust binaries.

**Untangling plan:**
1. **Before extraction:** No CI changes needed. The monorepo CI continues as-is.
2. **At extraction:** Split CI into three workflows:
   - `siropkin/budi`: Rust-only CI (fmt, clippy, test, build, smoke tests). No Node.js setup needed.
   - `siropkin/budi-cursor`: Extension CI (npm ci, lint, format, test, build, package vsix). Publishes vsix to GitHub Releases and/or Marketplace.
   - `siropkin/budi-cloud`: Dashboard + cloud API CI. Build, test, deploy.
3. **Release workflow:** `siropkin/budi` release no longer builds the extension. It packages Rust binaries only.

#### 3.4 Session Tracking File

**Current state:**
- `~/.local/share/budi/cursor-sessions.json` is written by the host-aware extension and consumed by `budi` (the daemon reads it as an optional UX hints file in `doctor`, not as a workspace-resolution oracle). Workspace / repo / branch attribution is per-message and is set by the parsers at ingest time (`repo_id` is resolved by `crate::repo_id::resolve_repo_id`); the file is purely a UX signal layer.

**Wire contract — v1:**

```json
{
  "active_session_id": "uuid-string",
  "updated_at": "ISO-8601"
}
```

**Wire contract — v1.1 (2026-05-12, [#780](https://github.com/siropkin/budi/issues/780), companion to [siropkin/budi-cursor#64](https://github.com/siropkin/budi-cursor/issues/64)):**

The same extension binary may run inside **Cursor** *or* **VS Code** when installed via the VS Code Marketplace / Open VSX. The host is determined by the extension at runtime (`vscode.env.appName`). To keep one canonical wire file for the host-aware extension, **the file path stays `cursor-sessions.json` regardless of host** and grows an optional `surface` field carrying the host id:

```json
{
  "active_session_id": "uuid-string",
  "updated_at": "ISO-8601",
  "surface": "vscode",
  "installed_extensions": {
    "copilot_chat": ["github.copilot-chat"]
  }
}
```

Rules:

1. **Filename stays `cursor-sessions.json`.** The name is a historical artifact; it does not imply the host. Decision rationale: option (A) of #780 ("keep one file, key on content") was picked over (B) "two filenames" because the daemon's per-message workspace resolver already keys on file *content* (`messages.surface`, `repo_id`, `git_branch` columns), not on this file's name. A second filename would have required no daemon work either way — but it would have forced two file watches in the extension and split the doctor permissive-merge logic for no gain.
2. **`surface` is optional.** Allowed values: `cursor`, `vscode`. Unknown / missing values are treated as `cursor` for backward compatibility with budi-cursor 1.x writers. Future hosts add new values.
3. **The daemon is permissive.** `budi doctor`'s loader (`read_session_hint_file` / `merge_hint_extensions` in `crates/budi-cli/src/commands/doctor.rs`) ignores any field it does not recognise. For backward compatibility a second filename `vscode-sessions.json` is *also* read at the same path; the contents of both are merged. This is a compatibility carve-out — the host-aware extension should write only `cursor-sessions.json` going forward.
4. **No version bump needed.** The schema is purely additive — `surface` is optional, ignored when absent, and does not invalidate v1 writers. Both v1 and v1.1 documents coexist on the same path.

**Acceptance signal:** `GET /analytics/statusline?surface=vscode` returns the per-VS-Code rollup based on the `messages.surface` column populated at ingest time (covered by tests in `crates/budi-core/src/analytics/tests.rs` for surface-filter scoping). The wire-contract file itself is consumed only for installed-extension hints in `budi doctor` — its `surface` field is informational, not load-bearing on the analytics path.

This file format must be documented and stable before extraction. Future breaking changes (rename, removal of `active_session_id`, schema bump) require a version bump and an ADR amendment.

#### 3.5 Config File Ownership

All config files live under `~/.config/budi/` and are owned by budi-core. Neither budi-cursor nor budi-cloud writes to these files. The extension reads `budi.daemonUrl` from its own VS Code settings. Cloud credentials in `cloud.toml` are written by `budi cloud join` (a CLI command in budi-core).

No untangling needed — ownership is already clean.

### 4. Extraction Prerequisites Checklist

Each item must be satisfied before R4.4 extraction is allowed.

#### budi-cursor extraction prerequisites

- [x] **R3.2 shipped:** Minimal Cursor bootstrap and status flow is working.
- [x] **VSIX embedding removed:** CLI no longer uses `include_bytes!` for the vsix. Extension is installed via download or marketplace.
- [x] **`build.rs` cleaned:** No references to `../../extensions/cursor-budi/`.
- [x] **Daemon API versioned:** `/health` response includes `api_version` (or `min_api_version`) so the extension can detect incompatible daemons.
- [x] **Session tracking file documented:** `cursor-sessions.json` format is documented in this ADR (see Section 3.4) and stable.
- [x] **Extension CI independent:** Extension can be built, tested, and released without the Rust workspace.
- [x] **Extension README standalone:** `extensions/cursor-budi/README.md` is self-contained (install instructions reference budi-core as a prerequisite, not a sibling directory).

#### budi-cloud extraction prerequisites

- [x] **R3.3 shipped:** Rich CLI is the primary local UX; dashboard removal does not leave a UX gap.
- [x] **R4.1–R4.3 shipped:** Cloud ingest API, sync worker, and dashboard alpha are working.
- [x] **Dashboard build decoupled:** `budi-daemon` no longer embeds `dashboard-dist/`. Dashboard serving routes are removed.
- [x] **`build-dashboard.sh` removed:** No scripts referencing `frontend/dashboard/`.
- [x] **Sync payload contract locked:** ADR-0083 sync envelope is implemented and tested.
- [x] **Cloud config separate:** `cloud.toml` is the only budi-core config that mentions cloud. The cloud service has its own deployment config.
- [x] **Cloud CI independent:** Cloud dashboard and API can be built, tested, and deployed without the Rust workspace.

#### Shared prerequisites

- [x] **No circular dependencies:** budi-core does not import from budi-cursor or budi-cloud. (Already true today.)
- [x] **Version coordination strategy:** Decide whether budi-core and budi-cursor share version numbers or version independently. Recommendation: version independently with a `compatible_daemon_version` range in the extension package.json.
- [x] **Monorepo continues shipping:** Until extraction day, the monorepo can still build and release all three products from a single `cargo build` + npm pipeline.

### 5. Extraction Mechanics (R4.4 Playbook)

When all prerequisites are met, the extraction is a mechanical process:

1. **Create `siropkin/budi-cursor` repo.** Copy `extensions/cursor-budi/` as the repo root. Add CI workflow, LICENSE, README. Tag initial release.
2. **Create `siropkin/budi-cloud` repo.** Copy `frontend/dashboard/` as the starting point. Add cloud API code (built in R4.1–R4.3). Add CI workflow, LICENSE, README. Tag initial release.
3. **Clean up `siropkin/budi`.** Remove `extensions/cursor-budi/`, `frontend/dashboard/`, `crates/budi-daemon/static/dashboard-dist/`, `scripts/build-dashboard.sh`. Update CI to remove Node.js / extension steps. Update release workflow to remove extension packaging.
4. **Update cross-references.** README.md, SOUL.md, and CONTRIBUTING.md should reference the new repos. Issue templates and labels should be scoped to budi-core.
5. **Verify.** Run CI on all three repos independently. Verify the Cursor extension still connects to the daemon. Verify the cloud dashboard still receives synced data.

### 6. What Must Be Stable Before Extraction

| Contract | Owner | Consumers | Stability requirement |
|----------|-------|-----------|----------------------|
| Daemon HTTP API endpoints | budi-core | budi-cursor | Versioned via `/health` `api_version` field |
| `budi statusline --format json` | budi-core | budi-cursor | JSON schema stable; breaking changes require major version bump |
| `cursor-sessions.json` format | budi-core | budi-cursor | Documented format with version field |
| Sync payload envelope (ADR-0083) | budi-core | budi-cloud | `schema_version` field in envelope; server rejects unknown versions |
| Ingest API (`POST /v1/ingest`) | budi-cloud | budi-core | Versioned endpoint; 422 on schema mismatch |
| Daemon port default (7878) | budi-core | budi-cursor | Configurable; extension has `budi.daemonUrl` setting |
| Proxy port default (9878) | budi-core | — | Not consumed by extension or cloud in v1 |

## Consequences

### Expected

- R2–R4 implementers have a clear map of what belongs where and what contracts to respect.
- Extraction in R4.4 is a checklist-driven process, not a design exercise.
- The monorepo continues shipping normally until extraction.
- No new coupling is introduced because the boundaries are documented.

### Trade-offs

- This ADR front-loads documentation work. If the roadmap changes significantly, some of this analysis may need updating.
- The extraction prerequisites add gates to R4.4 that could delay it if earlier rounds leave unfinished work.
- Version coordination between independent repos adds release management overhead compared to the monorepo.

### What This ADR Does NOT Decide

- Specific daemon API version numbers or version negotiation protocol — decided during R3.2 implementation.
- CI/CD tooling for the extracted repos (GitHub Actions vs. other) — decided during R4.4.
- Whether the extension is published to VS Code Marketplace, Open VSX, or GitHub Releases only — decided during R3.2 or R4.4.
- Cloud deployment infrastructure (Supabase hosting, CDN, etc.) — decided during R4.1–R4.3.

---

*Last verified against code on 2026-05-14.*
