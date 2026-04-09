# Issue #58 Review: Enrichment Pipeline, Tagging, and Cost Attribution

## Findings (highest severity first)

### P1 (fixed): `activity` attribution could become stale after the first classified prompt in a session
- Area: `crates/budi-core/src/pipeline/mod.rs` (`propagate_session_context`)
- Risk: `prompt_category` propagation only latched the first non-empty category per session. Later user turns with a new category (for example `bugfix` -> `feature`) were ignored, so downstream assistant messages kept the old `activity` tag.
- Concrete example: `u1(prompt_category=bugfix) -> a1 -> u2(prompt_category=feature) -> a2`. Before fix, `a2` was still tagged `activity=bugfix`.
- Fix in this PR: update propagation to keep the latest non-empty `prompt_category`, matching the documented "most recent preceding message" semantics.

## Invariant assumptions to encode (tests/docs)

1. Enricher order is semantic and must stay: `Hook -> Identity -> Git -> Tool -> Cost -> Tag`.
2. Context fields (`git_branch`, `repo_id`, `cwd`, `prompt_category`) propagate from the most recent prior message in-session.
3. Identity tags are deduplicated per session; context tags can legitimately vary across turns.
4. `cost_cents.is_some()` on assistant messages requires a non-empty `cost_confidence`.
5. `repo_id` and `git_branch` remain canonical columns (not duplicated as tags) for query stability.

## Test coverage added

- `activity_tag_tracks_latest_prompt_category` in `crates/budi-core/src/pipeline/mod.rs`
  - Verifies that when prompt classification changes mid-session, subsequent assistant turns get the updated `activity` tag.

## Documentation drift corrected

- `SOUL.md`
  - Corrected pipeline order to include `ToolEnricher`.
  - Updated enricher count from 5 -> 6.
  - Clarified auto/conditional tag examples (`tool_use_id`, `cost_confidence`, `speed`).
- `CONTRIBUTING.md`
  - Fixed registration reference from `Pipeline::new()` -> `Pipeline::default_pipeline()`.
  - Corrected enricher order to include `ToolEnricher`.
- `README.md`
  - Clarified assistant-tag semantics and conditional tags (`cost_confidence`, `speed`).
  - Added `tool_use_id` to documented core attribution tags.

## Follow-up candidates (not in this PR)

- Add an integration test that asserts `HookEnricher` session metadata and JSONL-derived context merge deterministically when both are present and disagree.
- Add explicit coverage for `repo_id = "unknown"` repair paths to ensure tag-rule matching is not degraded by stale placeholder values.
- Add a pipeline-order regression test that fails loudly if `ToolEnricher` or `CostEnricher` move relative to `TagEnricher`.
