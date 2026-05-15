//! Background pricing manifest refresh worker (ADR-0091 §3).
//!
//! Cadence: once on daemon startup if the on-disk cache is absent or >24 h
//! old; once per 24 h thereafter while the daemon is running. Upstream:
//! the LiteLLM community manifest at
//! `raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json`.
//!
//! The worker runs entirely inside `spawn_blocking` since
//! [`budi_core::pricing`] and `ureq` are sync, matching the pattern in
//! `workers::cloud_sync`. Network + validation failures log `warn` and do
//! not block ingestion — the previous cache (or embedded baseline) keeps
//! serving [`budi_core::pricing::lookup`] until the next tick.
//!
//! Operator opt-out: `BUDI_PRICING_REFRESH=0` in the daemon's environment
//! disables all network calls. The embedded baseline becomes authoritative.
//!
//! # HTTP stack note (ADR-0091 §3 amendment)
//!
//! ADR-0091 §3 describes the HTTP stack as `reqwest`. In practice
//! `budi_core::cloud_sync::sync_tick` uses `ureq` (the only HTTP stack
//! pulled into `budi-core`'s dependency tree). The ADR's real constraint
//! is "no new HTTP stack"; this worker follows the `cloud_sync` pattern
//! and uses `ureq` too. See #376 PR body.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use budi_core::pricing::{self, MAX_PAYLOAD_BYTES, Manifest, PricingSource, RejectedUpstreamRow};

const UPSTREAM_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";

/// 24 hours, per ADR-0091 §3.
const REFRESH_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// How stale the on-disk cache can be before the startup tick fires the
/// first fetch. Matches [`REFRESH_INTERVAL`] so a daemon that was offline
/// for > 24 h kicks a fetch on the next startup.
const STARTUP_STALE_THRESHOLD: Duration = REFRESH_INTERVAL;

/// Env var that disables network fetches.
pub(crate) const DISABLE_ENV_VAR: &str = "BUDI_PRICING_REFRESH";

/// Entry point for the daemon refresh worker.
///
/// Spawned from `budi-daemon::main` alongside the cloud sync worker and
/// the tailer. Stops when `shutdown` is set; shutdown is checked between
/// ticks and during the 24 h sleep via a poll loop so clean exits don't
/// wait 24 h.
pub(crate) async fn run(db_path: PathBuf, shutdown: Arc<AtomicBool>) {
    // Warm-load the on-disk cache if present — the daemon may have been
    // restarted between refreshes and the embedded baseline in `pricing`
    // would otherwise serve until the next tick.
    warm_load_disk_cache(&db_path).await;

    if is_refresh_disabled() {
        tracing::info!(
            target: "budi_daemon::pricing_refresh",
            env_var = DISABLE_ENV_VAR,
            "network refresh disabled; embedded baseline is authoritative"
        );
        return;
    }

    // Startup fetch: only if cache is absent or stale.
    if should_fetch_on_startup().await {
        tick(&db_path).await;
    }

    // Steady-state loop: sleep, then tick. Exits cleanly on shutdown.
    loop {
        if sleep_with_shutdown(REFRESH_INTERVAL, &shutdown).await {
            return;
        }
        if is_refresh_disabled() {
            continue;
        }
        tick(&db_path).await;
    }
}

fn is_refresh_disabled() -> bool {
    match std::env::var(DISABLE_ENV_VAR) {
        Ok(v) => {
            let v = v.trim();
            v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off")
        }
        Err(_) => false,
    }
}

async fn warm_load_disk_cache(db_path: &std::path::Path) {
    let db_path = db_path.to_path_buf();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let cache_path = pricing::pricing_cache_path()?;
        let Some(mut entries) = pricing::load_disk_cache(&cache_path)? else {
            return Ok(());
        };
        // Attach the version from `pricing_manifests` if there's a cached
        // row; fall back to 1 if the audit table is empty for some reason.
        let version = latest_manifest_version(&db_path).unwrap_or(1);
        let fetched_at =
            cache_file_mtime(&cache_path).unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
        // ADR-0091 §2 amendment (8.3.1 / #483): the on-disk cache holds
        // the raw upstream bytes for audit, so warm load must re-run
        // row-level sanity on restart. Otherwise a daemon restart after a
        // tick that wrote bytes containing a ceiling-exceeding row would
        // re-admit the row into the in-memory lookup.
        let rejected_upstream_rows = pricing::partition_rows_by_sanity(&mut entries);
        // 8.4.2 / #680: the alias overlay is Budi-curated, not part
        // of the upstream LiteLLM payload, so the refresh path
        // attaches the embedded aliases on every install. Otherwise
        // a refresh tick would clear the overlay until daemon restart.
        let manifest = Manifest {
            version,
            entries,
            aliases: pricing::embedded_aliases(),
            fetched_at,
        };
        pricing::install_manifest(manifest, PricingSource::Manifest { version });
        pricing::install_rejected_upstream_rows(rejected_upstream_rows.clone());
        tracing::info!(
            target: "budi_daemon::pricing_refresh",
            version,
            rejected_upstream_rows = rejected_upstream_rows.len(),
            "warm-loaded on-disk pricing cache"
        );
        for row in &rejected_upstream_rows {
            tracing::warn!(
                target: "budi_daemon::pricing_refresh",
                event = "rejected_upstream_row",
                model_id = row.model_id,
                reason = row.reason,
                "warm-load dropped cached row failing per-row sanity"
            );
        }
        Ok(())
    })
    .await;
    if let Err(e) = result {
        tracing::warn!(
            target: "budi_daemon::pricing_refresh",
            error = %e,
            "warm-load task panicked"
        );
    } else if let Ok(Err(e)) = result {
        tracing::warn!(
            target: "budi_daemon::pricing_refresh",
            error = %e,
            "warm-load failed; embedded baseline remains authoritative"
        );
    }
}

async fn should_fetch_on_startup() -> bool {
    let path = match pricing::pricing_cache_path() {
        Ok(p) => p,
        Err(_) => return true,
    };
    tokio::task::spawn_blocking(move || match std::fs::metadata(&path) {
        Ok(md) => md
            .modified()
            .ok()
            .and_then(|t| t.elapsed().ok())
            .map(|e| e >= STARTUP_STALE_THRESHOLD)
            .unwrap_or(true),
        Err(_) => true,
    })
    .await
    .unwrap_or(true)
}

/// Sleep for `duration`, polling `shutdown` every 5 seconds so a clean
/// exit doesn't wait the full interval. Returns `true` if shutdown fired.
async fn sleep_with_shutdown(duration: Duration, shutdown: &AtomicBool) -> bool {
    const POLL: Duration = Duration::from_secs(5);
    let deadline = tokio::time::Instant::now() + duration;
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return true;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return false;
        }
        let remaining = deadline - now;
        tokio::time::sleep(POLL.min(remaining)).await;
    }
}

async fn tick(db_path: &std::path::Path) {
    let db_path = db_path.to_path_buf();
    let result = tokio::task::spawn_blocking(move || run_tick(&db_path)).await;
    match result {
        Ok(Ok(report)) => {
            tracing::info!(
                target: "budi_daemon::pricing_refresh",
                version = report.version,
                known_models = report.known_model_count,
                backfilled_rows = report.backfilled_rows,
                rejected_upstream_rows = report.rejected_upstream_rows.len(),
                "pricing manifest refreshed"
            );
            // One structured warn per rejected row so an operator grepping
            // the daemon log for `rejected_upstream_row` sees exactly what
            // the current refresh filtered out. ADR-0091 §2 amendment
            // acceptance: every skipped row is identified by model id.
            for row in &report.rejected_upstream_rows {
                tracing::warn!(
                    target: "budi_daemon::pricing_refresh",
                    event = "rejected_upstream_row",
                    model_id = row.model_id,
                    reason = row.reason,
                    "dropped upstream pricing row failed per-row sanity"
                );
            }
        }
        Ok(Err(e)) => tracing::warn!(
            target: "budi_daemon::pricing_refresh",
            error = %e,
            "pricing refresh failed; previous cache remains authoritative"
        ),
        Err(e) => tracing::error!(
            target: "budi_daemon::pricing_refresh",
            error = %e,
            "pricing refresh task panicked"
        ),
    }
}

/// Public return shape for a single refresh tick. Serialized directly by
/// `POST /pricing/refresh` so the CLI `--refresh` flag can surface the
/// outcome without re-running its own lookup.
///
/// `rejected_upstream_rows` (8.3.1+, #483) lists upstream rows dropped
/// by the row-level sanity partition. Pre-8.3.1 the same rows would
/// have whole-payload-rejected the refresh (ADR-0091 §2 amendment).
#[derive(Debug, serde::Serialize)]
pub(crate) struct RefreshReport {
    pub version: u32,
    pub known_model_count: usize,
    pub backfilled_rows: usize,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub rejected_upstream_rows: Vec<RejectedUpstreamRow>,
}

/// Run a single refresh tick: fetch → partition/validate → atomic-write
/// → install → backfill unknowns. Called by the daemon's periodic loop
/// and by the `POST /pricing/refresh` route.
///
/// ADR-0091 §2 amendment (8.3.1 / #483): per-row sanity failures no
/// longer hard-fail the tick. Rows are partitioned, the kept rows are
/// written to the cache, and the rejected rows are returned via
/// `RefreshReport.rejected_upstream_rows` + surfaced on
/// `GET /pricing/status`.
pub(crate) fn run_tick(db_path: &std::path::Path) -> anyhow::Result<RefreshReport> {
    let bytes = fetch_upstream()?;
    let entries = pricing::parse_entries(&bytes)?;
    let now = chrono::Utc::now().to_rfc3339();
    let previous = pricing::current_manifest_snapshot();
    let previous_opt = if previous.entries.is_empty() {
        None
    } else {
        Some(&previous)
    };
    // Version is assigned under the DB transaction so concurrent writers
    // (none in practice, but defensive) can't collide on the primary key.
    let conn = budi_core::analytics::open_db(db_path)?;
    let next_version = next_manifest_version(&conn)?;
    // 8.4.2 / #680: attach the curated alias overlay on every
    // refresh tick. LiteLLM ships no aliases section; the overlay
    // is Budi-owned and rebuilt cheaply per install.
    let mut candidate = Manifest {
        version: next_version,
        entries,
        aliases: pricing::embedded_aliases(),
        fetched_at: now.clone(),
    };
    let rejected_upstream_rows =
        match pricing::validate_payload(&mut candidate, previous_opt, bytes.len()) {
            Ok(rejected) => rejected,
            Err(e) => return Err(anyhow::anyhow!("validation rejected: {e}")),
        };
    // Persist the raw bytes atomically before updating in-memory state so
    // the on-disk cache and the `pricing_manifests` row are coherent.
    // Note: the on-disk cache holds the RAW upstream bytes, not the
    // partitioned kept set. `load_disk_cache` re-runs `parse_entries` on
    // warm-load, and the kept set is re-derived from the in-memory
    // install below. This preserves exact fidelity for audit / replay.
    let cache_path = pricing::pricing_cache_path()?;
    pricing::atomic_write_cache(&cache_path, &bytes)?;
    insert_manifest_row(
        &conn,
        next_version,
        &now,
        "network",
        None,
        candidate.entries.len() as i64,
    )?;
    let known_model_count = candidate.entries.len();
    pricing::install_manifest(
        candidate,
        PricingSource::Manifest {
            version: next_version,
        },
    );
    pricing::install_rejected_upstream_rows(rejected_upstream_rows.clone());
    let backfilled_rows = pricing::backfill_unknown_rows(&conn, next_version).unwrap_or(0);
    Ok(RefreshReport {
        version: next_version,
        known_model_count,
        backfilled_rows,
        rejected_upstream_rows,
    })
}

fn fetch_upstream() -> anyhow::Result<Vec<u8>> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into();
    let mut response = agent
        .get(UPSTREAM_URL)
        .call()
        .map_err(|e| anyhow::anyhow!("upstream fetch failed: {e}"))?;
    // `read_to_vec` respects an explicit body size cap so a hostile
    // upstream can't pin ingestion memory.
    let bytes = response
        .body_mut()
        .with_config()
        .limit(MAX_PAYLOAD_BYTES as u64)
        .read_to_vec()
        .map_err(|e| anyhow::anyhow!("upstream read failed: {e}"))?;
    Ok(bytes)
}

// ---------------------------------------------------------------------------
// pricing_manifests audit table helpers
// ---------------------------------------------------------------------------

fn latest_manifest_version(db_path: &std::path::Path) -> Option<u32> {
    let conn = budi_core::analytics::open_db(db_path).ok()?;
    conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM pricing_manifests",
        [],
        |r| r.get::<_, i64>(0),
    )
    .ok()
    .and_then(|v| u32::try_from(v).ok())
}

fn next_manifest_version(conn: &rusqlite::Connection) -> anyhow::Result<u32> {
    let max: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM pricing_manifests",
        [],
        |r| r.get(0),
    )?;
    Ok(u32::try_from(max + 1).unwrap_or(1))
}

fn insert_manifest_row(
    conn: &rusqlite::Connection,
    version: u32,
    fetched_at: &str,
    source: &str,
    upstream_etag: Option<&str>,
    known_model_count: i64,
) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO pricing_manifests
            (version, fetched_at, source, upstream_etag, known_model_count)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            version,
            fetched_at,
            source,
            upstream_etag,
            known_model_count
        ],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Misc
// ---------------------------------------------------------------------------

fn cache_file_mtime(path: &std::path::Path) -> Option<String> {
    let md = std::fs::metadata(path).ok()?;
    let mtime = md.modified().ok()?;
    let since = mtime.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64;
    chrono::DateTime::<chrono::Utc>::from_timestamp(since, 0).map(|dt| dt.to_rfc3339())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serial mutex for tests that touch the process-global env table
    /// under `DISABLE_ENV_VAR`. Without this, `cargo test` runs the
    /// env-setting tests in parallel and they observe each other's
    /// writes between `set_var` and `remove_var`.
    fn env_serial() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    #[test]
    fn refresh_disabled_by_env_zero() {
        let _g = env_serial().lock().unwrap();
        unsafe { std::env::set_var(DISABLE_ENV_VAR, "0") };
        let disabled = is_refresh_disabled();
        unsafe { std::env::remove_var(DISABLE_ENV_VAR) };
        assert!(disabled);
    }

    #[test]
    fn refresh_disabled_by_env_false() {
        let _g = env_serial().lock().unwrap();
        unsafe { std::env::set_var(DISABLE_ENV_VAR, "false") };
        let disabled = is_refresh_disabled();
        unsafe { std::env::remove_var(DISABLE_ENV_VAR) };
        assert!(disabled);
    }

    #[test]
    fn refresh_enabled_when_env_unset() {
        let _g = env_serial().lock().unwrap();
        unsafe { std::env::remove_var(DISABLE_ENV_VAR) };
        assert!(!is_refresh_disabled());
    }

    #[test]
    fn refresh_enabled_when_env_empty_or_other() {
        let _g = env_serial().lock().unwrap();
        unsafe { std::env::set_var(DISABLE_ENV_VAR, "1") };
        let enabled = !is_refresh_disabled();
        unsafe { std::env::remove_var(DISABLE_ENV_VAR) };
        assert!(enabled);
    }

    /// ADR-0091 §2 amendment (8.3.1 / #483): a payload containing one
    /// ceiling-exceeding row refreshes successfully; the bad row is
    /// filtered, the rest of the manifest lands. This is the 2026-04-22
    /// `wandb/Qwen3-Coder-480B-A35B-Instruct` regression fixed in
    /// RC-1.
    ///
    /// Test acts on the `pricing::` public API directly (rather than
    /// spinning the network path) so it stays hermetic in CI. The
    /// wire/cache/install contract is the same — `run_tick` is a thin
    /// wrapper over `validate_payload` + `install_manifest` +
    /// `install_rejected_upstream_rows` + `backfill_unknown_rows`.
    /// Serial mutex for daemon tests that touch the process-global
    /// pricing state. Kept private to this test module; budi-core has
    /// its own separate serial lock in `pricing::pricing_tests`.
    fn pricing_state_serial() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    #[test]
    fn refresh_skips_invalid_row_keeps_rest_when_majority_valid() {
        use budi_core::pricing::{
            self as pricing, ManifestEntry, PricingOutcome, PricingSource, RejectedUpstreamRow,
        };
        use std::collections::HashMap;

        let _g = pricing_state_serial().lock().unwrap();

        // Build a 50-row payload where 1 row exceeds the sanity ceiling
        // and 49 rows are valid. Uses the same entry shape the real
        // LiteLLM manifest uses.
        let mut entries: HashMap<String, ManifestEntry> = HashMap::new();
        entries.insert(
            "wandb/Qwen/Qwen3-Coder-480B-A35B-Instruct".to_string(),
            ManifestEntry {
                // 0.1/token = $100,000/M — 100x the ceiling (the
                // actual 2026-04-22 upstream value).
                input_cost_per_token: 0.1,
                output_cost_per_token: 0.1,
                cache_creation_input_token_cost: None,
                cache_read_input_token_cost: None,
                litellm_provider: Some("wandb".to_string()),
            },
        );
        for i in 0..49 {
            entries.insert(
                format!("claude-test-{i}"),
                ManifestEntry {
                    input_cost_per_token: 0.000003,
                    output_cost_per_token: 0.000015,
                    cache_creation_input_token_cost: Some(0.00000375),
                    cache_read_input_token_cost: Some(0.0000003),
                    litellm_provider: Some("anthropic".to_string()),
                },
            );
        }
        let mut candidate = pricing::Manifest {
            version: 2,
            entries,
            aliases: HashMap::new(),
            fetched_at: chrono::Utc::now().to_rfc3339(),
        };

        let rejected = pricing::validate_payload(&mut candidate, None, 10_000)
            .expect("validation must accept the payload after row-level partitioning");

        // Exactly the bad row is rejected; the 49 good ones stay.
        assert_eq!(
            rejected,
            vec![RejectedUpstreamRow {
                model_id: "wandb/Qwen/Qwen3-Coder-480B-A35B-Instruct".to_string(),
                reason: "$100000.00/M exceeds sanity ceiling $1000/M".to_string(),
            }]
        );
        assert_eq!(candidate.entries.len(), 49);

        // Install the partitioned manifest. The rejected row must NOT
        // resolve via `lookup` — it was dropped from the kept set.
        pricing::install_manifest(candidate, PricingSource::Manifest { version: 2 });
        pricing::install_rejected_upstream_rows(rejected);

        match pricing::lookup("wandb/Qwen/Qwen3-Coder-480B-A35B-Instruct", "wandb") {
            PricingOutcome::Known { .. } => {
                panic!("rejected row must not resolve in lookup")
            }
            PricingOutcome::Unknown { .. } => {}
        }
        // Good rows still resolve.
        match pricing::lookup("claude-test-0", "claude_code") {
            PricingOutcome::Known { .. } => {}
            PricingOutcome::Unknown { .. } => panic!("kept row should resolve"),
        }

        // `pricing status` surfaces the rejected row.
        let state = pricing::current_state();
        assert_eq!(state.rejected_upstream_rows.len(), 1);
        assert_eq!(
            state.rejected_upstream_rows[0].model_id,
            "wandb/Qwen/Qwen3-Coder-480B-A35B-Instruct"
        );

        // Reset rejected-rows list + re-install embedded baseline so
        // this test doesn't leak state into sibling tests that share
        // the process-global pricing state.
        pricing::install_rejected_upstream_rows(Vec::new());
        let baseline = pricing::load_embedded_manifest().expect("embedded baseline must parse");
        pricing::install_manifest(baseline, PricingSource::EmbeddedBaseline);
    }

    /// ADR-0091 §6 / #376 test gate 7: when `BUDI_PRICING_REFRESH=0` is
    /// set in the daemon's env, the worker must not issue any network
    /// call. Proof-by-timing: the disabled path returns almost
    /// immediately after `warm_load_disk_cache`; the enabled path would
    /// either hit the upstream or enter the 24 h sleep loop. A `run()`
    /// that returns in <1 s under `timeout(5 s)` is therefore only
    /// achievable on the disabled code path.
    // The guard must be held across the `.await` so a sibling test
    // running concurrently can't `remove_var(DISABLE_ENV_VAR)` before
    // the worker's `is_refresh_disabled()` check reads it. `tokio::test`
    // uses the single-threaded current-thread runtime here, so the lock
    // stays on one OS thread — clippy's lint is about scheduling
    // concerns that don't apply to this shape. See the other env tests
    // in this module, which share the same `env_serial` mutex.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn gate_7_disabled_env_suppresses_network_and_exits() {
        let _g = env_serial().lock().unwrap();
        unsafe { std::env::set_var(DISABLE_ENV_VAR, "0") };
        let tmp =
            std::env::temp_dir().join(format!("budi-pricing-gate7-{}.db", std::process::id()));
        let shutdown = Arc::new(AtomicBool::new(false));

        let start = std::time::Instant::now();
        let result =
            tokio::time::timeout(Duration::from_secs(5), super::run(tmp.clone(), shutdown)).await;
        let elapsed = start.elapsed();
        unsafe { std::env::remove_var(DISABLE_ENV_VAR) };
        let _ = std::fs::remove_file(&tmp);

        assert!(
            result.is_ok(),
            "run() must exit when refresh is disabled, not hang"
        );
        assert!(
            elapsed < Duration::from_secs(4),
            "run() should return quickly on the disabled path (took {elapsed:?})"
        );
    }
}
