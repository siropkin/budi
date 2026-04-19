use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use budi_core::config;
use budi_core::provider::Provider;
use chrono::{DateTime, Local, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::daemon::{daemon_health, ensure_daemon_running, resolve_daemon_binary};

pub fn cmd_doctor(repo_root: Option<PathBuf>, deep: bool) -> Result<()> {
    let repo_root = super::try_resolve_repo_root(repo_root);
    let config = match &repo_root {
        Some(root) => config::load_or_default(root)?,
        None => config::BudiConfig::default(),
    };

    let dim = super::ansi("\x1b[90m");
    let reset = super::ansi("\x1b[0m");

    if let Some(ref root) = repo_root {
        println!("budi doctor - {}", root.display());
    } else {
        println!("budi doctor - global mode");
    }
    println!();

    let mut report = DoctorReport::default();

    let daemon = check_daemon_health(repo_root.as_deref(), &config);
    daemon.result.print();
    report.record(&daemon.result);

    if daemon.started_this_run {
        // The daemon seeds tail offsets on startup before the backstop loop
        // begins. A short pause keeps `doctor` from racing that initial
        // bookkeeping on a cold start.
        std::thread::sleep(Duration::from_millis(750));
    }

    let db_path = budi_core::analytics::db_path()?;
    let schema = check_schema(&db_path, deep);
    schema.result.print();
    report.record(&schema.result);

    let proxy_residue = check_legacy_proxy_env();
    proxy_residue.print();
    report.record(&proxy_residue);

    if let Some(conn) = schema.conn.as_ref() {
        let legacy_proxy_history = check_legacy_proxy_history(conn);
        legacy_proxy_history.print();
        report.record(&legacy_proxy_history);
    }

    let providers = doctor_providers();
    if providers.is_empty() {
        let no_providers = CheckResult::warn(
            "tailer providers",
            "no enabled transcript providers are visible on disk yet",
            Some(
                "Open your agent once so it creates its local transcript directory, then rerun `budi doctor`."
                    .to_string(),
            ),
        );
        no_providers.print();
        report.record(&no_providers);
    } else {
        let conn = schema.conn.as_ref();
        for provider in &providers {
            let diag = gather_provider_doctor_data(conn, provider.as_ref());

            let tailer = summarize_tailer_health(&diag);
            tailer.print();
            report.record(&tailer);

            let visibility = summarize_transcript_visibility(&diag);
            visibility.print();
            report.record(&visibility);
        }
    }

    println!();
    if report.fails == 0 && report.warns == 0 {
        println!("All checks passed.");
        return Ok(());
    }

    if report.fails == 0 {
        println!("Doctor finished with {} warning(s).", report.warns);
        println!(
            "  {dim}Warnings are informational cleanup or readiness hints; Budi should still be able to collect live data where the PASS checks say it can.{reset}"
        );
        return Ok(());
    }

    anyhow::bail!(
        "doctor found {} failing check(s) and {} warning(s)",
        report.fail_count(),
        report.warn_count()
    );
}

#[derive(Debug, Default)]
struct DoctorReport {
    warns: usize,
    fails: usize,
}

impl DoctorReport {
    fn record(&mut self, result: &CheckResult) {
        match result.state {
            CheckState::Pass => {}
            CheckState::Warn => self.warns += 1,
            CheckState::Fail => self.fails += 1,
        }
    }

    fn warn_count(&self) -> usize {
        self.warns
    }

    fn fail_count(&self) -> usize {
        self.fails
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckState {
    Pass,
    Warn,
    Fail,
}

impl CheckState {
    fn label(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Warn => "WARN",
            Self::Fail => "FAIL",
        }
    }

    fn color(self) -> &'static str {
        match self {
            Self::Pass => "\x1b[32m",
            Self::Warn => "\x1b[33m",
            Self::Fail => "\x1b[31m",
        }
    }
}

#[derive(Debug, Clone)]
struct CheckResult {
    state: CheckState,
    label: String,
    detail: String,
    fix: Option<String>,
}

impl CheckResult {
    fn pass(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            state: CheckState::Pass,
            label: label.into(),
            detail: detail.into(),
            fix: None,
        }
    }

    fn warn(label: impl Into<String>, detail: impl Into<String>, fix: Option<String>) -> Self {
        Self {
            state: CheckState::Warn,
            label: label.into(),
            detail: detail.into(),
            fix,
        }
    }

    fn fail(label: impl Into<String>, detail: impl Into<String>, fix: Option<String>) -> Self {
        Self {
            state: CheckState::Fail,
            label: label.into(),
            detail: detail.into(),
            fix,
        }
    }

    fn print(&self) {
        let color = super::ansi(self.state.color());
        let reset = super::ansi("\x1b[0m");
        println!(
            "  {color}{}{reset} {}: {}",
            self.state.label(),
            self.label,
            self.detail
        );
        if let Some(ref fix) = self.fix {
            println!("       fix: {fix}");
        }
    }
}

struct DaemonCheck {
    result: CheckResult,
    started_this_run: bool,
}

fn check_daemon_health(repo_root: Option<&Path>, config: &config::BudiConfig) -> DaemonCheck {
    let base_url = config.daemon_base_url();
    let daemon_bin_override = std::env::var_os("BUDI_DAEMON_BIN");
    if daemon_health(config) {
        return DaemonCheck {
            result: CheckResult::pass("daemon health", format!("responding on {base_url}")),
            started_this_run: false,
        };
    }

    let daemon_bin = match resolve_daemon_binary() {
        Ok(path) => path,
        Err(e) => {
            return DaemonCheck {
                result: CheckResult::fail(
                    "daemon health",
                    format!("daemon is not responding on {base_url} and the daemon binary could not be resolved ({e})"),
                    Some(
                        "Install `budi` and `budi-daemon` from the same build, or set `BUDI_DAEMON_BIN` to the daemon binary path."
                            .to_string(),
                    ),
                ),
                started_this_run: false,
            };
        }
    };

    match ensure_daemon_running(repo_root, config) {
        Ok(()) if daemon_health(config) => DaemonCheck {
            result: CheckResult::pass(
                "daemon health",
                format!(
                    "started successfully and is responding on {base_url} (binary: {})",
                    daemon_bin.display()
                ),
            ),
            started_this_run: true,
        },
        Ok(()) => DaemonCheck {
            result: CheckResult::fail(
                "daemon health",
                format!(
                    "attempted to start {base_url} via {}, but the daemon still did not answer",
                    daemon_bin.display()
                ),
                Some(
                    "Inspect the daemon log under `~/.local/share/budi/logs/daemon.log`, then rerun `budi init`."
                        .to_string(),
                ),
            ),
            started_this_run: true,
        },
        Err(e) => DaemonCheck {
            result: CheckResult::fail(
                "daemon health",
                format!(
                    "daemon is not responding on {base_url}; restart via {}{} failed ({e})",
                    daemon_bin.display(),
                    if daemon_bin_override.is_some() {
                        " (from BUDI_DAEMON_BIN)"
                    } else {
                        ""
                    }
                ),
                Some(
                    "Inspect the daemon log under `~/.local/share/budi/logs/daemon.log`, fix the startup error, then rerun `budi init`."
                        .to_string(),
                ),
            ),
            started_this_run: false,
        },
    }
}

struct SchemaCheck {
    result: CheckResult,
    conn: Option<Connection>,
}

fn check_schema(db_path: &Path, deep: bool) -> SchemaCheck {
    if !db_path.exists() {
        return SchemaCheck {
            result: CheckResult::warn(
                "schema drift",
                format!(
                    "analytics database does not exist yet at {}",
                    db_path.display()
                ),
                Some(
                    "Run `budi init` to create the schema before expecting live ingestion."
                        .to_string(),
                ),
            ),
            conn: None,
        };
    }

    let conn = match budi_core::analytics::open_db(db_path) {
        Ok(conn) => conn,
        Err(e) => {
            return SchemaCheck {
                result: CheckResult::fail(
                    "schema drift",
                    format!(
                        "analytics database exists at {} but could not be opened ({e})",
                        db_path.display()
                    ),
                    Some(
                        "Repair or remove the broken database, then rerun `budi init`.".to_string(),
                    ),
                ),
                conn: None,
            };
        }
    };

    let current = budi_core::migration::current_version(&conn);
    let target = budi_core::migration::SCHEMA_VERSION;
    if current != target {
        return SchemaCheck {
            result: CheckResult::fail(
                "schema drift",
                format!("database schema is v{current}; this build expects v{target}"),
                Some("Run `budi init` or `budi update` so the current binary can migrate the database.".to_string()),
            ),
            conn: Some(conn),
        };
    }

    let pragma = integrity_check_pragma(deep);
    let mode = integrity_check_mode_label(deep);
    match conn.query_row(pragma, [], |row| row.get::<_, String>(0)) {
        Ok(result) if result == "ok" => SchemaCheck {
            result: CheckResult::pass(
                "schema drift",
                format!("schema v{target} at {} ({mode} ok)", db_path.display()),
            ),
            conn: Some(conn),
        },
        Ok(result) => SchemaCheck {
            result: CheckResult::fail(
                "schema drift",
                format!("{mode} returned `{result}`"),
                Some(
                    "Run `budi repair` after backing up the database, then rerun `budi doctor`."
                        .to_string(),
                ),
            ),
            conn: Some(conn),
        },
        Err(e) => SchemaCheck {
            result: CheckResult::fail(
                "schema drift",
                format!("could not run {mode} on {} ({e})", db_path.display()),
                Some("Run `budi repair` or recreate the database with `budi init`.".to_string()),
            ),
            conn: Some(conn),
        },
    }
}

fn integrity_check_pragma(deep: bool) -> &'static str {
    if deep {
        "PRAGMA integrity_check"
    } else {
        "PRAGMA quick_check"
    }
}

fn integrity_check_mode_label(deep: bool) -> &'static str {
    if deep {
        "integrity_check"
    } else {
        "quick_check"
    }
}

fn check_legacy_proxy_env() -> CheckResult {
    let residue = legacy_proxy_env_vars();
    if residue.is_empty() {
        return CheckResult::pass(
            "leftover proxy config",
            "no legacy proxy-routing env vars are exported",
        );
    }

    let rendered = residue
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(", ");

    CheckResult::warn(
        "leftover proxy config",
        format!("legacy proxy env vars are still exported ({rendered})"),
        Some(
            "Run `budi init --cleanup` to review and remove old 8.0/8.1 proxy residue with explicit consent."
                .to_string(),
        ),
    )
}

fn legacy_proxy_env_vars() -> Vec<(String, String)> {
    [
        "ANTHROPIC_BASE_URL",
        "OPENAI_BASE_URL",
        "COPILOT_PROVIDER_BASE_URL",
    ]
    .into_iter()
    .filter_map(|key| {
        let value = std::env::var(key).ok()?;
        looks_like_legacy_proxy_value(&value).then_some((key.to_string(), value))
    })
    .collect()
}

fn looks_like_legacy_proxy_value(value: &str) -> bool {
    let lower = value.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return false;
    }
    lower.contains("localhost:9878")
        || lower.contains("127.0.0.1:9878")
        || lower.contains("[::1]:9878")
}

#[derive(Debug, Clone, Default)]
struct LegacyProxyHistoryData {
    retained_assistant_messages: usize,
    oldest_message: Option<DateTime<Utc>>,
    newest_message: Option<DateTime<Utc>>,
    proxy_events_table_present: bool,
}

fn check_legacy_proxy_history(conn: &Connection) -> CheckResult {
    match load_legacy_proxy_history(conn) {
        Ok(data) => summarize_legacy_proxy_history(&data),
        Err(e) => CheckResult::fail(
            "legacy proxy history",
            format!("could not inspect retained 8.1 proxy-era data ({e})"),
            Some("Run `budi repair`, then rerun `budi doctor`.".to_string()),
        ),
    }
}

fn load_legacy_proxy_history(conn: &Connection) -> Result<LegacyProxyHistoryData> {
    let proxy_events_table_present = sqlite_table_exists(conn, "proxy_events")?;
    let (retained_assistant_messages, oldest_message, newest_message) = conn
        .query_row(
            "SELECT
                COUNT(*),
                MIN(timestamp),
                MAX(timestamp)
             FROM messages
             WHERE role = 'assistant'
               AND cost_confidence = 'proxy_estimated'",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .context("failed to read proxy_estimated assistant rows")?;

    Ok(LegacyProxyHistoryData {
        retained_assistant_messages: retained_assistant_messages.max(0) as usize,
        oldest_message: oldest_message.and_then(|value| parse_rfc3339_utc(&value)),
        newest_message: newest_message.and_then(|value| parse_rfc3339_utc(&value)),
        proxy_events_table_present,
    })
}

fn sqlite_table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let count = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = ?1",
        params![table],
        |row| row.get::<_, i64>(0),
    )?;
    Ok(count > 0)
}

fn summarize_legacy_proxy_history(data: &LegacyProxyHistoryData) -> CheckResult {
    let label = "legacy proxy history";
    let row_word = if data.retained_assistant_messages == 1 {
        "row"
    } else {
        "rows"
    };

    let retained_detail = if data.retained_assistant_messages == 0 {
        "no retained 8.1 proxy-sourced assistant history remains".to_string()
    } else {
        format!(
            "retaining {} proxy-sourced assistant {} read-only (oldest {}, newest {}); newer live data comes from transcript tailing and legacy attribution may be weaker",
            data.retained_assistant_messages,
            row_word,
            format_timestamp_local(data.oldest_message),
            format_timestamp_local(data.newest_message)
        )
    };

    if data.proxy_events_table_present {
        return CheckResult::warn(
            label,
            format!("{retained_detail}; obsolete `proxy_events` table is still present"),
            Some(
                "Run `budi init` or `budi repair` with the current 8.2 build to remove the old `proxy_events` table."
                    .to_string(),
            ),
        );
    }

    CheckResult::pass(label, retained_detail)
}

fn doctor_providers() -> Vec<Box<dyn Provider>> {
    match budi_core::config::load_agents_config() {
        Some(cfg) => budi_core::provider::all_providers()
            .into_iter()
            .filter(|provider| cfg.is_agent_enabled(provider.name()))
            .collect(),
        None => budi_core::provider::available_providers(),
    }
}

#[derive(Debug, Clone)]
struct ProviderDoctorData {
    display_name: &'static str,
    watch_roots: Vec<PathBuf>,
    discovered_files: usize,
    latest_file: Option<PathBuf>,
    latest_file_len: Option<u64>,
    latest_file_mtime: Option<DateTime<Utc>>,
    tracked_offsets: Option<usize>,
    latest_tail_offset: Option<u64>,
    latest_tail_seen: Option<DateTime<Utc>>,
    discover_error: Option<String>,
    db_error: Option<String>,
}

fn gather_provider_doctor_data(
    conn: Option<&Connection>,
    provider: &dyn Provider,
) -> ProviderDoctorData {
    let watch_roots = provider.watch_roots();
    let mut data = ProviderDoctorData {
        display_name: provider.display_name(),
        watch_roots,
        discovered_files: 0,
        latest_file: None,
        latest_file_len: None,
        latest_file_mtime: None,
        tracked_offsets: None,
        latest_tail_offset: None,
        latest_tail_seen: None,
        discover_error: None,
        db_error: None,
    };

    match provider.discover_files() {
        Ok(files) => {
            data.discovered_files = files.len();
            if let Some(file) = files.first() {
                data.latest_file = Some(file.path.clone());
                data.latest_file_len = std::fs::metadata(&file.path).ok().map(|m| m.len());
                data.latest_file_mtime = std::fs::metadata(&file.path)
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .map(DateTime::<Utc>::from);
            }
        }
        Err(e) => {
            data.discover_error = Some(e.to_string());
        }
    }

    if let Some(conn) = conn {
        match load_tail_offset_count(conn, provider.name()) {
            Ok(count) => data.tracked_offsets = Some(count),
            Err(e) => data.db_error = Some(e.to_string()),
        }
        match load_latest_tail_seen(conn, provider.name()) {
            Ok(last_seen) => data.latest_tail_seen = last_seen,
            Err(e) if data.db_error.is_none() => data.db_error = Some(e.to_string()),
            Err(_) => {}
        }
        if let Some(ref latest_file) = data.latest_file {
            match load_tail_offset_state(conn, provider.name(), latest_file) {
                Ok(Some(state)) => {
                    data.latest_tail_offset = Some(state.byte_offset);
                    data.latest_tail_seen = data.latest_tail_seen.or(state.last_seen);
                }
                Ok(None) => {}
                Err(e) if data.db_error.is_none() => data.db_error = Some(e.to_string()),
                Err(_) => {}
            }
        }
    }

    data
}

#[derive(Debug, Clone)]
struct TailOffsetState {
    byte_offset: u64,
    last_seen: Option<DateTime<Utc>>,
}

fn load_tail_offset_count(conn: &Connection, provider: &str) -> Result<usize> {
    let count = conn
        .query_row(
            "SELECT COUNT(*) FROM tail_offsets WHERE provider = ?1",
            params![provider],
            |row| row.get::<_, i64>(0),
        )
        .context("failed to count tail_offsets rows")?;
    Ok(count.max(0) as usize)
}

fn load_latest_tail_seen(conn: &Connection, provider: &str) -> Result<Option<DateTime<Utc>>> {
    let last_seen: Option<String> = conn
        .query_row(
            "SELECT MAX(last_seen) FROM tail_offsets WHERE provider = ?1",
            params![provider],
            |row| row.get(0),
        )
        .context("failed to read last_seen from tail_offsets")?;
    Ok(last_seen.and_then(|value| parse_rfc3339_utc(&value)))
}

fn load_tail_offset_state(
    conn: &Connection,
    provider: &str,
    path: &Path,
) -> Result<Option<TailOffsetState>> {
    let path_str = path.display().to_string();
    let row = conn
        .query_row(
            "SELECT byte_offset, last_seen
             FROM tail_offsets
             WHERE provider = ?1 AND path = ?2",
            params![provider, path_str],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .optional()
        .context("failed to read tail offset state")?;

    Ok(row.map(|(offset, last_seen)| TailOffsetState {
        byte_offset: offset.max(0) as u64,
        last_seen: last_seen.and_then(|value| parse_rfc3339_utc(&value)),
    }))
}

fn summarize_tailer_health(diag: &ProviderDoctorData) -> CheckResult {
    let label = format!("tailer health / {}", diag.display_name);

    if let Some(ref error) = diag.discover_error {
        return CheckResult::fail(
            label,
            format!("could not discover transcript files ({error})"),
            Some("Check the provider's local transcript directory permissions, then rerun `budi doctor`.".to_string()),
        );
    }

    if diag.watch_roots.is_empty() {
        return CheckResult::warn(
            label,
            "no transcript watch roots exist on disk yet".to_string(),
            Some(format!(
                "Open {} once so it creates its local transcript directory, then rerun `budi doctor`.",
                diag.display_name
            )),
        );
    }

    if let Some(ref error) = diag.db_error {
        return CheckResult::fail(
            label,
            format!("watch roots exist, but tailer state could not be inspected ({error})"),
            Some("Fix the database or schema error above, then rerun `budi doctor`.".to_string()),
        );
    }

    if diag.discovered_files == 0 {
        return CheckResult::pass(
            label,
            format!(
                "watching {} root(s); no transcript files discovered yet",
                diag.watch_roots.len()
            ),
        );
    }

    match diag.tracked_offsets {
        Some(0) => CheckResult::fail(
            label,
            format!(
                "watching {} root(s) and found {} transcript file(s), but the tailer has not seeded any offsets",
                diag.watch_roots.len(),
                diag.discovered_files
            ),
            Some(
                "Keep `budi-daemon` running for a moment so it can seed tail offsets, then rerun `budi doctor`."
                    .to_string(),
            ),
        ),
        Some(count) => {
            let detail = if let Some(last_seen) = diag.latest_tail_seen {
                format!(
                    "watching {} root(s); tracking {count} transcript file(s); last tailer activity {} ago",
                    diag.watch_roots.len(),
                    format_relative_age(last_seen)
                )
            } else {
                format!(
                    "watching {} root(s); tracking {count} transcript file(s)",
                    diag.watch_roots.len()
                )
            };
            CheckResult::pass(label, detail)
        }
        None => CheckResult::fail(
            label,
            "watch roots exist, but no tailer offset summary is available".to_string(),
            Some("Fix the database or schema error above, then rerun `budi doctor`.".to_string()),
        ),
    }
}

fn summarize_transcript_visibility(diag: &ProviderDoctorData) -> CheckResult {
    let label = format!("transcript visibility / {}", diag.display_name);

    if let Some(ref error) = diag.discover_error {
        return CheckResult::fail(
            label,
            format!("could not discover transcript files ({error})"),
            Some("Check the provider's local transcript directory permissions, then rerun `budi doctor`.".to_string()),
        );
    }

    let Some(ref latest_file) = diag.latest_file else {
        return CheckResult::pass(label, "no transcript files discovered yet");
    };

    if !latest_file_is_from_today(diag.latest_file_mtime) {
        return CheckResult::pass(
            label,
            format!(
                "latest transcript is {} (last modified {}; no transcript activity today)",
                latest_file.display(),
                format_timestamp_local(diag.latest_file_mtime)
            ),
        );
    }

    if let Some(ref error) = diag.db_error {
        return CheckResult::fail(
            label,
            format!(
                "latest transcript is {} but tailer state could not be inspected ({error})",
                latest_file.display()
            ),
            Some("Fix the database or schema error above, then rerun `budi doctor`.".to_string()),
        );
    }

    let Some(file_len) = diag.latest_file_len else {
        return CheckResult::warn(
            label,
            format!(
                "latest transcript is {} but its file size could not be read",
                latest_file.display()
            ),
            Some(
                "Check that the transcript still exists and is readable, then rerun `budi doctor`."
                    .to_string(),
            ),
        );
    };

    let Some(offset) = diag.latest_tail_offset else {
        return CheckResult::fail(
            label,
            format!("latest transcript is {} and is not tracked by the tailer yet", latest_file.display()),
            Some(
                "Make sure `budi-daemon` is running so the tailer can seed this file, then rerun `budi doctor`. Run `budi import` if you also need older history backfilled."
                    .to_string(),
            ),
        );
    };

    let gap = file_len.saturating_sub(offset);
    if gap > 0 {
        return CheckResult::fail(
            label,
            format!(
                "latest transcript is {} and the tailer is {} B behind",
                latest_file.display(),
                gap
            ),
            Some(
                "Keep `budi-daemon` running and rerun `budi doctor`; if the gap persists, restart with `budi init`."
                    .to_string(),
            ),
        );
    }

    CheckResult::pass(
        label,
        format!(
            "latest transcript is {} and the tailer is caught up (0 B behind)",
            latest_file.display()
        ),
    )
}

fn latest_file_is_from_today(ts: Option<DateTime<Utc>>) -> bool {
    let Some(ts) = ts else {
        return false;
    };
    ts.with_timezone(&Local).date_naive() == Local::now().date_naive()
}

fn format_timestamp_local(ts: Option<DateTime<Utc>>) -> String {
    ts.map(|value| {
        value
            .with_timezone(&Local)
            .format("%Y-%m-%d %H:%M")
            .to_string()
    })
    .unwrap_or_else(|| "unknown".to_string())
}

fn format_relative_age(ts: DateTime<Utc>) -> String {
    let delta = Utc::now().signed_duration_since(ts);
    if delta.num_seconds() < 60 {
        format!("{}s", delta.num_seconds().max(0))
    } else if delta.num_minutes() < 60 {
        format!("{}m", delta.num_minutes())
    } else if delta.num_hours() < 48 {
        format!("{}h", delta.num_hours())
    } else {
        format!("{}d", delta.num_days())
    }
}

fn parse_rfc3339_utc(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diag(display_name: &'static str) -> ProviderDoctorData {
        ProviderDoctorData {
            display_name,
            watch_roots: vec![PathBuf::from("/tmp/watch")],
            discovered_files: 1,
            latest_file: Some(PathBuf::from("/tmp/watch/session.jsonl")),
            latest_file_len: Some(120),
            latest_file_mtime: Some(Utc::now()),
            tracked_offsets: Some(1),
            latest_tail_offset: Some(120),
            latest_tail_seen: Some(Utc::now()),
            discover_error: None,
            db_error: None,
        }
    }

    #[test]
    fn integrity_check_uses_quick_check_by_default() {
        assert_eq!(integrity_check_pragma(false), "PRAGMA quick_check");
        assert_eq!(integrity_check_mode_label(false), "quick_check");
    }

    #[test]
    fn integrity_check_uses_full_check_in_deep_mode() {
        assert_eq!(integrity_check_pragma(true), "PRAGMA integrity_check");
        assert_eq!(integrity_check_mode_label(true), "integrity_check");
    }

    #[test]
    fn detects_legacy_proxy_env_values() {
        assert!(looks_like_legacy_proxy_value("http://127.0.0.1:9878"));
        assert!(looks_like_legacy_proxy_value("http://localhost:9878/v1"));
        assert!(looks_like_legacy_proxy_value("http://[::1]:9878"));
        assert!(!looks_like_legacy_proxy_value("https://api.anthropic.com"));
    }

    #[test]
    fn legacy_proxy_history_passes_when_only_retained_messages_remain() {
        let result = summarize_legacy_proxy_history(&LegacyProxyHistoryData {
            retained_assistant_messages: 2,
            oldest_message: Some(Utc::now() - chrono::Duration::days(1)),
            newest_message: Some(Utc::now()),
            proxy_events_table_present: false,
        });

        assert_eq!(result.state, CheckState::Pass);
        assert!(
            result
                .detail
                .contains("retaining 2 proxy-sourced assistant rows")
        );
        assert!(result.detail.contains("transcript tailing"));
    }

    #[test]
    fn legacy_proxy_history_warns_when_proxy_events_table_is_still_present() {
        let result = summarize_legacy_proxy_history(&LegacyProxyHistoryData {
            retained_assistant_messages: 1,
            oldest_message: Some(Utc::now()),
            newest_message: Some(Utc::now()),
            proxy_events_table_present: true,
        });

        assert_eq!(result.state, CheckState::Warn);
        assert!(result.detail.contains("obsolete `proxy_events` table"));
        assert!(
            result
                .fix
                .as_deref()
                .unwrap_or_default()
                .contains("budi repair")
        );
    }

    #[test]
    fn legacy_proxy_history_loader_reads_proxy_rows_and_table_presence() {
        let conn = Connection::open_in_memory().unwrap();
        budi_core::migration::migrate(&conn).unwrap();
        conn.execute_batch(
            "
            CREATE TABLE proxy_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp TEXT NOT NULL
            );
            INSERT INTO messages (
                id, role, timestamp, model, provider, input_tokens, output_tokens,
                cache_creation_tokens, cache_read_tokens, cost_cents, cost_confidence
            ) VALUES (
                'legacy-proxy-row', 'assistant', '2026-04-19T17:00:00Z', 'gpt-4o',
                'openai', 1, 1, 0, 0, 0.5, 'proxy_estimated'
            );
            ",
        )
        .unwrap();

        let data = load_legacy_proxy_history(&conn).unwrap();

        assert_eq!(data.retained_assistant_messages, 1);
        assert!(data.proxy_events_table_present);
        assert_eq!(
            data.newest_message
                .expect("newest proxy timestamp should parse")
                .to_rfc3339(),
            "2026-04-19T17:00:00+00:00"
        );
    }

    #[test]
    fn transcript_visibility_fails_when_latest_file_is_untracked() {
        let mut data = diag("Claude Code");
        data.latest_tail_offset = None;

        let result = summarize_transcript_visibility(&data);

        assert_eq!(result.state, CheckState::Fail);
        assert!(result.detail.contains("not tracked by the tailer"));
        assert!(
            result
                .fix
                .as_deref()
                .unwrap_or_default()
                .contains("budi-daemon")
        );
    }

    #[test]
    fn transcript_visibility_reports_gap_bytes() {
        let mut data = diag("Claude Code");
        data.latest_tail_offset = Some(96);

        let result = summarize_transcript_visibility(&data);

        assert_eq!(result.state, CheckState::Fail);
        assert!(result.detail.contains("24 B behind"));
    }

    #[test]
    fn transcript_visibility_passes_when_no_activity_today() {
        let mut data = diag("Claude Code");
        data.latest_file_mtime = Some(Utc::now() - chrono::Duration::days(2));

        let result = summarize_transcript_visibility(&data);

        assert_eq!(result.state, CheckState::Pass);
        assert!(result.detail.contains("no transcript activity today"));
    }

    #[test]
    fn tailer_health_warns_when_watch_root_is_missing() {
        let mut data = diag("Cursor");
        data.watch_roots.clear();

        let result = summarize_tailer_health(&data);

        assert_eq!(result.state, CheckState::Warn);
        assert!(result.detail.contains("no transcript watch roots"));
    }

    #[test]
    fn tailer_health_fails_when_offsets_are_missing() {
        let mut data = diag("Claude Code");
        data.tracked_offsets = Some(0);

        let result = summarize_tailer_health(&data);

        assert_eq!(result.state, CheckState::Fail);
        assert!(result.detail.contains("has not seeded any offsets"));
    }
}
