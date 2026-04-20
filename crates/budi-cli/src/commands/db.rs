//! `budi db` namespace — DB admin verbs grouped under a single subcommand.
//!
//! Before 8.2.1 the DB admin surface was a bag of top-level verbs
//! (`budi migrate`, `budi repair`, `budi import`) wired straight into
//! `Cli::Commands`. That was the one surviving CLI layout outlier after
//! the 8.1 `budi autostart` / `budi integrations` / `budi cloud`
//! namespace work (R2.1, #339) and the 8.2.1 `budi vitals` rename
//! (#367). #368 folds the three DB admin verbs under a single `budi db`
//! namespace so newcomers reading `--help` see one DB admin surface
//! instead of three unrelated top-level verbs.
//!
//! The module hosts:
//!
//! - `cmd_db_migrate` — thin wrapper that runs the daemon migration
//!   endpoint and prints the same result text the old inline
//!   `Commands::Migrate` arm printed. Called from both `budi db migrate`
//!   (canonical) and the hidden `budi migrate` alias.
//! - `nudge_db_alias` — one-per-day stderr hint emitted when a legacy
//!   bare verb is invoked. Mirrors the `budi health` → `budi vitals`
//!   deprecation nudge from `commands::vitals::nudge_health_alias`
//!   (#367) so the deprecation cadence stays identical across the two
//!   8.2.1 CLI renames.
//!
//! `budi db repair` and `budi db import` stay in their existing
//! `commands::repair` / `commands::import` modules so the actual
//! daemon-talking implementations are in one place; this module only
//! holds the namespace-level bits (migrate wrapper + nudge + marker
//! file name).

use std::fs;
use std::io;
use std::path::PathBuf;

use anyhow::Result;
use budi_core::config;
use chrono::Utc;

use crate::client::DaemonClient;
use crate::commands::ansi;

/// Run the DB migration endpoint and print the same human-readable
/// result that `Commands::Migrate` printed inline before #368. Shared
/// by the canonical `budi db migrate` command and the deprecated
/// `budi migrate` alias so both entry points stay byte-identical in
/// their output.
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

/// Relative name (under `BUDI_HOME`) of the marker file that remembers
/// the last UTC date on which we emitted the bare-verb →
/// `budi db <verb>` deprecation nudge. One marker per day keeps the
/// hint visible without spamming every invocation (mirrors the
/// statusline legacy-token nudge in #345 and the `budi health` nudge
/// in #367).
const DB_ALIAS_NUDGE_MARKER: &str = "db-alias-nudge";

fn db_alias_marker_path() -> Option<PathBuf> {
    config::budi_home_dir()
        .ok()
        .map(|d| d.join(DB_ALIAS_NUDGE_MARKER))
}

/// Emit a one-per-day stderr nudge telling the caller that `budi migrate`
/// / `budi repair` / `budi import` have moved under `budi db`. `verb` is
/// the short new name (`migrate`, `repair`, or `import`) so the hint
/// points the user at the exact replacement command rather than a
/// generic "use the new namespace" message.
///
/// Filesystem errors are swallowed so a CLI invocation never fails just
/// because the marker file couldn't be written.
pub fn nudge_db_alias(verb: &str) {
    nudge_db_alias_inner(verb, db_alias_marker_path, &mut io::stderr());
}

fn nudge_db_alias_inner(
    verb: &str,
    marker_path: impl FnOnce() -> Option<PathBuf>,
    sink: &mut dyn io::Write,
) {
    let today = Utc::now().format("%Y-%m-%d").to_string();
    let marker = marker_path();

    if let Some(ref path) = marker
        && let Ok(existing) = fs::read_to_string(path)
        && existing.trim() == today
    {
        return;
    }

    let _ = writeln!(
        sink,
        "budi: `budi {verb}` has moved to `budi db {verb}` and the bare \
         verb will be removed in 8.3. Switch to `budi db {verb}` to \
         silence this notice."
    );

    if let Some(path) = marker {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(&path, format!("{today}\n"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nudge_db_alias_writes_once_per_day() {
        let dir = std::env::temp_dir().join(format!(
            "budi-db-nudge-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let _ = fs::remove_dir_all(&dir);
        let marker = dir.join(DB_ALIAS_NUDGE_MARKER);
        let marker_fn = || Some(marker.clone());

        let mut first = Vec::<u8>::new();
        nudge_db_alias_inner("migrate", marker_fn, &mut first);
        let first_text = String::from_utf8(first).unwrap();
        assert!(
            first_text.contains("`budi migrate` has moved to `budi db migrate`"),
            "first invocation should nudge with the exact replacement verb, got {first_text:?}"
        );
        assert!(marker.exists(), "marker should be written after nudging");
        let stored = fs::read_to_string(&marker).unwrap();
        assert_eq!(stored.trim(), Utc::now().format("%Y-%m-%d").to_string());

        let mut second = Vec::<u8>::new();
        nudge_db_alias_inner("repair", marker_fn, &mut second);
        assert!(
            second.is_empty(),
            "second invocation on the same day should stay quiet (shared marker across verbs)"
        );

        fs::write(&marker, "1970-01-01\n").unwrap();
        let mut third = Vec::<u8>::new();
        nudge_db_alias_inner("import", marker_fn, &mut third);
        let third_text = String::from_utf8(third).unwrap();
        assert!(
            third_text.contains("`budi import` has moved to `budi db import`"),
            "stale marker should let the nudge fire again, got {third_text:?}"
        );
        let refreshed = fs::read_to_string(&marker).unwrap();
        assert_eq!(refreshed.trim(), Utc::now().format("%Y-%m-%d").to_string());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn nudge_db_alias_survives_missing_marker_dir() {
        let dir = std::env::temp_dir().join(format!(
            "budi-db-nudge-mkdir-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let _ = fs::remove_dir_all(&dir);
        let marker = dir.join("nested").join(DB_ALIAS_NUDGE_MARKER);
        let mut out = Vec::<u8>::new();
        nudge_db_alias_inner("migrate", || Some(marker.clone()), &mut out);
        assert!(!out.is_empty());
        assert!(marker.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn nudge_db_alias_without_budi_home_still_nudges() {
        let mut out = Vec::<u8>::new();
        nudge_db_alias_inner("repair", || None, &mut out);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("`budi repair` has moved to `budi db repair`"));
    }
}
