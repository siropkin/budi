-- Budi Cloud Dashboard Schema Extension
-- Adds columns and tables needed for the web dashboard (issue #102).
-- Applied after 001_ingest_schema.sql.

-- ============================================================
-- 1. Extend users table for web-authenticated users
-- ============================================================

ALTER TABLE users ADD COLUMN IF NOT EXISTS display_name TEXT;
ALTER TABLE users ADD COLUMN IF NOT EXISTS email TEXT;

-- Make org_id nullable so a user can exist before creating/joining an org.
ALTER TABLE users ALTER COLUMN org_id DROP NOT NULL;

-- ============================================================
-- 2. Invite tokens for org onboarding
-- ============================================================

CREATE TABLE IF NOT EXISTS invite_tokens (
    id          TEXT PRIMARY KEY,                -- random token string
    org_id      TEXT NOT NULL REFERENCES orgs(id),
    role        TEXT NOT NULL DEFAULT 'member'
                CHECK (role IN ('member', 'manager')),
    created_by  TEXT NOT NULL REFERENCES users(id),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at  TIMESTAMPTZ NOT NULL,
    used_by     TEXT REFERENCES users(id),
    used_at     TIMESTAMPTZ
);

ALTER TABLE invite_tokens ENABLE ROW LEVEL SECURITY;

-- Managers can see invite tokens for their org
CREATE POLICY "Managers can read org invite tokens"
    ON invite_tokens FOR SELECT
    USING (
        org_id IN (
            SELECT org_id FROM users
            WHERE id = auth.uid()::text
              AND role = 'manager'
        )
    );

-- Managers can create invite tokens for their org
CREATE POLICY "Managers can create invite tokens"
    ON invite_tokens FOR INSERT
    WITH CHECK (
        org_id IN (
            SELECT org_id FROM users
            WHERE id = auth.uid()::text
              AND role = 'manager'
        )
    );
