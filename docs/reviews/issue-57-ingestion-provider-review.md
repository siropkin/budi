# Issue #57 Review: Ingestion Sources and Provider Adapters

## Findings (highest severity first)

### P1 (fixed): Cursor API events with numeric timestamps were silently dropped, and quick-sync pagination could stop early
- Area: `crates/budi-core/src/providers/cursor.rs`
- Risk: `parse_usage_event` and watermark checks only accepted string `timestamp`, so numeric `timestamp` values were treated as invalid. In quick-sync mode this can also incorrectly mark a page as "all below watermark", stopping pagination early.
- Reproduction note: feed `fetch_usage_events_with_page_loader(Some(1000), false, ...)` pages whose events use numeric timestamps (`"timestamp": 1200` as JSON number). Before fix, parsing produced no new events and pagination could terminate after page 1.
- Fix in this PR: accept both string and numeric timestamp encodings in one shared parser (`parse_timestamp_ms`) used by both event parsing and watermark/page-stop checks.

### P2 (fixed): Cursor state DB discovery missed Windows user-data layout
- Area: `crates/budi-core/src/providers/cursor.rs`
- Risk: provider discovery and auth extraction only checked macOS/Linux paths. On Windows, `%APPDATA%\\Cursor\\User\\...` is a primary location; missing it can disable Cursor API ingestion and force degraded fallback behavior.
- Reproduction note: install Cursor on Windows with only `%APPDATA%\\Cursor\\User\\globalStorage\\state.vscdb` present. Prior lookup logic does not discover this DB.
- Fix in this PR: unify user-state root discovery to include macOS/Linux and Windows (`%APPDATA%` + `~/AppData/Roaming` fallback), and de-duplicate discovered paths.

## Risky edge cases reviewed

- Malformed queued payloads are retried with backoff and eventually dead-lettered after `MAX_ATTEMPTS` (covered by existing queue tests).
- OTEL↔JSONL dedup has explicit ambiguity guards; ambiguous matches avoid unsafe merges and log warnings.
- Cursor API outages and expired auth degrade to transcript fallback instead of hard failure.

## Test coverage added

- `parse_usage_event_accepts_numeric_timestamp`
- `quick_sync_handles_numeric_timestamps`
- `cursor_user_state_roots_include_windows_variants_without_duplicates`

## Documentation drift fixed

- Updated `README.md` Cursor support summary to mention transcript fallback behavior.
- Updated architecture text in `README.md` to describe Cursor source-of-truth fallback path.
- Updated `SOUL.md` Cursor provider description to reflect cross-platform state DB lookup and fallback semantics.

## Follow-up candidates (not in this PR)

- Emit an explicit integration-health warning when Cursor auth lookup fails due unreadable/locked `state.vscdb`, not just silent fallback.
- Add coverage for mixed timestamp encodings within the same Cursor API page (string + number + malformed) to assert ordering and watermark behavior remain stable.
- Consider broadening transcript discovery depth (currently shallow under `agent-transcripts`) for non-standard Cursor storage layouts.
