# Coverage baseline — 8.5.2

Captured by `cargo-llvm-cov` (v0.8.7) against `cargo test --workspace --locked --test-threads=1`. Tracks issue #804.

- **Tool**: `cargo-llvm-cov` (LLVM source-based instrumentation).
- **Command**: `scripts/coverage.sh` → `cargo llvm-cov --workspace --summary-only -- --test-threads=1`.
- **Date captured**: 2026-05-14, against `main`.
- **Test runner constraint**: `--test-threads=1` is required. A few worker tests in `budi-daemon` (`workers::pricing_refresh`, `workers::team_pricing`) mutate process-wide env vars under a `std::sync::Mutex`. Under parallelism, one panicking test poisons the lock and the next thread observes `PoisonError` rather than the real condition. Single-threaded test execution sidesteps the race without changing the tests. (Follow-up: rework the env-mutation tests to scope env per-process or use a `parking_lot::Mutex` that does not poison on panic — separate issue.)

## Headline numbers

| Metric    | Total | Missed | Covered |
| --------- | ----: | -----: | ------: |
| Regions   | 58,024 | 21,064 | **63.70%** |
| Functions | 3,512  | 1,218  | **65.32%** |
| Lines     | 37,633 | 13,505 | **64.11%** |

No branch coverage is reported — llvm-cov does not emit branch counts for Rust today.

## Reading the table

- `Lines (cov)` = source lines actually executed at least once during the test run.
- `Regions` = compiler-instrumented basic blocks; usually the strictest of the three measures.
- `Functions` = item-level coverage; "uncovered" includes derived items the tests never touch (e.g. `Debug` for a struct only printed on error paths).

Coverage numbers are an interest-rate, not a quality score. A 90% line-coverage module with no assertions on the hot path can still ship bugs; a 40% module that fully exercises its public contract can be sound. The numbers below are starting points for *gap analysis*, not targets.

## Per-file coverage (line %)

Sorted by coverage ascending. Files at 100% (only `pipeline/emit.rs`) omitted for brevity. Lines columns are absolute counts from llvm-cov.

| File | Lines | Missed | Cover |
| --- | ---: | ---: | ---: |
| `budi-daemon/src/routes/analytics.rs` | 1,025 | 1,012 | **1.27%** |
| `budi-cli/src/commands/update.rs` | 556 | 526 | 5.40% |
| `budi-daemon/src/routes/pricing.rs` | 136 | 121 | 11.03% |
| `budi-cli/src/commands/db.rs` | 107 | 90 | 15.89% |
| `budi-cli/src/commands/sessions.rs` | 418 | 333 | 20.33% |
| `budi-cli/src/daemon.rs` | 496 | 395 | 20.36% |
| `budi-cli/src/commands/uninstall.rs` | 584 | 455 | 22.09% |
| `budi-cli/src/commands/init.rs` | 392 | 299 | 23.72% |
| `budi-daemon/src/routes/hooks.rs` | 713 | 531 | 25.53% |
| `budi-cli/src/commands/stats/mod.rs` | 1,547 | 1,140 | 26.31% |
| `budi-cli/src/commands/cloud.rs` | 691 | 481 | 30.39% |
| `budi-cli/src/commands/status.rs` | 137 | 92 | 32.85% |
| `budi-cli/src/client.rs` | 1,057 | 705 | 33.30% |
| `budi-cli/src/commands/autostart.rs` | 124 | 83 | 33.06% |
| `budi-core/src/providers/claude_code.rs` | 88 | 56 | 36.36% |
| `budi-cli/src/commands/integrations.rs` | 782 | 437 | 44.12% |
| `budi-cli/src/commands/import.rs` | 313 | 167 | 46.65% |
| `budi-core/src/providers/cursor/mod.rs` | 1,486 | 758 | 48.99% |
| `budi-core/src/analytics/sync.rs` | 906 | 471 | 48.01% |
| `budi-cli/src/commands/pricing.rs` | 526 | 261 | 50.38% |
| `budi-daemon/src/workers/cloud_sync.rs` | 157 | 69 | 56.05% |
| `budi-daemon/src/workers/pricing_refresh.rs` | 323 | 155 | 52.01% |
| `budi-daemon/src/workers/team_pricing.rs` | 253 | 118 | 53.36% |
| `budi-daemon/src/routes/cloud.rs` | 366 | 150 | 59.02% |
| `budi-core/src/analytics/queries/summary.rs` | 800 | 271 | 66.12% |
| `budi-core/src/analytics/queries/dimensions.rs` | 1,147 | 310 | 72.97% |
| `budi-core/src/analytics/sessions.rs` | 855 | 199 | 76.73% |
| `budi-core/src/cloud_sync/mod.rs` | 669 | 210 | 68.61% |
| `budi-daemon/src/main.rs` | 600 | 206 | 65.67% |
| `budi-cli/src/commands/statusline.rs` | 811 | 282 | 65.23% |
| `budi-core/src/analytics/health.rs` | 703 | 162 | 76.96% |
| `budi-core/src/analytics/queries/breakdowns.rs` | 756 | 122 | 83.86% |
| `budi-core/src/providers/jetbrains_ai_assistant.rs` | 329 | 55 | 83.28% |
| `budi-core/src/providers/copilot.rs` | 406 | 69 | 83.00% |
| `budi-core/src/providers/codex.rs` | 378 | 61 | 83.86% |
| `budi-core/src/migration.rs` | 1,833 | 84 | 95.42% |
| `budi-core/src/jsonl.rs` | 594 | 23 | 96.13% |
| `budi-core/src/pipeline/mod.rs` | 810 | 25 | 96.91% |
| `budi-core/src/pipeline/enrichers.rs` | 686 | 51 | 92.57% |
| `budi-core/src/pipeline/emit.rs` | 161 | 0 | 100.00% |

For the full per-file listing run `scripts/coverage.sh` locally.

## Top-10 gap list

Ranked by (low coverage) × (blast radius — defined per #804 as anything in `crates/budi-core/src/{providers,pipeline,pricing,cloud_sync,migration}` plus public HTTP surfaces in `budi-daemon/src/routes/`). Each entry says where the gap is and what the follow-up plan is.

1. **`budi-daemon/src/routes/analytics.rs` — 1.27%** — the entire HTTP analytics surface is essentially untested at the route layer. The underlying query modules (`analytics/queries/*`) cover the SQL/shape logic, but request validation, error mapping, pagination, and `connect_info` gating live in the route module. **Follow-up**: #816 (8.5.2).
2. **`budi-core/src/providers/cursor/mod.rs` — 49.0% → 75.9%** — the largest provider module (1,486 lines), and the one with the most volatile upstream contract (see ADR-0090). **Closed by #819** in 8.5.2: each ADR-0090 §1 `kind` variant (`INCLUDED_IN_BUSINESS`, `FREE_CREDIT`, `USAGE_BASED`, subscription `Included`, opaque) now has a parser test, plus tests for the bubble path, session-context attachment, watermark filtering, the legacy-cwd repair, and the JSONL fallback parser.
3. **`budi-daemon/src/routes/hooks.rs` — 25.5%** — `surface_for_path` and `collect_health_sources` are well tested; the actual handler bodies are not. The surface is on the proxy hot path. **Follow-up**: #817 (8.5.2).
4. **`budi-cli/src/commands/stats/mod.rs` — 26.3%** — recently split out of the legacy mega-module (#813). Stat formatting is the main user-visible surface. **Follow-up**: #821 (8.5.2) — golden-output tests for the formatter functions; the IO orchestration can stay uncovered for now.
5. **`budi-cli/src/client.rs` — 33.3%** — daemon-client glue: HTTP request construction, error mapping, retry policy. **Follow-up**: #822 (8.5.2) — mock-server tests.
6. **`budi-cli/src/commands/cloud.rs` — 30.4%** — the cloud CLI subcommands. Cloud is alpha (R4) — coverage will follow the feature work. **Note**: covered by R4 follow-ups, no separate gap issue needed.
7. **`budi-daemon/src/routes/pricing.rs` — 11.0%** — small surface (136 lines). `recompute_query_*` tests cover the query parser. **Follow-up**: #818 (8.5.2).
8. **`budi-core/src/providers/claude_code.rs` — 36.4%** — small file (88 lines), but the provider for our flagship surface. **Follow-up**: #820 (8.5.2) — quick win, fixture-based.
9. **`budi-cli/src/commands/init.rs` — 23.7%** — onboarding flow; touches filesystem, `gh`, daemon start. Hard to unit-test cleanly. **Note**: covered by existing install-script e2e tests in CI; no unit-test gap issue.
10. **`budi-core/src/analytics/sync.rs` — 48.0%** — cloud-sync producer (mints sync chunks). Adjacent to `cloud_sync/mod.rs`. **Follow-up**: #823 (8.5.2).

### Items deliberately not on the gap list

- **`budi-cli/src/commands/update.rs` (5.40%)** — self-update path. Hits GitHub releases and writes to `$PATH`. The on-disk swap is exercised by the release-flow e2e in CI; the rest is best left as integration coverage. No unit-test gap issue.
- **`budi-cli/src/commands/db.rs` (15.9%)** and **`budi-cli/src/commands/uninstall.rs` (22.1%)** — destructive maintenance commands; already exercised by `scripts/install.sh` / `scripts/uninstall.sh` smoke tests in CI.
- **`budi-cli/src/commands/sessions.rs` (20.3%)** and **`budi-cli/src/daemon.rs` (20.4%)** — thin CLI shells that delegate into well-covered core modules.

### Already well covered

- `budi-core/src/pipeline/` — `emit.rs` 100%, `mod.rs` 96.9%, `enrichers.rs` 92.6%. ADR-0089 contract is well exercised.
- `budi-core/src/migration.rs` — 95.4%.
- `budi-core/src/jsonl.rs` — 96.1%.
- `budi-core/src/providers/copilot_chat/` — `mod.rs` 87.7%, `jetbrains.rs` 93.1%. ADR-0092 contract well exercised.
- `budi-core/src/pricing/` — 88–93% across the three files.

## Reproducing locally

```bash
# install once
cargo install cargo-llvm-cov --locked
rustup component add llvm-tools-preview

# text summary
scripts/coverage.sh

# also produce browsable HTML at target/coverage/html/index.html
scripts/coverage.sh --html

# also write an LCOV file (for editor integrations)
scripts/coverage.sh --lcov target/coverage/lcov.info
```

CI runs the same `scripts/coverage.sh` in the `coverage` job (non-blocking; see `.github/workflows/ci.yml`).

## Why not a percentage target

Per #804: a target would push contributors toward easy-coverage gains (testing trivial getters) and away from the gap list above. The acceptance criterion for 8.5.2 is **visibility**, not a number. A target may be reconsidered for 8.6.x once the top-10 gaps are closed; gating in CI is explicitly out of scope here.
