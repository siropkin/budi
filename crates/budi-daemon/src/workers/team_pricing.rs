//! Team-pricing worker — polls `GET /v1/pricing/active` and recomputes
//! `messages.cost_cents_effective` when the org's list version bumps.
//!
//! See [ADR-0094] §6 (cloud → local pull) and [#731].
//!
//! Cadence: every 1 h while the daemon runs, configurable via
//! `BUDI_TEAM_PRICING_REFRESH_SECS`. Network calls are gated by the same
//! `BUDI_PRICING_REFRESH=0` opt-out as the LiteLLM manifest refresher — one
//! switch disables both fetches per the ticket's acceptance criteria.
//!
//! Status-code handling matches ADR-0094 §6:
//!
//! | Code         | Behaviour                                                                 |
//! |--------------|---------------------------------------------------------------------------|
//! | 200          | Persist + hot-swap + recompute. Audit row inserted.                       |
//! | 304          | Noop (the `since_version` header short-circuited the server).             |
//! | 404          | Clear in-memory pricing; reset `_effective := _ingested`.                 |
//! | 401          | Log warn, stop polling until next daemon restart.                         |
//! | network/5xx  | Log warn, retry next tick. No exponential backoff beyond the 1 h cadence. |
//!
//! [ADR-0094]: https://github.com/siropkin/budi/blob/main/docs/adr/0094-custom-team-pricing-and-effective-cost-recalculation.md
//! [#731]: https://github.com/siropkin/budi/issues/731

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use budi_core::pricing::team::{self, RecomputeSummary, TeamPricing};

/// Default cadence: 1 h. ADR-0094 §6.
const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Shared with the LiteLLM manifest refresher: when set to `0`/`false`/`off`,
/// no network fetches happen.
pub(crate) const DISABLE_ENV_VAR: &str = "BUDI_PRICING_REFRESH";

/// Optional override for the poll cadence. Same semantics as the LiteLLM
/// worker's `BUDI_PRICING_REFRESH` switch — a value of `0` here is treated
/// as "disabled", not "poll instantly", to match the ticket spec
/// ("one switch, both behaviors").
const REFRESH_INTERVAL_ENV: &str = "BUDI_TEAM_PRICING_REFRESH_SECS";

/// Endpoint route appended to the configured cloud base URL.
const ENDPOINT_PATH: &str = "/v1/pricing/active";

/// HTTP timeout per poll, matching the cloud-sync 30 s budget.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) async fn run(db_path: PathBuf, shutdown: Arc<AtomicBool>) {
    // Warm-load any cached price list so the very first `budi pricing
    // status` call after a daemon restart still has the team layer
    // populated. Cheap — a single JSON read off disk.
    warm_load_disk_cache().await;

    if is_refresh_disabled() {
        tracing::info!(
            target: "budi_daemon::team_pricing",
            env_var = DISABLE_ENV_VAR,
            "team-pricing poll disabled; in-memory state is whatever the cache held at startup"
        );
        return;
    }

    let interval = resolve_interval();

    // First tick fires immediately so a freshly linked daemon picks up
    // pricing without waiting an hour. Subsequent ticks sleep first.
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        if is_refresh_disabled() {
            // Operator flipped the env var while running — wait politely
            // and re-check on next tick. Matches LiteLLM refresher.
            if sleep_with_shutdown(interval, &shutdown).await {
                return;
            }
            continue;
        }
        tick(&db_path).await;
        if sleep_with_shutdown(interval, &shutdown).await {
            return;
        }
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

fn resolve_interval() -> Duration {
    if let Ok(v) = std::env::var(REFRESH_INTERVAL_ENV)
        && let Ok(n) = v.trim().parse::<u64>()
        && n > 0
    {
        return Duration::from_secs(n);
    }
    DEFAULT_REFRESH_INTERVAL
}

async fn warm_load_disk_cache() {
    let result = tokio::task::spawn_blocking(|| -> anyhow::Result<()> {
        let path = team::cache_path()?;
        if let Some(pricing) = team::load_cache(&path)? {
            tracing::info!(
                target: "budi_daemon::team_pricing",
                list_version = pricing.list_version,
                rows = pricing.rows.len(),
                "warm-loaded team-pricing cache"
            );
            team::install(Some(pricing));
        }
        Ok(())
    })
    .await;
    if let Err(e) = result {
        tracing::warn!(
            target: "budi_daemon::team_pricing",
            error = %e,
            "warm-load task panicked"
        );
    } else if let Ok(Err(e)) = result {
        tracing::warn!(
            target: "budi_daemon::team_pricing",
            error = %e,
            "warm-load failed; team pricing inactive until the next successful poll"
        );
    }
}

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
        Ok(Ok(TickOutcome::Updated(summary))) => {
            tracing::info!(
                target: "budi_daemon::team_pricing",
                list_version = summary.list_version,
                rows_processed = summary.rows_processed,
                rows_changed = summary.rows_changed,
                "team-pricing list installed; messages recomputed"
            );
        }
        Ok(Ok(TickOutcome::Cleared(summary))) => {
            tracing::info!(
                target: "budi_daemon::team_pricing",
                rows_processed = summary.rows_processed,
                rows_changed = summary.rows_changed,
                "team-pricing cleared; effective cost reset to ingested"
            );
        }
        Ok(Ok(TickOutcome::Unchanged)) => {
            tracing::debug!(
                target: "budi_daemon::team_pricing",
                "team-pricing unchanged (304 or version match)"
            );
        }
        Ok(Ok(TickOutcome::NotConfigured)) => {
            tracing::debug!(
                target: "budi_daemon::team_pricing",
                "cloud not configured; team-pricing poll skipped"
            );
        }
        Ok(Err(e)) => tracing::warn!(
            target: "budi_daemon::team_pricing",
            error = %e,
            "team-pricing tick failed"
        ),
        Err(e) => tracing::error!(
            target: "budi_daemon::team_pricing",
            error = %e,
            "team-pricing task panicked"
        ),
    }
}

#[derive(Debug)]
enum TickOutcome {
    Updated(RecomputeSummary),
    Cleared(RecomputeSummary),
    Unchanged,
    NotConfigured,
}

/// CLI-facing outcome from a manual `budi pricing recompute` call. Adds
/// the `ForcedRecompute` variant on top of [`TickOutcome`] so the CLI
/// can distinguish "the list version was unchanged but we ran anyway"
/// from "we installed a new list".
#[derive(Debug)]
pub(crate) enum CliTickOutcome {
    Updated(RecomputeSummary),
    Cleared(RecomputeSummary),
    ForcedRecompute(RecomputeSummary),
    Unchanged,
    NotConfigured,
}

/// Entry point for `POST /pricing/recompute` (#732). When `force` is
/// true and the in-memory list version is unchanged, skip the network
/// fetch and re-run `recompute_messages` against the currently-
/// installed list anyway — useful for support cases where the operator
/// suspects a cost number drifted.
pub(crate) fn run_tick_for_cli(force: bool) -> anyhow::Result<CliTickOutcome> {
    let db_path = budi_core::analytics::db_path()?;
    let outcome = run_tick(&db_path)?;
    match outcome {
        TickOutcome::Updated(s) => Ok(CliTickOutcome::Updated(s)),
        TickOutcome::Cleared(s) => Ok(CliTickOutcome::Cleared(s)),
        TickOutcome::NotConfigured => Ok(CliTickOutcome::NotConfigured),
        TickOutcome::Unchanged => {
            if !force {
                return Ok(CliTickOutcome::Unchanged);
            }
            let conn = budi_core::analytics::open_db(&db_path)?;
            let snap = team::snapshot();
            let summary = team::recompute_messages(&conn, snap.as_ref())?;
            insert_audit_row(&conn, &summary)?;
            Ok(CliTickOutcome::ForcedRecompute(summary))
        }
    }
}

fn run_tick(db_path: &std::path::Path) -> anyhow::Result<TickOutcome> {
    let config = budi_core::config::load_cloud_config();
    let Some(api_key) = config.effective_api_key() else {
        return Ok(TickOutcome::NotConfigured);
    };
    let endpoint = config.effective_endpoint();
    let current_version = team::snapshot().map(|p| p.list_version).unwrap_or(0);

    match fetch_pricing(&endpoint, &api_key, current_version)? {
        FetchOutcome::Updated(pricing) => {
            // Persist before hot-swap so a daemon restart between
            // install + recompute can reload the same pricing.
            let cache = team::cache_path()?;
            team::write_cache(&cache, &pricing)?;
            team::install(Some(pricing));
            let conn = budi_core::analytics::open_db(db_path)?;
            let snap = team::snapshot();
            let summary = team::recompute_messages(&conn, snap.as_ref())?;
            insert_audit_row(&conn, &summary)?;
            Ok(TickOutcome::Updated(summary))
        }
        FetchOutcome::NoActiveList => {
            let cache = team::cache_path()?;
            team::clear_cache(&cache)?;
            team::install(None);
            let conn = budi_core::analytics::open_db(db_path)?;
            let summary = team::recompute_messages(&conn, None)?;
            insert_audit_row(&conn, &summary)?;
            Ok(TickOutcome::Cleared(summary))
        }
        FetchOutcome::Unchanged => Ok(TickOutcome::Unchanged),
    }
}

#[derive(Debug)]
enum FetchOutcome {
    Updated(TeamPricing),
    NoActiveList,
    Unchanged,
}

fn fetch_pricing(
    endpoint: &str,
    api_key: &str,
    since_version: u32,
) -> anyhow::Result<FetchOutcome> {
    let url = format!("{endpoint}{ENDPOINT_PATH}?since_version={since_version}");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .build()
        .into();
    let result = agent
        .get(&url)
        .header("Authorization", &format!("Bearer {api_key}"))
        .call();
    match result {
        Ok(mut response) => {
            // #747: ureq's `call()` returns `Ok` for 304 in this workspace's
            // pinned version (304 is not a 4xx/5xx). The body is empty, so
            // `read_json()` would surface an EOF parse error to the user.
            // Short-circuit before touching the body. The matching 304 arm
            // on the `Err` side stays as defense-in-depth in case a future
            // ureq bump flips the classification.
            if response.status() == 304 {
                return Ok(FetchOutcome::Unchanged);
            }
            let pricing: TeamPricing = response
                .body_mut()
                .read_json()
                .map_err(|e| anyhow::anyhow!("parse team-pricing response: {e}"))?;
            // The server may also reply 200 with a body that has the same
            // version we already know — treat that as Unchanged to avoid a
            // redundant recompute pass.
            if pricing.list_version == since_version && since_version != 0 {
                Ok(FetchOutcome::Unchanged)
            } else {
                Ok(FetchOutcome::Updated(pricing))
            }
        }
        Err(ureq::Error::StatusCode(304)) => Ok(FetchOutcome::Unchanged),
        Err(ureq::Error::StatusCode(404)) => Ok(FetchOutcome::NoActiveList),
        Err(ureq::Error::StatusCode(401)) => {
            anyhow::bail!("authentication failed (401); stopping team-pricing poll")
        }
        Err(ureq::Error::StatusCode(status)) => {
            anyhow::bail!("team-pricing endpoint returned {status}")
        }
        Err(e) => Err(anyhow::anyhow!("team-pricing network error: {e}")),
    }
}

/// Append one row to the local audit table mirroring the cloud's
/// `recalculation_runs` shape. Surfaced by `budi pricing status` (#732).
fn insert_audit_row(conn: &rusqlite::Connection, summary: &RecomputeSummary) -> anyhow::Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO recalculation_runs_local
            (started_at, finished_at, list_version,
             rows_processed, rows_changed,
             before_total_cents, after_total_cents)
         VALUES (?1, ?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            now,
            summary.list_version,
            summary.rows_processed,
            summary.rows_changed,
            summary.before_total_cents,
            summary.after_total_cents,
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_serial() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    #[test]
    fn disabled_env_zero_short_circuits_poll() {
        let _g = env_serial().lock().unwrap();
        unsafe { std::env::set_var(DISABLE_ENV_VAR, "0") };
        let disabled = is_refresh_disabled();
        unsafe { std::env::remove_var(DISABLE_ENV_VAR) };
        assert!(disabled);
    }

    #[test]
    fn disabled_env_false_short_circuits_poll() {
        let _g = env_serial().lock().unwrap();
        unsafe { std::env::set_var(DISABLE_ENV_VAR, "false") };
        let disabled = is_refresh_disabled();
        unsafe { std::env::remove_var(DISABLE_ENV_VAR) };
        assert!(disabled);
    }

    #[test]
    fn refresh_interval_defaults_to_one_hour() {
        let _g = env_serial().lock().unwrap();
        unsafe { std::env::remove_var(REFRESH_INTERVAL_ENV) };
        assert_eq!(resolve_interval(), DEFAULT_REFRESH_INTERVAL);
    }

    #[test]
    fn refresh_interval_honours_env_override() {
        let _g = env_serial().lock().unwrap();
        unsafe { std::env::set_var(REFRESH_INTERVAL_ENV, "90") };
        let interval = resolve_interval();
        unsafe { std::env::remove_var(REFRESH_INTERVAL_ENV) };
        assert_eq!(interval, Duration::from_secs(90));
    }

    #[test]
    fn refresh_interval_ignores_zero_override() {
        // ADR-0094 §6 reserves `BUDI_PRICING_REFRESH=0` (not this var) as
        // the disable switch. A zero on the cadence var would otherwise
        // mean "poll instantly forever".
        let _g = env_serial().lock().unwrap();
        unsafe { std::env::set_var(REFRESH_INTERVAL_ENV, "0") };
        let interval = resolve_interval();
        unsafe { std::env::remove_var(REFRESH_INTERVAL_ENV) };
        assert_eq!(interval, DEFAULT_REFRESH_INTERVAL);
    }

    /// Spawn a one-shot TCP server that replies with the canned response and
    /// returns its `http://127.0.0.1:PORT` endpoint. Keeps the test free of
    /// any new test-only dependency — the existing workspace already exposes
    /// `std::net` and `std::thread`.
    fn spawn_one_shot_server(response: &'static [u8]) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().expect("accept");
            // Drain the request — the daemon will send GET headers + CRLFCRLF.
            // We don't need the bytes, just to read past them so the kernel
            // delivers our write back as the response.
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            stream.write_all(response).expect("write response");
        });
        format!("http://127.0.0.1:{port}")
    }

    /// #747: prior to this fix, ureq's `call()` returned `Ok` for a 304
    /// response in the pinned version, which then tripped `read_json()` on
    /// the empty body and surfaced an EOF parse error to the user every
    /// hourly tick. Assert the worker now reports `Unchanged` instead.
    #[test]
    fn fetch_pricing_treats_ok_304_as_unchanged() {
        let endpoint = spawn_one_shot_server(
            b"HTTP/1.1 304 Not Modified\r\n\
              Content-Length: 0\r\n\
              Connection: close\r\n\
              \r\n",
        );
        let outcome = fetch_pricing(&endpoint, "test-key", 7).expect("304 must not error");
        assert!(
            matches!(outcome, FetchOutcome::Unchanged),
            "got {outcome:?}"
        );
    }

    /// When `BUDI_PRICING_REFRESH=0` is set, `run()` must exit quickly
    /// without issuing a network call. Mirrors the LiteLLM refresher's
    /// gate-7 proof-by-timing.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn run_exits_when_disabled() {
        let _g = env_serial().lock().unwrap();
        unsafe { std::env::set_var(DISABLE_ENV_VAR, "0") };
        let tmp = std::env::temp_dir().join(format!(
            "budi-team-pricing-disabled-{}.db",
            std::process::id()
        ));
        let shutdown = Arc::new(AtomicBool::new(false));
        let start = std::time::Instant::now();
        let result =
            tokio::time::timeout(Duration::from_secs(5), super::run(tmp.clone(), shutdown)).await;
        let elapsed = start.elapsed();
        unsafe { std::env::remove_var(DISABLE_ENV_VAR) };
        let _ = std::fs::remove_file(&tmp);
        assert!(result.is_ok(), "run() must exit when disabled, not hang");
        assert!(
            elapsed < Duration::from_secs(4),
            "run() should return quickly on the disabled path (took {elapsed:?})"
        );
    }
}
