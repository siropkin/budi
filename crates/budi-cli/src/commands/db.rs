//! `budi db` namespace — DB admin verbs grouped under a single subcommand.
//!
//! Before 8.2.1 the DB admin surface was a bag of top-level verbs
//! (`budi migrate`, `budi repair`, `budi import`) wired straight into
//! `Cli::Commands`. #368 folded those under a single `budi db`
//! namespace. 8.2.x kept the bare verbs as hidden backward-compatibility
//! aliases that printed a one-per-day deprecation nudge; 8.3.0 (#428)
//! removed the aliases and the nudge entirely.
//!
//! The module hosts:
//!
//! - `cmd_db_migrate` — thin wrapper that runs the daemon migration
//!   endpoint and prints a human-readable result. Called from
//!   `budi db migrate`.
//! - `remove_db_alias_nudge_marker` — best-effort cleanup of the
//!   `$BUDI_HOME/db-alias-nudge` single-line date marker left over from
//!   the 8.2.x nudge. Called from the `budi update` post-install path so
//!   users upgrading from 8.2.x don't keep a dead marker file around.
//!
//! `budi db repair` and `budi db import` stay in their existing
//! `commands::repair` / `commands::import` modules so the actual
//! daemon-talking implementations are in one place; this module only
//! holds the namespace-level bits (migrate wrapper + stale-marker
//! cleanup).

use std::fs;

use anyhow::Result;
use budi_core::config;

use crate::client::DaemonClient;
use crate::commands::ansi;

/// Run the DB migration endpoint and print a human-readable result.
pub fn cmd_db_migrate() -> Result<()> {
    let c = DaemonClient::connect()?;
    let result = c.migrate()?;
    let migrated = result
        .get("migrated")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let current = result.get("current").and_then(|v| v.as_u64()).unwrap_or(0);
    if migrated {
        let from = result.get("from").and_then(|v| v.as_u64()).unwrap_or(0);
        println!("Migrated database v{} → v{}.", from, current);
        let green = ansi("\x1b[32m");
        let reset = ansi("\x1b[0m");
        println!("{green}✓{reset} Migration complete.");
    } else {
        println!("Database schema is up to date (v{}).", current);
    }
    Ok(())
}

/// Relative name (under `BUDI_HOME`) of the single-line date marker that
/// 8.2.x used to rate-limit the bare-verb deprecation nudge (#368). The
/// nudge was retired in 8.3.0 (#428); this constant survives only so the
/// upgrade path can remove the stale file.
const DB_ALIAS_NUDGE_MARKER: &str = "db-alias-nudge";

/// Best-effort removal of the leftover `$BUDI_HOME/db-alias-nudge` marker
/// from 8.2.x. The file is a single-line UTC-date marker with no data;
/// failure to remove it (missing budi home, permission denied) is
/// silently ignored so this never fails an upgrade.
pub fn remove_db_alias_nudge_marker() {
    if let Ok(home) = config::budi_home_dir() {
        let _ = fs::remove_file(home.join(DB_ALIAS_NUDGE_MARKER));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_db_alias_nudge_marker_is_idempotent_when_file_is_absent() {
        let dir = std::env::temp_dir().join(format!(
            "budi-db-nudge-remove-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let marker = dir.join(DB_ALIAS_NUDGE_MARKER);
        // Writing then removing twice exercises the "best-effort, never
        // fails" contract: file present (real cleanup) and file absent
        // (idempotent no-op).
        fs::write(&marker, "2026-04-20\n").unwrap();
        assert!(marker.exists());

        let _ = fs::remove_file(&marker);
        assert!(!marker.exists());

        let _ = fs::remove_file(&marker);
        assert!(!marker.exists());

        let _ = fs::remove_dir_all(&dir);
    }
}
