# ADR-0083: Cloud Ingest, Identity, and Privacy Contract

- **Date**: 2026-04-10
- **Status**: Accepted (amended by [ADR-0091](./0091-model-pricing-manifest-source-of-truth.md) and [ADR-0094](./0094-custom-team-pricing-and-effective-cost-recalculation.md) — see banners)
- **Issue**: [#83](https://github.com/siropkin/budi/issues/83)
- **Milestone**: 8.0.0
- **Depends on**: [ADR-0082](./0082-proxy-compatibility-matrix-and-gateway-contract.md)

> **Amended by [ADR-0091](./0091-model-pricing-manifest-source-of-truth.md) (2026-04-21), §Neutral.** Budi's permitted outbound-network surface is extended by exactly one additional destination: an anonymous HTTPS `GET` to `https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json` issued by the daemon-side pricing refresher. The request carries no user content, no identifiers, and no headers beyond the standard `User-Agent` / `Accept` pair. Operator opt-out is `BUDI_PRICING_REFRESH=0`. The user-data privacy contract defined in §1 of this ADR is unchanged.

> **Amended by [ADR-0094](./0094-custom-team-pricing-and-effective-cost-recalculation.md) (2026-05-11), §Neutral.** The permitted outbound-network surface gains one additional destination: an authenticated HTTPS `GET` to `https://app.getbudi.dev/v1/pricing/active` (Bearer-authed with the same `budi_<key>` token as `POST /v1/ingest`). The request carries no user content; the response carries the calling org's negotiated price list (a small JSON document of rates) — not per-user data, no rollups, no token counts. Operator opt-out is the existing `BUDI_PRICING_REFRESH=0` env switch (one switch governs both the LiteLLM manifest refresh and the team-pricing pull). The user-data privacy contract defined in §1 of this ADR is unchanged.

## Context

Budi 8.0 adds an optional cloud layer so that engineering managers can see aggregated AI coding costs across a team without accessing individual developers' machines. Before any cloud implementation begins (R4), the data contract between the local daemon and the cloud ingest API must be locked — specifically what data leaves the machine, how identity and deduplication work, and what is explicitly forbidden.

### Local Data Model Today

The local SQLite database stores:

| Table | Contains | Sensitive? |
|-------|----------|------------|
| `messages` | Per-request records: tokens, cost, model, provider, repo, branch, timestamps | Token counts and cost are safe. `cwd`, `raw_json` (if present), session content are sensitive. |
| `sessions` | Session metadata: provider, email, workspace root, duration, git branch | `user_email`, `workspace_root`, `raw_json` are sensitive. |
| `message_rollups_hourly` | Hourly aggregates: token counts, cost, model, provider, repo, branch | **No sensitive data.** Pre-aggregated counts and costs only. |
| `message_rollups_daily` | Daily aggregates: same dimensions as hourly | **No sensitive data.** |
| `tags` | Key-value pairs on messages (user-defined) | Tag keys/values are user-controlled; could contain anything. |
The rollup tables are the natural sync unit. They contain pre-aggregated cost and usage metrics with no content, no prompts, no code, and no responses.

### Proxy Attribution (from ADR-0082)

The proxy captures per-request metadata including `model`, `input_tokens`, `output_tokens`, `duration_ms`, `repo`, `branch`, `ticket`, `provider`, and `status_code`. This metadata flows into `messages` and is aggregated into rollups. The cloud sync operates on the aggregated output, not the raw proxy events.

## Decision

### 1. Privacy Contract

**Absolute rule: prompts, code snippets, and model responses must never leave the local machine.**

This is not configurable. There is no "full upload" mode. The sync worker enforces this structurally by only reading from rollup tables and a curated projection of session/message metadata.

#### Never-Upload Fields

The following data categories are **permanently excluded** from the sync payload:

| Category | Examples | Reason |
|----------|----------|--------|
| Prompt content | Message `role: user` text, system prompts | Contains proprietary code and business logic |
| Model responses | Message `role: assistant` text, tool call arguments | Contains generated code and reasoning |
| Raw payloads | `raw_json` on messages, sessions | Unstructured dumps that may contain anything |
| File paths | `cwd`, `workspace_root` on messages/sessions | Reveals project structure and local file system layout |
| Email addresses | `user_email` on sessions | PII; identity is handled by API key, not email |
| Tool execution details | MCP server names, tool arguments, tool results | Contains code context and execution artifacts |
| Tag values | `tags.value` | User-defined; could contain anything sensitive |

#### Always-Upload Fields (the sync payload)

Only pre-aggregated, scrubbed metrics cross the wire:

| Field | Source | Why it's safe |
|-------|--------|---------------|
| Token counts (input, output, cache_creation, cache_read) | Rollups | Numeric counts, no content |
| Cost (cents) | Rollups | Derived from token counts and pricing |
| Model name | Rollups | Public model identifier (e.g., `claude-sonnet-4-20250514`) |
| Provider | Rollups | Public provider name (e.g., `anthropic`, `openai`) |
| Repo identifier | Rollups | Hashed repo root, not the path (see Identity section) |
| Git branch | Rollups | Branch name (contains ticket IDs, not code) |
| Message count | Rollups | Numeric count per bucket |
| Time bucket | Rollups | Hour or day granularity |
| Session count | Derived | Number of distinct sessions per period |
| Ticket identifier | Derived from branch | Extracted ticket ID (e.g., `PROJ-1234`) |

**Repo identifier handling**: The `repo_id` stored locally is already a hash of the repo root path (see `crates/budi-core/src/repo_id.rs`). The sync payload uses this hash directly. The actual file system path never appears in the sync payload.

### 2. Sync Payload Shape

The daemon syncs **daily rollup records** to the cloud. Daily granularity balances usefulness (managers need daily/weekly/monthly views) with privacy (hourly data can reveal individual work patterns too precisely).

#### Sync Envelope

```json
{
  "schema_version": 1,
  "device_id": "d_abc123def456",
  "org_id": "org_xyz789",
  "synced_at": "2026-04-10T18:30:00Z",
  "payload": {
    "daily_rollups": [ ... ],
    "session_summaries": [ ... ]
  }
}
```

#### Daily Rollup Record

One record per unique `(bucket_day, role, provider, model, repo_id, git_branch)` tuple:

```json
{
  "bucket_day": "2026-04-10",
  "role": "assistant",
  "provider": "anthropic",
  "model": "claude-sonnet-4-20250514",
  "repo_id": "sha256:a1b2c3d4e5f6",
  "git_branch": "feature/PROJ-1234-add-auth",
  "ticket": "PROJ-1234",
  "ticket_source": "branch",
  "message_count": 47,
  "input_tokens": 125000,
  "output_tokens": 89000,
  "cache_creation_tokens": 15000,
  "cache_read_tokens": 42000,
  "cost_cents": 3.42
}
```

`ticket_source` is optional and only set when `ticket` is present. Its
value matches the `ticket_source` sibling tag in the local analytics
pipeline (`branch` for the alphanumeric `<PREFIX>-<NUM>` pattern,
`branch_numeric` for the ADR-0082 §9 numeric fallback). Cloud
dashboards that surface a provenance marker alongside the ticket
render the same bucketing local `budi stats --tickets` does. Absent
values round-trip as `NULL` server-side (8.2.1, #333).

#### Session Summary Record

A scrubbed per-session summary (no content, no paths):

```json
{
  "session_id": "d99dfe22-d05c-4c78-8698-015d06e5dabb",
  "provider": "claude_code",
  "started_at": "2026-04-10T09:15:00Z",
  "ended_at": "2026-04-10T10:45:00Z",
  "duration_ms": 5400000,
  "repo_id": "sha256:a1b2c3d4e5f6",
  "git_branch": "feature/PROJ-1234-add-auth",
  "ticket": "PROJ-1234",
  "ticket_source": "branch",
  "message_count": 47,
  "total_input_tokens": 125000,
  "total_output_tokens": 89000,
  "total_cost_cents": 3.42,
  "primary_model": "claude-sonnet-4-20250514"
}
```

Session summaries are derived by aggregating messages per session. They include only computed totals, never per-message detail.

`primary_model` is optional (8.3.19, #638). It records the model that
consumed the largest share of `input + output` tokens for the session,
with ties broken by the latest message timestamp. Sessions with zero
scored messages omit the field entirely so the cloud column stays
`NULL` rather than being guessed (paired with budi-cloud#140's
`session_summaries.main_model` column).

### 3. Identity Model

#### Org

An **org** is the billing and visibility boundary. All devices in an org contribute to the same dashboard.

| Property | Value |
|----------|-------|
| ID format | `org_<alphanumeric>` (server-generated) |
| Creation | Manager creates org in cloud dashboard; receives org ID |
| Membership | Devices join an org via an invite token or org ID + API key pairing |
| Visibility | Manager sees aggregated data for all devices in the org |

#### User

A **user** is a cloud account that authenticates API requests.

| Property | Value |
|----------|-------|
| ID format | `usr_<alphanumeric>` (server-generated) |
| Creation | Self-registration or manager invite |
| Auth | API key (`budi_<alphanumeric>`) stored locally in `~/.config/budi/cloud.toml` |
| Role | `member` (default) or `manager`. Members sync data; managers view dashboards. |

#### Device

A **device** is a single budi installation (one machine, one daemon).

| Property | Value |
|----------|-------|
| ID format | `dev_<alphanumeric>` |
| Creation | Generated locally on first `budi cloud login` or `budi cloud init`. Stored in `~/.config/budi/cloud.toml`. |
| Stability | Persists across daemon restarts. Regenerated only if the config file is deleted. |
| Uniqueness | Globally unique (UUID-based). One device per machine per budi home directory. |

#### Relationship

```
Org (1) ──< User (many) ──< Device (many)
```

A user can have multiple devices (laptop, desktop). A device belongs to exactly one user. A user belongs to exactly one org. Cross-org data sharing is not supported in v1.

### 4. Authentication and Trust Model

#### Daemon → Cloud Ingest

| Aspect | Decision |
|--------|----------|
| Transport | HTTPS only. The daemon refuses to sync over plain HTTP (hard-coded check). |
| Auth method | API key in `Authorization: Bearer budi_<key>` header. |
| Key storage | `~/.config/budi/cloud.toml` with file permissions `0600`. |
| Key scope | One key per user. The key identifies both the user and (transitively) the org. |
| Key rotation | User can regenerate the key in the cloud dashboard. Old key is immediately invalid. |
| Rate limiting | Server-side. The daemon backs off on 429 responses with exponential retry (1s → 2s → 4s → ... → 5min cap). |

#### Cloud Ingest → Daemon

The cloud never initiates connections to the daemon. All sync is push-only, initiated by the local daemon. There is no webhook, no pull, no remote command channel.

#### Trust Boundary

| Trust level | What the cloud sees | What the cloud does NOT see |
|-------------|--------------------|-----------------------------|
| Full | Aggregated cost/usage metrics, session summaries (counts and durations), model names, provider names, hashed repo IDs, branch names, ticket IDs | Prompts, responses, code, file paths, email addresses, raw payloads, tag values, tool arguments, MCP traffic |

### 5. Idempotency and Deduplication

The sync worker must handle retries, network failures, and overlapping sync windows without creating duplicate records on the server.

#### Idempotency Key

Each sync payload includes a deterministic **idempotency key** derived from its content:

| Record type | Idempotency key | Dedup behavior |
|-------------|----------------|----------------|
| Daily rollup | `(device_id, bucket_day, role, provider, model, repo_id, git_branch)` | UPSERT — later values overwrite earlier ones for the same key. This handles the case where a rollup for "today" is synced multiple times as new data arrives. |
| Session summary | `(device_id, session_id)` | UPSERT — session end time, duration, and totals may update as the session progresses. |

#### Sync Watermark

The daemon tracks what has been successfully synced using a local watermark:

| Field | Purpose |
|-------|---------|
| `cloud_sync_watermark` | ISO 8601 timestamp of the latest `bucket_day` that has been fully synced (all rollups for that day confirmed by server). |
| Storage | `sync_state` table, key `__budi_cloud_sync__`. Reuses the existing sync tracking infrastructure. |

On each sync tick, the daemon sends:
1. All daily rollups where `bucket_day > watermark` (new days).
2. The current day's rollups (always re-sent, since they may have grown).
3. Session summaries for sessions that started or ended since the last sync.

#### Server Response

The ingest API returns a confirmation that includes the server-side watermark:

```json
{
  "accepted": true,
  "watermark": "2026-04-09",
  "records_upserted": 12
}
```

The daemon updates its local watermark to match the server-confirmed value. If the server rejects the payload (schema mismatch, auth failure), the watermark is not advanced.

### 6. Minimum Viable Team Model (Cloud Alpha)

The cloud alpha (R4) supports one org size: **small team** (1–20 developers).

| Aspect | Decision |
|--------|----------|
| Org creation | Manager signs up, creates org, gets an invite link. |
| Member onboarding | Developer runs `budi cloud join <invite-token>`, which registers the device and stores the API key locally. |
| Roles | `manager` (view dashboard, manage members, create/revoke invite links) and `member` (sync data, view own data). |
| Dashboard access | Manager sees aggregated cost data across the org: by day, by user/device, by repo, by model, by ticket. Member sees only their own data. |
| Data granularity | Dashboard shows daily granularity for cost aggregations. Session summaries provide per-session metadata (start/end time, duration, totals) but no per-message detail. No per-hour or real-time streaming views in v1. |
| Retention | Cloud retains synced data for 90 days in v1. Configurable in later versions. |
| Multi-org | Not supported in v1. A user belongs to exactly one org. |
| SSO / SAML | Not supported in v1. API key auth only. |

### 7. Cloud Ingest API Surface (v1)

The ingest API is the only cloud endpoint the daemon talks to. The full cloud API (dashboard, user management) is separate and not defined here.

| Endpoint | Method | Purpose |
|----------|--------|---------|
| `POST /v1/ingest` | POST | Receive sync payload from daemon |
| `GET /v1/ingest/status` | GET | Return current watermark and sync health for the authenticated device |

#### `POST /v1/ingest` Contract

| Aspect | Contract |
|--------|----------|
| Auth | `Authorization: Bearer budi_<key>` |
| Content-Type | `application/json` |
| Body | Sync envelope (see section 2) |
| Max body size | 1 MiB (daily rollups for a single developer are typically < 10 KiB) |
| Success response | `200 OK` with confirmation JSON |
| Schema mismatch | `422 Unprocessable Entity` — daemon should log warning and not retry until updated |
| Auth failure | `401 Unauthorized` — daemon should stop syncing and prompt user to re-authenticate |
| Rate limited | `429 Too Many Requests` — daemon backs off with exponential retry |
| Server error | `5xx` — daemon retries with exponential backoff (1s → 5min cap) |

### 8. Supabase Schema (Ingest Tables)

The cloud alpha uses Supabase (Postgres + auth). The ingest tables mirror the sync payload:

```sql
-- Orgs
CREATE TABLE orgs (
    id          TEXT PRIMARY KEY,   -- org_<alphanumeric>
    name        TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Users
CREATE TABLE users (
    id          TEXT PRIMARY KEY,   -- usr_<alphanumeric>
    org_id      TEXT NOT NULL REFERENCES orgs(id),
    role        TEXT NOT NULL DEFAULT 'member' CHECK (role IN ('member', 'manager')),
    api_key     TEXT UNIQUE NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Devices
CREATE TABLE devices (
    id          TEXT PRIMARY KEY,   -- dev_<alphanumeric>
    user_id     TEXT NOT NULL REFERENCES users(id),
    label       TEXT,               -- optional friendly name
    first_seen  TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen   TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Daily rollups (synced from local daemon)
CREATE TABLE daily_rollups (
    device_id              TEXT NOT NULL REFERENCES devices(id),
    bucket_day             DATE NOT NULL,
    role                   TEXT NOT NULL,
    provider               TEXT NOT NULL,
    model                  TEXT NOT NULL,
    repo_id                TEXT NOT NULL,
    git_branch             TEXT NOT NULL,
    ticket                 TEXT,
    ticket_source          TEXT,               -- 'branch' | 'branch_numeric' | NULL
    message_count          INTEGER NOT NULL DEFAULT 0,
    input_tokens           BIGINT NOT NULL DEFAULT 0,
    output_tokens          BIGINT NOT NULL DEFAULT 0,
    cache_creation_tokens  BIGINT NOT NULL DEFAULT 0,
    cache_read_tokens      BIGINT NOT NULL DEFAULT 0,
    cost_cents             NUMERIC(12,4) NOT NULL DEFAULT 0,
    synced_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (device_id, bucket_day, role, provider, model, repo_id, git_branch)
);

-- Session summaries (synced from local daemon)
CREATE TABLE session_summaries (
    device_id            TEXT NOT NULL REFERENCES devices(id),
    session_id           TEXT NOT NULL,
    provider             TEXT NOT NULL,
    started_at           TIMESTAMPTZ,
    ended_at             TIMESTAMPTZ,
    duration_ms          BIGINT,
    repo_id              TEXT,
    git_branch           TEXT,
    ticket               TEXT,
    ticket_source        TEXT,                 -- 'branch' | 'branch_numeric' | NULL
    message_count        INTEGER NOT NULL DEFAULT 0,
    total_input_tokens   BIGINT NOT NULL DEFAULT 0,
    total_output_tokens  BIGINT NOT NULL DEFAULT 0,
    total_cost_cents     NUMERIC(12,4) NOT NULL DEFAULT 0,
    synced_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (device_id, session_id)
);

-- Row-level security: users see only their org's data
ALTER TABLE daily_rollups ENABLE ROW LEVEL SECURITY;
ALTER TABLE session_summaries ENABLE ROW LEVEL SECURITY;
```

Row-level security policies are defined during R4 implementation. The schema here defines the table structure; RLS rules depend on Supabase auth integration details.

### 9. Config Surface

New fields in `~/.config/budi/cloud.toml` (created by `budi cloud join` or `budi cloud init`):

```toml
[cloud]
enabled = false
api_key = "budi_..."
device_id = "dev_..."
org_id = "org_..."
endpoint = "https://app.getbudi.dev"

[cloud.sync]
interval_seconds = 300    # 5 minutes
retry_max_seconds = 300   # 5 minute backoff cap
```

New env vars:

| Var | Purpose |
|-----|---------|
| `BUDI_CLOUD_ENABLED` | `true`/`false` override |
| `BUDI_CLOUD_API_KEY` | Override API key (useful for CI) |
| `BUDI_CLOUD_ENDPOINT` | Override cloud endpoint (useful for self-hosted) |

**Cloud sync is disabled by default.** It must be explicitly enabled via `budi cloud join` or `budi cloud init`. There is no automatic opt-in, no telemetry, no "phone home" behavior when cloud is not configured.

### 10. What This ADR Does NOT Decide

- Supabase RLS policy details → decided during R4.1 implementation.
- Cloud dashboard UI/UX → decided in R4.3.
- Invite link generation and flow → decided in R4.1.
- Self-hosted cloud deployment → post-8.0 scope.
- Per-message cloud sync (explicitly excluded — aggregates only).
- Real-time streaming to cloud (explicitly excluded — batch sync only).

## Consequences

### Expected

- R4 implementation issues have a locked data contract, identity model, and API surface.
- The privacy boundary is unambiguous: only aggregated metrics, never content.
- The sync payload is small (< 10 KiB/day for typical usage), making the sync worker simple and cheap.
- Idempotent UPSERT semantics mean the sync worker can retry freely without data corruption.

### Trade-offs

- **Daily granularity for cost aggregations.** Managers cannot see real-time or hourly cost breakdowns in the cloud dashboard. Session summaries (§2) provide per-session metadata (timestamps, duration, totals) but no per-message detail — this is sub-daily but not content-revealing. Hourly cost data remains available locally via the Rich CLI.
- **No per-message visibility in the cloud.** The cloud sees totals per day per model per branch, not individual requests. This limits debugging capability but preserves the privacy contract.
- **No tag sync.** Tag values are user-defined and could contain sensitive information. Tags remain local-only. Managers can see cost by model/repo/branch/ticket but not by custom tag.
- **Single org per user.** Consultants working for multiple clients cannot aggregate across orgs. Multi-org support is a post-8.0 concern.
- **API key auth only.** No SSO, no SAML, no OAuth. Acceptable for cloud alpha with small teams. Enterprise auth is a post-8.0 concern.
- **No cloud-to-daemon communication.** The manager cannot push budget limits or config changes to developer machines via the cloud. All configuration is local. This preserves the local-first design but means budget enforcement (R5) is locally configured, not centrally managed.

### Downstream Impact

- **R4.1** (cloud ingest API): Implements the `POST /v1/ingest` and `GET /v1/ingest/status` endpoints per this contract. Creates the Supabase schema.
- **R4.2** (cloud sync worker): Builds the daemon-side sync loop that reads rollups, builds the sync envelope, and pushes to the ingest API. Uses the watermark and retry logic defined here.
- **R4.3** (manager dashboard): Queries the Supabase tables defined here. Dashboard views are bounded by the data available in `daily_rollups` and `session_summaries`.
- **R4.4** (repo extraction): The `cloud.toml` config and sync worker code are extracted to the budi-cloud repo.
- **R5.1** (budget engine): Budget thresholds are evaluated locally using local rollup data. The cloud provides visibility but not enforcement.

---

*Last verified against code on 2026-05-14.*
