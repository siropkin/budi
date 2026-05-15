//! `budi db` namespace — DB admin verbs grouped under a single subcommand.
//!
//! Before 8.2.1 the DB admin surface was a bag of top-level verbs
//! (`budi migrate`, `budi repair`, `budi import`) wired straight into
//! `Cli::Commands`. #368 folded those under a single `budi db`
//! namespace. 8.2.x kept the bare verbs as hidden backward-compatibility
//! aliases that printed a one-per-day deprecation nudge; 8.3.0 (#428)
//! removed the aliases and the nudge entirely. 8.3.14 (#586) collapsed
//! `db migrate` and `db repair` into a single `db check [--fix]` verb
//! matching the `git fsck` / `cargo check` convention: read-only by
//! default, opt-in repair via `--fix`.
//!
//! The module hosts:
//!
//! - `cmd_db_check` — runs the daemon's `/admin/check` (read-only) when
//!   `fix=false`, or `/admin/repair` when `fix=true`. Renders the same
//!   `RepairReport` shape either way.
//! - `remove_db_alias_nudge_marker` — best-effort cleanup of the
//!   `$BUDI_HOME/db-alias-nudge` single-line date marker left over from
//!   the 8.2.x nudge. Called from the `budi update` post-install path so
//!   users upgrading from 8.2.x don't keep a dead marker file around.
//!
//! `budi db import` stays in `commands::import` so the daemon-talking
//! implementation lives in one place; this module only holds the
//! namespace-level bits (check wrapper + stale-marker cleanup).

use std::fs;

use anyhow::Result;
use budi_core::config;
use serde_json::Value;

use crate::client::DaemonClient;
use crate::commands::ansi;

/// Run schema diagnostic (read-only) or schema repair (`fix=true`).
///
/// Read-only mode reports drift via the structured payload from
/// `/admin/check`. If drift is present the process exits non-zero so
/// scripts (`budi db check && deploy`) can branch on the result. Repair
/// mode (`/admin/repair`) is the old `db repair` behaviour: applies the
/// migration, fixes additive drift, and prints a green ✓.
pub(crate) fn cmd_db_check(fix: bool) -> Result<()> {
    let client = DaemonClient::connect()?;
    let result = if fix {
        client.repair()?
    } else {
        client.check()?
    };

    let from = result
        .get("from_version")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let to = result
        .get("to_version")
        .and_then(Value::as_u64)
        .unwrap_or(from);
    let migrated = result
        .get("migrated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let added_columns = string_list(&result, "added_columns");
    let added_indexes = string_list(&result, "added_indexes");
    let removed_tables = string_list(&result, "removed_tables");
    let drift_present =
        !added_columns.is_empty() || !added_indexes.is_empty() || !removed_tables.is_empty();

    if fix {
        if migrated {
            println!("Migrated database v{from} -> v{to}.");
        } else {
            println!("Database schema version is v{to}.");
        }

        if drift_present {
            println!("Repaired schema drift:");
            print_drift(&added_columns, &added_indexes, &removed_tables);
        } else {
            println!("No schema drift detected.");
        }

        let green = ansi("\x1b[32m");
        let reset = ansi("\x1b[0m");
        println!("{green}✓{reset} Repair complete.");
        return Ok(());
    }

    // Read-only diagnostic.
    if migrated {
        println!("Schema is v{from}; binary expects v{to}. Run `budi db check --fix` to upgrade.");
        anyhow::bail!("schema migration required");
    }

    if drift_present {
        println!("Schema drift detected:");
        print_drift(&added_columns, &added_indexes, &removed_tables);
        println!("Run `budi db check --fix` to repair.");
        anyhow::bail!("schema drift detected");
    }

    println!("Database schema is up to date (v{to}).");
    Ok(())
}

fn string_list(result: &Value, key: &str) -> Vec<String> {
    result
        .get(key)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(ToOwned::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn print_drift(added_columns: &[String], added_indexes: &[String], removed_tables: &[String]) {
    if !added_columns.is_empty() {
        println!("Missing columns / tables / triggers:");
        for col in added_columns {
            println!("  - {col}");
        }
    }
    if !added_indexes.is_empty() {
        println!("Missing indexes:");
        for idx in added_indexes {
            println!("  - {idx}");
        }
    }
    if !removed_tables.is_empty() {
        println!("Obsolete tables:");
        for table in removed_tables {
            println!("  - {table}");
        }
    }
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
pub(crate) fn remove_db_alias_nudge_marker() {
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
