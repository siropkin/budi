//! Background cloud sync worker.
//!
//! Runs on a configurable interval (default 300s per ADR-0083 §9).
//! Pushes scrubbed daily rollups and session summaries to the cloud ingest API.
//! Never blocks terminal execution.

use std::path::PathBuf;
use std::time::Duration;

use budi_core::cloud_sync::{self, SyncResult};
use budi_core::config::CloudConfig;

/// Run the cloud sync worker loop. Designed to be spawned with `tokio::spawn`.
pub async fn run(db_path: PathBuf, config: CloudConfig) {
    let interval = Duration::from_secs(config.sync.interval_seconds);
    let retry_max = config.sync.retry_max_seconds;
    let mut consecutive_failures: u32 = 0;
    let mut auth_failed = false;
    let mut schema_mismatch = false;

    loop {
        // If auth failed, stop syncing (ADR-0083 §4: "stop syncing and prompt re-auth")
        if auth_failed {
            tracing::warn!(
                "Cloud sync stopped: authentication failed. \
                 Check api_key in ~/.config/budi/cloud.toml."
            );
            // Sleep long and re-check config in case user re-authenticates
            tokio::time::sleep(Duration::from_secs(retry_max)).await;
            let fresh_config = budi_core::config::load_cloud_config();
            if fresh_config.is_ready() {
                auth_failed = false;
                tracing::info!("Cloud config refreshed, resuming sync");
            }
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

        // Normal sync tick
        let db = db_path.clone();
        let cfg = config.clone();
        let result = tokio::task::spawn_blocking(move || cloud_sync::sync_tick(&db, &cfg)).await;

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
            Ok(SyncResult::SchemaMismatch(msg)) => {
                schema_mismatch = true;
                tracing::error!("Cloud sync: schema mismatch (422): {msg}");
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
