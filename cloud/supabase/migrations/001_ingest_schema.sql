-- Budi Cloud Ingest Schema
-- Per ADR-0083 §8: Supabase Schema (Ingest Tables)
-- Applied via Supabase CLI or dashboard migration runner.

-- ============================================================
-- 1. Core identity tables
-- ============================================================

-- Orgs: billing and visibility boundary
CREATE TABLE orgs (
    id          TEXT PRIMARY KEY,          -- org_<alphanumeric>
    name        TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Users: cloud accounts that authenticate API requests
CREATE TABLE users (
    id          TEXT PRIMARY KEY,          -- usr_<alphanumeric>
    org_id      TEXT NOT NULL REFERENCES orgs(id),
    role        TEXT NOT NULL DEFAULT 'member'
                CHECK (role IN ('member', 'manager')),
    api_key     TEXT UNIQUE NOT NULL,      -- budi_<alphanumeric>
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Devices: one budi daemon installation per machine
CREATE TABLE devices (
    id          TEXT PRIMARY KEY,          -- dev_<alphanumeric>
    user_id     TEXT NOT NULL REFERENCES users(id),
    label       TEXT,                      -- optional friendly name
    first_seen  TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen   TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- ============================================================
-- 2. Synced data tables
-- ============================================================

-- Daily rollups synced from local daemon.
-- Composite PK enforces UPSERT idempotency per ADR-0083 §5.
CREATE TABLE daily_rollups (
    device_id              TEXT NOT NULL REFERENCES devices(id),
    bucket_day             DATE NOT NULL,
    role                   TEXT NOT NULL,
    provider               TEXT NOT NULL,
    model                  TEXT NOT NULL,
    repo_id                TEXT NOT NULL,
    git_branch             TEXT NOT NULL,
    ticket                 TEXT,
    message_count          INTEGER NOT NULL DEFAULT 0,
    input_tokens           BIGINT NOT NULL DEFAULT 0,
    output_tokens          BIGINT NOT NULL DEFAULT 0,
    cache_creation_tokens  BIGINT NOT NULL DEFAULT 0,
    cache_read_tokens      BIGINT NOT NULL DEFAULT 0,
    cost_cents             NUMERIC(12,4) NOT NULL DEFAULT 0,
    synced_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (device_id, bucket_day, role, provider, model, repo_id, git_branch)
);

-- Session summaries synced from local daemon.
-- Composite PK enforces UPSERT idempotency per ADR-0083 §5.
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
    message_count        INTEGER NOT NULL DEFAULT 0,
    total_input_tokens   BIGINT NOT NULL DEFAULT 0,
    total_output_tokens  BIGINT NOT NULL DEFAULT 0,
    total_cost_cents     NUMERIC(12,4) NOT NULL DEFAULT 0,
    synced_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (device_id, session_id)
);

-- ============================================================
-- 3. Indexes for common dashboard queries
-- ============================================================

-- Dashboard queries by org + date range
CREATE INDEX idx_daily_rollups_bucket_day
    ON daily_rollups (bucket_day);

CREATE INDEX idx_session_summaries_started_at
    ON session_summaries (started_at);

-- ============================================================
-- 4. Row-Level Security (RLS)
-- ============================================================
-- RLS ensures users can only see data belonging to their org.
-- The ingest API uses the service_role key (bypasses RLS).
-- Dashboard queries use the anon key + Supabase Auth JWT,
-- which triggers these policies.

ALTER TABLE daily_rollups ENABLE ROW LEVEL SECURITY;
ALTER TABLE session_summaries ENABLE ROW LEVEL SECURITY;
ALTER TABLE devices ENABLE ROW LEVEL SECURITY;
ALTER TABLE users ENABLE ROW LEVEL SECURITY;
ALTER TABLE orgs ENABLE ROW LEVEL SECURITY;

-- Org members can read their own org
CREATE POLICY "Users can read own org"
    ON orgs FOR SELECT
    USING (
        id IN (
            SELECT org_id FROM users
            WHERE id = auth.uid()::text
        )
    );

-- Users can read other users in their org
CREATE POLICY "Users can read org members"
    ON users FOR SELECT
    USING (
        org_id IN (
            SELECT org_id FROM users
            WHERE id = auth.uid()::text
        )
    );

-- Users can read devices belonging to their org's users
CREATE POLICY "Users can read org devices"
    ON devices FOR SELECT
    USING (
        user_id IN (
            SELECT id FROM users
            WHERE org_id IN (
                SELECT org_id FROM users
                WHERE id = auth.uid()::text
            )
        )
    );

-- Users can read daily rollups from devices in their org
CREATE POLICY "Users can read org daily_rollups"
    ON daily_rollups FOR SELECT
    USING (
        device_id IN (
            SELECT d.id FROM devices d
            JOIN users u ON d.user_id = u.id
            WHERE u.org_id IN (
                SELECT org_id FROM users
                WHERE id = auth.uid()::text
            )
        )
    );

-- Users can read session summaries from devices in their org
CREATE POLICY "Users can read org session_summaries"
    ON session_summaries FOR SELECT
    USING (
        device_id IN (
            SELECT d.id FROM devices d
            JOIN users u ON d.user_id = u.id
            WHERE u.org_id IN (
                SELECT org_id FROM users
                WHERE id = auth.uid()::text
            )
        )
    );
