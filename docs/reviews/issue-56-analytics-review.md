# Issue #56 Review: Analytics Storage, Queries, and Session Health

## Findings (highest severity first)

### P1 (fixed): Auto-selected session health could target stale sessions
- Area: `crates/budi-core/src/analytics/health.rs`
- Risk: `session_health(None)` selected by `sessions.started_at` only. Sessions created from message ingestion can have `started_at = NULL`, so an older session with metadata could win even when another session has newer assistant activity.
- Impact: `budi health` and consumers that rely on default session resolution (no explicit session ID) could report health for the wrong session.
- Fix in this PR: prefer latest assistant activity (`messages.role = 'assistant'`) when auto-selecting the session, with `sessions.started_at/ended_at` used as fallback ordering.

## Missing/weak tests identified

- Added: `health_auto_select_prefers_recent_assistant_activity_when_started_at_missing` in `crates/budi-core/src/analytics/tests.rs`.
- Coverage added: verifies that a session with recent assistant messages is selected over a newer `started_at`-only session when the active session metadata is incomplete.

## Documentation drift fixed

- Updated `README.md` session-health wording to reflect how default session selection works.
- Updated `SOUL.md` session-health architecture note with the same behavior.

## Follow-up candidates (not in this PR)

- Add an explicit `session_health(None)` fallback path for corrupted DB states where `messages.session_id` exists without a `sessions` row.
- Add `EXPLAIN QUERY PLAN`-style regression checks for the session list/detail aggregations on large datasets to protect index assumptions as queries evolve.
