# `cursor` provider — 8.5.2 review pass

- **Module**: `crates/budi-core/src/providers/cursor.rs` (3,128 LOC; prod 2,284 / tests 843)
- **Tracking**: #799
- **ADRs**: [0089](../../adr/0089-reverse-proxy-first-jsonl-tailing-as-sole-live-path.md), [0090](../../adr/0090-cursor-usage-api-contract.md)

## Shape

The provider with the most ingestion paths — three:

1. **`cursorDiskKV` bubbles** (preferred, #553) — exact per-message
   tokens + model from the local `state.vscdb`, no network call.
2. **Cursor Usage API** (`/api/dashboard/get-filtered-usage-events`) —
   supplementary signal for overage attribution, exact `cost_cents`.
3. **JSONL transcripts** under `~/.cursor/projects/*/agent-transcripts/`
   — fallback for older sessions and machines without auth.

Plus a hefty "session repair" suite (`run_cursor_repairs`,
`repair_cursor_workspace_metadata`, `backfill_cursor_session_ids`) that
patches historical rows in SQL.

## Adherence to ADRs

- **ADR-0089 §1**: ✓ — `watch_roots` returns the JSONL projects dir
  only. State.vscdb and the Usage API are pull-mode via `sync_direct`.
- **ADR-0089 §7** (Usage API pull-only, not a watch root): ✓ —
  `combine_cursor_sync_results` returns `None` from `sync_direct`
  when both arms are unavailable so the file fallback still runs.
- **ADR-0090** (Usage API contract): ✓ — JWT extraction
  (`extract_cursor_auth`), pagination with watermark
  (`fetch_usage_events`, `paginate_all` logic), pricing-source tagging
  (`pricing::COLUMN_VALUE_UPSTREAM_API`).
- **ADR-0090 §2026-04-23 amendment** (bubbles primary, API
  supplementary): ✓ — `sync_from_bubbles` runs first, `sync_from_usage_api`
  runs after; combiner merges.

## Observations

### Where the discipline shines

- **`CursorAuthIssue` enum + `reason_tag()` + `human_message()`**
  (cursor.rs:410-488). Best-in-class operator diagnostics in the tree.
  Seven typed reasons each map to (a) a stable wire-contract tag for
  log grepping, (b) a human-readable message that names the fix. Pinned
  by a test that the reason tags don't drift (cursor.rs:2296-).
  **This is the template the other providers should follow** for any
  "configured but not working" failure-mode surface.
- **Watermark separation**: `cursor-bubbles` and `cursor-api-usage` are
  intentionally distinct `sync_state` keys (cursor.rs:42-53). Both
  paths advance independently; correctly justified inline.
- **JWT exp claim heuristic** (cursor.rs:378-392): `> 1_700_000_000_000`
  to distinguish ms from s. Magic but defensible (10^12 ≈ Nov 2023 in
  ms vs year 56,000 in s). Worth a date comment so the next reader
  doesn't have to derive it.

### Worth fixing (defer follow-up tickets)

1. **No `MIN_API_VERSION` on the Usage API or bubble shapes.** Both
   are documented in ADR-0090, but neither has the `MIN_API_VERSION` /
   `*_unknown_shape` guardrails copilot_chat ships. The bubble path
   does have `warn_bubble_schema_once` (cursor.rs:1231) — but that's a
   single bit ("we warned once for this process"), not a per-shape dedup.
   If Cursor renames `tokenCount.inputTokens` to `tokens.input` in a
   future release, the SQL would emit zero rows and the warn would
   fire once and never again. Worth tightening to the
   `(file_path, shape_signature)`-style dedup copilot_chat uses.
2. **Silent `let _ = conn.execute(...)`** sprinkled through the SQL
   repair code (cursor.rs:1692, 1725, 1793, 1803). The intent is "this
   is best-effort, don't fail the sync" — fair — but a single
   `tracing::debug!` on the error would let operators trace why a
   particular row didn't repair. Worth a one-line per-site fix.
3. **`repair_cursor_workspace_metadata` does sequential prepared
   statements without a per-old_cwd transaction wrapper**
   (cursor.rs:1768-1812). If the second `conn.execute` fails after the
   first succeeds, the row lands in a half-repaired state. The outer
   loop already iterates per `old_cwd`; wrapping each iteration in a
   transaction makes each row atomic without changing the per-row
   semantics. Small ticket.
4. **`base64url_decode` is hand-rolled** (cursor.rs:247-295) with a
   compile-time lookup table. Works fine; if any other path in the
   tree later needs JWT parsing, it deserves to live in
   `crate::util::base64`.
5. **Three-path provider with three ingestion code paths is the most
   complex in the tree** — the comment block at the top of the module
   gives a one-line summary, but a state-flow diagram (or a
   pretty-printed call graph in this review's successor) would help
   future readers. Worth tracking against #800.

### Minor

- `deterministic_cursor_message_uuid` (cursor.rs:952) is the most
  RFC-4122-compliant UUID formatter in the providers/ tree (sets the
  v4/variant bits). The other providers' deterministic-UUID helpers do
  not. If the shared helper proposed in #800 lands, this implementation
  is the one to keep.
- `find_matching_session` uses a `±5s` clock-skew window
  (`CLOCK_SKEW_MS`, cursor.rs:731). Documented; reasonable.
- `bubble_to_parsed_message` (cursor.rs:1416) leaves `cwd` / `git_branch`
  / `repo_id` as `None` and patches them later via
  `attach_session_context_to_bubbles`. The dance is correct but
  indirect; a comment at the `None` site explaining "filled by
  attach_session_context_to_bubbles after composer-header merge" would
  save future debugging.
- `cost_confidence: String::new()` (cursor.rs:1533) — empty string as
  a sentinel that the `CostEnricher` interprets later. Three valid
  values plus a sentinel; an enum on `ParsedMessage` would be cleaner.
- `surface: Some(crate::surface::CURSOR.to_string())` repeated four
  times — same `to_string()`-of-a-const pattern as the other providers.

### No issues found

- ADR-0090 path-discovery cross-product correct (macOS / Linux /
  Windows variants).
- Composer-header merge logic (`load_composer_header_contexts` →
  `load_session_contexts`) correctly prefers local Cursor windows over
  hook-derived timestamps. Reasoning is justified inline.
- `total_cents` validation in `parse_usage_event` (cursor.rs:565-586)
  has thoughtful clamping for negative / >$1000 / >$50 values, with
  tracing warnings rather than silent drops.

## Concrete follow-up

- **`MIN_API_VERSION` + `(path, shape)` warn-once for bubbles** —
  small ticket, paired with an ADR-0090 amendment if the bubble shape
  ever shifts.
- **Surface SQL repair errors at `debug` level** — one-line per site.
- **Transaction-per-row for `repair_cursor_workspace_metadata`** — small
  refactor.
- **Document the three-path flow** — track in #800 or a dedicated
  cursor walkthrough doc.

No 8.5.2-scoped behavior change required. The operator-diagnostics
quality on the auth path is a model to copy elsewhere.
