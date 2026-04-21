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

use budi_core::pricing::{self, MAX_PAYLOAD_BYTES, Manifest, PricingSource};

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
pub async fn run(db_path: PathBuf, shutdown: Arc<AtomicBool>) {
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
        let Some(entries) = pricing::load_disk_cache(&cache_path)? else {
            return Ok(());
        };
        // Attach the version from `pricing_manifests` if there's a cached
        // row; fall back to 1 if the audit table is empty for some reason.
        let version = latest_manifest_version(&db_path).unwrap_or(1);
        let fetched_at =
            cache_file_mtime(&cache_path).unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
        let manifest = Manifest {
            version,
            entries,
            fetched_at,
        };
        pricing::install_manifest(manifest, PricingSource::Manifest { version });
        tracing::info!(
            target: "budi_daemon::pricing_refresh",
            version,
            "warm-loaded on-disk pricing cache"
        );
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
        Ok(Ok(report)) => tracing::info!(
            target: "budi_daemon::pricing_refresh",
            version = report.version,
            known_models = report.known_model_count,
            backfilled_rows = report.backfilled_rows,
            "pricing manifest refreshed"
        ),
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
#[derive(Debug, serde::Serialize)]
pub struct RefreshReport {
    pub version: u32,
    pub known_model_count: usize,
    pub backfilled_rows: usize,
}

/// Run a single refresh tick: fetch → validate → atomic-write → install
/// → backfill unknowns. Called by the daemon's periodic loop and by the
/// `POST /pricing/refresh` route.
pub fn run_tick(db_path: &std::path::Path) -> anyhow::Result<RefreshReport> {
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
    let candidate = Manifest {
        version: next_version,
        entries,
        fetched_at: now.clone(),
    };
    if let Err(e) = pricing::validate_payload(&candidate, previous_opt, bytes.len()) {
        return Err(anyhow::anyhow!("validation rejected: {e}"));
    }
    // Persist the raw bytes atomically before updating in-memory state so
    // the on-disk cache and the `pricing_manifests` row are coherent.
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
    let backfilled_rows = pricing::backfill_unknown_rows(&conn, next_version).unwrap_or(0);
    Ok(RefreshReport {
        version: next_version,
        known_model_count,
        backfilled_rows,
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
