//! Background cloud sync worker.
//!
//! Runs on a configurable interval (default 300s per ADR-0083 §9).
//! Pushes scrubbed daily rollups and session summaries to the cloud ingest API.
//! Never blocks terminal execution.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use budi_core::cloud_sync::{self, SyncResult};
use budi_core::config::CloudConfig;

/// Run the cloud sync worker loop. Designed to be spawned with `tokio::spawn`.
///
/// The `cloud_syncing` flag is shared with the manual `POST /cloud/sync`
/// route so a user-triggered flush and the interval-based worker never run
/// concurrently. If the flag is already set when the interval fires, the
/// worker skips that tick — the manual invocation will advance the
/// watermarks.
///
/// `initial_config` is only used for the very first tick's interval; from
/// the second tick onward the worker re-reads `cloud.toml` at the top of
/// every iteration so an api_key rotation (`budi cloud init --force`,
/// cross-org switch per #559, manager-driven rotation) lands on the next
/// sync without a daemon restart (#560).
pub async fn run(db_path: PathBuf, initial_config: CloudConfig, cloud_syncing: Arc<AtomicBool>) {
    let mut config = initial_config;
    let mut consecutive_failures: u32 = 0;
    let mut auth_failed = false;
    let mut schema_mismatch = false;

    loop {
        // #560: re-read cloud.toml every tick so a rewritten api_key /
        // endpoint / org_id / device_id propagates without a daemon
        // restart. The on-disk read is a small TOML parse — cheap at the
        // default 5-minute interval. The previous behaviour cloned the
        // config captured at daemon startup, so a key rotation produced
        // 401s indefinitely until the user `launchctl kickstart`'d the
        // daemon manually.
        let prev_api_key = config.effective_api_key();
        let prev_endpoint = config.effective_endpoint();
        config = budi_core::config::load_cloud_config();
        let interval = Duration::from_secs(config.sync.interval_seconds);
        let retry_max = config.sync.retry_max_seconds;

        // If we were in auth-failed state and the api_key (or endpoint —
        // self-hosted users may swap clouds) actually changed on disk,
        // surface that so the recovery line correlates with the user's
        // edit instead of firing every retry whether or not anything
        // changed (the pre-#560 misleading "Cloud config refreshed,
        // resuming sync" log).
        if auth_failed && credentials_changed(prev_api_key.as_deref(), &prev_endpoint, &config) {
            auth_failed = false;
            tracing::info!("Cloud credentials changed on disk; resuming sync");
        }

        // If auth failed, stop syncing (ADR-0083 §4: "stop syncing and prompt re-auth")
        if auth_failed {
            tracing::warn!(
                "Cloud sync stopped: authentication failed. \
                 Check api_key in ~/.config/budi/cloud.toml."
            );
            // Sleep long; the next iteration re-reads cloud.toml and the
            // `credentials_changed` check above will clear auth_failed if
            // the user has rotated their key.
            tokio::time::sleep(Duration::from_secs(retry_max)).await;
            continue;
        }

        // If schema mismatch, don't retry until updated (ADR-0083 §7)
        if schema_mismatch {
            tracing::warn!(
                "Cloud sync paused: schema mismatch. \
                 Update budi to resume syncing."
            );
            tokio::time::sleep(Duration::from_secs(retry_max)).await;
            // Re-check: user may have updated
            schema_mismatch = false;
            continue;
        }

        // If a manual `budi cloud sync` (or a previous tick) is still
        // running, skip this interval rather than contend — watermarks make
        // this safe and avoid double-posting the same records.
        if cloud_syncing
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            tracing::debug!("Cloud sync skipped: another sync already in progress");
            tokio::time::sleep(interval).await;
            continue;
        }

        // Normal sync tick.
        //
        // We clear the `cloud_syncing` flag via an RAII guard rather than a
        // trailing `flag.store(false)`. If `sync_tick` ever panics inside
        // `spawn_blocking`, a manual reset would be skipped and the flag
        // would stay `true` forever — wedging both this worker and every
        // `POST /cloud/sync` (409 Conflict) until the daemon was restarted.
        // See issue #343.
        let db = db_path.clone();
        let cfg = config.clone();
        let flag = cloud_syncing.clone();
        let result = tokio::task::spawn_blocking(move || {
            let _guard = CloudBusyFlagGuard::new(flag);
            cloud_sync::sync_tick(&db, &cfg)
        })
        .await;

        match result {
            Ok(SyncResult::Success(resp)) => {
                consecutive_failures = 0;
                let upserted = resp.records_upserted.unwrap_or(0);
                if upserted > 0 {
                    tracing::info!(
                        records = upserted,
                        watermark = ?resp.watermark,
                        "Cloud sync completed"
                    );
                } else {
                    tracing::debug!("Cloud sync: server confirmed, no new records");
                }
            }
            Ok(SyncResult::EmptyPayload) => {
                consecutive_failures = 0;
                tracing::debug!("Cloud sync: nothing to send");
            }
            Ok(SyncResult::AuthFailure) => {
                auth_failed = true;
                tracing::error!("Cloud sync: authentication failed (401)");
                continue; // Skip normal sleep, handled at loop top
            }
            Ok(SyncResult::SchemaMismatch(mismatch)) => {
                schema_mismatch = true;
                // #756: log the server's actual body and our classification
                // so on-call doesn't have to guess whether budi or the
                // cloud is the lagging side.
                tracing::error!(
                    kind = ?mismatch.kind,
                    "Cloud sync: server returned 422: {}",
                    mismatch.body,
                );
                continue; // Skip normal sleep, handled at loop top
            }
            Ok(SyncResult::TransientError(msg)) => {
                consecutive_failures += 1;
                let backoff = cloud_sync::backoff_delay(consecutive_failures, retry_max);
                tracing::warn!(
                    attempt = consecutive_failures,
                    backoff_s = backoff.as_secs(),
                    "Cloud sync transient error: {msg}"
                );
                tokio::time::sleep(backoff).await;
                continue; // Retry after backoff, don't add interval
            }
            Err(e) => {
                // spawn_blocking task panicked
                consecutive_failures += 1;
                tracing::error!("Cloud sync task panicked: {e}");
            }
        }

        // Normal interval sleep
        tokio::time::sleep(interval).await;
    }
}

/// #560: detect whether the credentials a sync tick would use just
/// changed on disk. Used by the worker loop to decide whether a fresh
/// `cloud.toml` read should clear an in-flight auth-failed state.
///
/// Returns `true` when the api_key the next tick would send differs
/// from what the previous tick sent, OR when the endpoint changed
/// (a rare but real path: self-hosted users swapping clouds). Returns
/// `false` if the previous tick had no api_key — in that case there's
/// no "rotation" to celebrate, the worker was simply waiting for a key
/// to be added, and the regular `auth_failed` clear-on-success path
/// will surface the recovery once a tick succeeds.
fn credentials_changed(
    prev_api_key: Option<&str>,
    prev_endpoint: &str,
    fresh: &CloudConfig,
) -> bool {
    let Some(prev_key) = prev_api_key else {
        return false;
    };
    let fresh_key = match fresh.effective_api_key() {
        Some(k) => k,
        None => return false,
    };
    if fresh_key != prev_key {
        return true;
    }
    fresh.effective_endpoint() != prev_endpoint
}

/// RAII guard that clears the `cloud_syncing` busy flag on drop.
///
/// Shared between the background worker and the manual `POST /cloud/sync`
/// route so a panic inside either sync path still releases the flag. Without
/// this guard, a panicking `sync_tick` would leave `cloud_syncing = true`
/// forever — every subsequent background tick would log "another sync
/// already in progress" and every manual flush would return 409 Conflict
/// until the daemon restarted.
pub(crate) struct CloudBusyFlagGuard {
    flag: Arc<AtomicBool>,
}

impl CloudBusyFlagGuard {
    pub(crate) fn new(flag: Arc<AtomicBool>) -> Self {
        Self { flag }
    }
}

impl Drop for CloudBusyFlagGuard {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_clears_flag_on_normal_drop() {
        let flag = Arc::new(AtomicBool::new(true));
        {
            let _guard = CloudBusyFlagGuard::new(flag.clone());
        }
        assert!(!flag.load(Ordering::SeqCst));
    }

    #[test]
    fn guard_clears_flag_on_panic_unwind() {
        // Simulates `sync_tick` panicking inside `spawn_blocking`: the
        // worker would otherwise leave `cloud_syncing` stuck at `true`.
        let flag = Arc::new(AtomicBool::new(true));
        let flag_in = flag.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = CloudBusyFlagGuard::new(flag_in);
            panic!("sync_tick blew up");
        }));
        assert!(result.is_err(), "expected the closure to panic");
        assert!(
            !flag.load(Ordering::SeqCst),
            "guard must reset the cloud_syncing flag even on panic"
        );
    }

    fn cfg_with(api_key: &str, endpoint: &str) -> CloudConfig {
        CloudConfig {
            api_key: Some(api_key.to_string()),
            endpoint: endpoint.to_string(),
            ..CloudConfig::default()
        }
    }

    #[test]
    fn credentials_changed_detects_rotated_api_key() {
        // #560 happy path: user ran `budi cloud init --force` with a new
        // key. The worker must spot that and clear auth_failed instead
        // of looping on the stale captured key forever.
        let fresh = cfg_with("budi_NEW", "https://app.getbudi.dev");
        assert!(credentials_changed(
            Some("budi_OLD"),
            "https://app.getbudi.dev",
            &fresh
        ));
    }

    #[test]
    fn credentials_changed_detects_swapped_endpoint() {
        // Self-hosted users sometimes swap which cloud the daemon talks
        // to without rotating the api_key. Treat that as a credential
        // change too — the auth-failed state was tied to the previous
        // (endpoint, api_key) pair.
        let fresh = cfg_with("budi_KEY", "https://cloud.example.com");
        assert!(credentials_changed(
            Some("budi_KEY"),
            "https://app.getbudi.dev",
            &fresh
        ));
    }

    #[test]
    fn credentials_changed_returns_false_when_unchanged() {
        // Most common path: cloud.toml wasn't touched between ticks.
        // Don't fire a "credentials changed" log every retry.
        let fresh = cfg_with("budi_KEY", "https://app.getbudi.dev");
        assert!(!credentials_changed(
            Some("budi_KEY"),
            "https://app.getbudi.dev",
            &fresh
        ));
    }

    #[test]
    fn credentials_changed_returns_false_when_no_previous_key() {
        // First-tick / cold-start path: the worker had no key to send
        // yet, so a new key showing up on disk isn't a "rotation"
        // recovery — the normal sync path will pick it up and clear any
        // auth-failed state on its own.
        let fresh = cfg_with("budi_KEY", "https://app.getbudi.dev");
        assert!(!credentials_changed(
            None,
            "https://app.getbudi.dev",
            &fresh
        ));
    }

    #[test]
    fn credentials_changed_returns_false_when_fresh_key_missing() {
        // User edited cloud.toml mid-flight and removed the api_key
        // (or commented it out). That's a config-degradation, not a
        // recovery — the next sync tick will just AuthFailure and we
        // re-enter the auth-failed loop with the same warning.
        let fresh = CloudConfig {
            endpoint: "https://app.getbudi.dev".to_string(),
            ..CloudConfig::default()
        };
        assert!(fresh.api_key.is_none());
        assert!(!credentials_changed(
            Some("budi_KEY"),
            "https://app.getbudi.dev",
            &fresh
        ));
    }
}
