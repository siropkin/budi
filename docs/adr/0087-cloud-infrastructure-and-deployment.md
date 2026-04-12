# ADR-0087: Cloud Infrastructure, Deployment, and Domain Strategy

- **Date**: 2026-04-11
- **Status**: Proposed
- **Milestone**: 8.0.0
- **Depends on**: [ADR-0083](./0083-cloud-ingest-identity-and-privacy-contract.md), [ADR-0086](./0086-extraction-boundaries.md)

## Context

ADR-0083 defines the cloud data contract (sync payload, identity model, API surface, privacy guarantees) and ADR-0086 defines the extraction boundaries for budi-cursor and budi-cloud. However, neither specifies:

- Cloud hosting infrastructure (where the API and dashboard run)
- Development vs production environment separation
- Domain strategy and DNS configuration
- Web authentication for dashboard users (distinct from daemon-to-cloud API key auth)
- Technology choices for the cloud dashboard
- Extension marketplace publishing infrastructure
- Daemon lifecycle management (autostart on boot)
- The full repository ecosystem and how repos relate

These decisions must be locked before R4 implementation begins (#100–#105).

## Decision

### 1. Supabase Infrastructure

Two Supabase projects separate development from production:

| Project | Purpose | Used by |
|---------|---------|---------|
| `budi-dev` | Development and staging | Preview deployments, local development, CI tests |
| `budi-prod` | Production | `app.getbudi.dev` and `api.getbudi.dev` |

Both projects share the same schema (managed via versioned migration files in the `budi-cloud` repo). Supabase free tier is sufficient for the cloud alpha (1-20 developers per ADR-0083 §6).

Schema migrations are SQL files committed to the repo and applied via Supabase CLI (`supabase db push`) or the dashboard migration runner. The ingest tables are defined in ADR-0083 §8.

### 2. Vercel Infrastructure

The cloud dashboard and ingest API are deployed as a single Next.js application on Vercel.

One Vercel project (`budi-cloud`) with automatic environments:

| Environment | Trigger | Supabase target | URL |
|-------------|---------|-----------------|-----|
| Preview | Push to any PR branch | `budi-dev` | `*.vercel.app` (auto-generated) |
| Production | Push to `main` | `budi-prod` | `app.getbudi.dev` / `api.getbudi.dev` |

Vercel environment variables connect each deployment to the correct Supabase project:
- `NEXT_PUBLIC_SUPABASE_URL`, `NEXT_PUBLIC_SUPABASE_ANON_KEY` (client-side)
- `SUPABASE_SERVICE_ROLE_KEY` (server-side only, for ingest API)

### 3. Domain Strategy

**Domain**: `getbudi.dev` (registered on GoDaddy).

| Subdomain | Purpose | Hosting | Repo |
|-----------|---------|---------|------|
| `getbudi.dev` | Marketing / landing page | Vercel | `siropkin/getbudi.dev` |
| `app.getbudi.dev` | Cloud dashboard (manager/member views) | Vercel | `siropkin/budi-cloud` |
| `api.getbudi.dev` | Cloud ingest API (daemon sync target) | Vercel API routes | `siropkin/budi-cloud` |

**DNS setup**: Either transfer nameservers from GoDaddy to Vercel (simplest — Vercel manages all DNS) or add CNAME records pointing subdomains to `cname.vercel-dns.com`. Vercel handles TLS certificates automatically.

**ADR-0083 update**: §9 specifies `endpoint = "https://cloud.budi.dev"` as the default cloud endpoint. This ADR updates it to `https://api.getbudi.dev`.

### 4. Web Authentication (Dashboard Users)

ADR-0083 §4 defines daemon-to-cloud auth (API key in `Authorization: Bearer budi_<key>`). This section defines **web browser auth** for the cloud dashboard at `app.getbudi.dev`.

**Provider**: Supabase Auth with three sign-in methods:

| Method | Priority | Target users |
|--------|----------|-------------|
| **GitHub** | Primary | Developers (already have accounts) |
| **Google** | Secondary | Managers who may not be developers |
| **Magic link** (email) | Fallback | Anyone, no password required |

**Onboarding flow**:

1. **Manager** signs up at `app.getbudi.dev` via GitHub or Google
2. Creates an org → receives an org ID and invite link
3. Shares invite link with the team
4. **Developers** click the invite link → sign up via GitHub/Google → automatically linked to the org as `member` role
5. On their local machine, developer runs `budi cloud join <invite-token>` → stores API key and device ID in `~/.config/budi/cloud.toml`

**Supabase Auth user ↔ budi user mapping**: The Supabase Auth `user.id` maps to the `users.id` in ADR-0083 §8. When a user signs up via the web dashboard, a row is created in `users` with a generated `api_key` (`budi_<alphanumeric>`). The API key is shown once for the developer to copy into their local `budi cloud join` flow.

### 5. Cloud Dashboard Technology

| Choice | Decision | Reason |
|--------|----------|--------|
| Framework | Next.js (App Router) | Server-side rendering, API routes, auth middleware, Vercel-native |
| Database client | `@supabase/supabase-js` + `@supabase/ssr` | Type-safe Postgres queries, RLS-aware, SSR support |
| UI library | Tailwind CSS + shadcn/ui | Consistent with local dashboard patterns, accessible components |
| Charts | Recharts | Reuse visualization patterns from local dashboard |
| Auth | Supabase Auth helpers for Next.js | Integrates with Supabase RLS and session management |

**Relationship to local dashboard**: The local dashboard (`frontend/dashboard/`) serves as **design inspiration** — chart patterns, color schemes, layout structure — but is not a direct port. Key differences:

| Aspect | Local dashboard | Cloud dashboard |
|--------|----------------|-----------------|
| Data source | SQLite via daemon HTTP API (localhost:7878) | Supabase Postgres via `@supabase/supabase-js` |
| Granularity | Per-message, per-session, hourly/daily | Daily only (per ADR-0083 trade-off) |
| Scope | Single developer's machine | Team-wide aggregation across devices |
| Auth | None (localhost only) | Supabase Auth (GitHub/Google/magic link) |
| Rendering | Client-side SPA (Vite) | Server-side + client-side (Next.js) |

### 6. Repository Ecosystem

After R4.4 extraction (issue #103), the ecosystem consists of five repos:

| Repo | Description | Tech | Release mechanism | Version scheme |
|------|-------------|------|-------------------|---------------|
| `siropkin/budi` | Core: daemon + CLI + proxy | Rust | GitHub Releases → platform binaries + Homebrew tap | semver (e.g., `8.0.0`) |
| `siropkin/budi-cursor` | VS Code / Cursor extension | TypeScript | GitHub Releases → VS Code Marketplace + Open VSX | semver (e.g., `1.0.0`) |
| `siropkin/budi-cloud` | Cloud dashboard + ingest API | Next.js + Supabase | Push to `main` → Vercel auto-deploy | No user-facing version (deployed service) |
| `siropkin/getbudi.dev` | Marketing website | Next.js or Astro | Push to `main` → Vercel auto-deploy | No user-facing version |
| `siropkin/homebrew-budi` | Homebrew tap formula | Ruby | Updated by `budi` release workflow | Tracks `budi` version |

**Version coordination**:
- `budi-cursor` declares `"compatible_daemon_version": ">=8.0.0"` in `package.json`. Extension checks daemon `api_version` on startup (per ADR-0086 §2).
- `budi-cloud` ingest API uses `schema_version` in the sync envelope (per ADR-0083 §2). Server returns 422 on version mismatch.
- No compile-time or link-time dependencies between repos. All communication is over HTTP or JSON file contracts.

**Repo descriptions (for GitHub)**:
- `budi` — "Local-first cost analytics for AI coding agents. Proxy, CLI, and daemon."
- `budi-cursor` — "Budi extension for VS Code and Cursor. Session health, cost tracking, status bar."
- `budi-cloud` — "Budi cloud dashboard and ingest API. Team-wide AI cost visibility."
- `getbudi.dev` — "Marketing website for Budi."
- `homebrew-budi` — "Homebrew tap for installing Budi."

### 7. Extension Publishing (budi-cursor)

Replicates the proven `siropkin/kursor-vscode` workflow:

**Trigger**: GitHub Release published (tag must match `v{version}` from `package.json`).

**Workflow steps**:
1. Checkout code
2. Setup Node.js 20, `npm ci`
3. Verify `package.json` version matches release tag
4. `npx @vscode/vsce package` (triggers esbuild production build)
5. `npx @vscode/vsce publish --pat $VSCE_PAT` (VS Code Marketplace)
6. `npx ovsx publish *.vsix --pat $OVSX_PAT` (Open VSX / Cursor)
7. Upload `.vsix` to GitHub Release

**Required secrets**:
- `VSCE_PAT` — Personal Access Token from Azure DevOps (Marketplace > Manage scope)
- `OVSX_PAT` — Personal Access Token from open-vsx.org
- `GITHUB_TOKEN` — Automatic (for release asset upload)

### 8. Daemon Lifecycle (Autostart on Boot)

After a machine reboot, the daemon must restart automatically. Without this, the proxy is dead and budi stops tracking.

| Platform | Mechanism | Service file | Scope |
|----------|-----------|-------------|-------|
| macOS | launchd LaunchAgent | `~/Library/LaunchAgents/dev.getbudi.budi-daemon.plist` | Current user (login trigger) |
| Linux | systemd user service | `~/.config/systemd/user/budi-daemon.service` | Current user (`systemctl --user`) |
| Windows | Task Scheduler | Created via `schtasks` at user logon | Current user |

**Installation**: `budi init` creates and loads the service file. The daemon starts at user login (not system boot — user-level service, not root/admin).

**Uninstallation**: `budi uninstall` removes the service file and stops the daemon.

**Health**: `budi doctor` checks whether the service is installed and whether the daemon is actually running.

### 9. Owner Prerequisites (Manual Setup)

Before R4 implementation can begin, the repository owner must complete these manual steps. They are ordered by when they block implementation.

#### Before #100 (Cloud Ingest API)

**Supabase setup**:
1. Create a Supabase account at [supabase.com](https://supabase.com)
2. Create project `budi-dev` (region: `us-east-1` recommended for low latency)
3. Create project `budi-prod` (same region)
4. From each project's **Settings → API**, note: Project URL, `anon` key, `service_role` key
5. Store keys securely (will be added as GitHub secrets when `budi-cloud` repo exists)

#### Before #102 (Cloud Dashboard)

**Vercel setup**:
1. Create a Vercel account at [vercel.com](https://vercel.com)
2. Link GitHub account (for automatic deployments)
3. Create project linked to `siropkin/budi-cloud` repo (after creation)
4. Configure environment variables: preview → dev Supabase keys, production → prod Supabase keys

**Domain setup**:
1. In Vercel: add custom domains `getbudi.dev`, `app.getbudi.dev`, `api.getbudi.dev`
2. In GoDaddy: update nameservers to Vercel's (displayed in Vercel domain settings), OR add CNAME records as Vercel instructs
3. Wait for DNS propagation and verify in Vercel

**Supabase Auth setup**:
1. In `budi-prod` project: Settings → Authentication → Providers
2. Enable **GitHub**: create OAuth App at github.com/settings/developers, add client ID + secret
3. Enable **Google**: create OAuth credentials in Google Cloud Console, add client ID + secret
4. Configure redirect URLs to `app.getbudi.dev`

#### Before #103 (Repo Extraction)

**Extension marketplace setup**:
1. Create Azure DevOps organization at dev.azure.com (if not already)
2. Generate PAT with `Marketplace > Manage` scope → this is `VSCE_PAT`
3. Create account at [open-vsx.org](https://open-vsx.org) → generate PAT → this is `OVSX_PAT`
4. Add both as secrets in `siropkin/budi-cursor` repo (after creation)

### 10. What This ADR Does NOT Decide

- Supabase or Vercel pricing tier (free tier for alpha; evaluate after measuring usage)
- Marketing site content, design, or copywriting
- Self-hosted cloud deployment option (post-8.0)
- Custom domain email (e.g., `hello@getbudi.dev`)
- CDN or edge caching strategy for the dashboard
- Monitoring/alerting for the cloud service (post-alpha)
- Billing/payment infrastructure for paid tiers

## Consequences

### Expected

- R4 implementers know the full technology stack and infrastructure before writing code.
- The owner has a clear, ordered checklist of manual setup steps.
- All repos, domains, secrets, and deployment targets are documented in one place.
- The extension publishing workflow is proven (reuses kursor-vscode pattern).

### Trade-offs

- **Vercel + Supabase lock-in**: Acceptable for a cloud alpha targeting 1-20 developers. Both have generous free tiers and export capabilities. Can be re-evaluated for scale.
- **Next.js is heavier than a Vite SPA**: Justified by server-side API routes (ingest endpoint), auth middleware, and SSR for SEO (marketing site and dashboard).
- **Five repos adds coordination overhead**: Mitigated by clear contracts (HTTP APIs, JSON schemas, `schema_version`, `compatible_daemon_version`). No compile-time dependencies between repos.
- **getbudi.dev as domain**: Clear and memorable. The `get` prefix is a common pattern for developer tools (getcomposer.org, getbootstrap.com). Alternative `budi.dev` may be taken or expensive.

### Downstream Impact

- **#100 (R4.1)**: Ingest API implemented as Next.js API routes in `budi-cloud`, targeting `api.getbudi.dev`. Supabase schema applied via migration files.
- **#101 (R4.2)**: Sync worker pushes to `https://api.getbudi.dev/v1/ingest` (updated from ADR-0083 §9 default).
- **#102 (R4.3)**: Dashboard deployed at `app.getbudi.dev`, uses Supabase Auth with GitHub + Google providers.
- **#103 (R4.4)**: Extraction creates `budi-cursor` and `budi-cloud` repos with independent CI and publishing.
- **#108 (R5.3)**: Release readiness includes daemon autostart across all platforms.
