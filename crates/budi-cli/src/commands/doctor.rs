use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use budi_core::config;
use budi_core::legacy_proxy;
use budi_core::provider::Provider;
use chrono::{DateTime, Local, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::StatsFormat;
use crate::daemon::{daemon_health, ensure_daemon_running, resolve_daemon_binary};

pub fn cmd_doctor(
    repo_root: Option<PathBuf>,
    deep: bool,
    quiet: bool,
    format: StatsFormat,
) -> Result<()> {
    let repo_root = super::try_resolve_repo_root(repo_root);
    let config = match &repo_root {
        Some(root) => config::load_or_default(root)?,
        None => config::BudiConfig::default(),
    };

    let json_output = matches!(format, StatsFormat::Json);

    let dim = super::ansi("\x1b[90m");
    let reset = super::ansi("\x1b[0m");

    if !json_output {
        if let Some(ref root) = repo_root {
            println!("budi doctor - {}", root.display());
        } else {
            println!("budi doctor - global mode");
        }
        println!();
    }

    let mut report = DoctorReport::default();
    // Buffer of every check that ran, in the same order they appear in the
    // text-mode output. Used to render the `--format json` payload after
    // the run completes so the JSON shape matches what the operator sees.
    let mut checks: Vec<CheckResult> = Vec::new();

    let daemon = check_daemon_health(repo_root.as_deref(), &config);
    if !json_output {
        daemon.result.print_respecting(quiet);
    }
    report.record(&daemon.result);
    checks.push(daemon.result.clone());

    if daemon.started_this_run {
        // The daemon seeds tail offsets on startup before the backstop loop
        // begins. A short pause keeps `doctor` from racing that initial
        // bookkeeping on a cold start.
        std::thread::sleep(Duration::from_millis(750));
    }

    let db_path = budi_core::analytics::db_path()?;
    let schema = check_schema(&db_path, deep);
    if !json_output {
        schema.result.print_respecting(quiet);
    }
    report.record(&schema.result);
    checks.push(schema.result.clone());

    let proxy_residue = check_legacy_proxy_residue();
    if !json_output {
        proxy_residue.print_respecting(quiet);
    }
    report.record(&proxy_residue);
    checks.push(proxy_residue);

    let statusline_check = check_claude_statusline_integration();
    if !json_output {
        statusline_check.print_respecting(quiet);
    }
    report.record(&statusline_check);
    checks.push(statusline_check);

    if let Some(conn) = schema.conn.as_ref() {
        let legacy_proxy_history = check_legacy_proxy_history(conn);
        if !json_output {
            legacy_proxy_history.print_respecting(quiet);
        }
        report.record(&legacy_proxy_history);
        checks.push(legacy_proxy_history);
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
        if !json_output {
            no_providers.print_respecting(quiet);
        }
        report.record(&no_providers);
        checks.push(no_providers);
    } else {
        let conn = schema.conn.as_ref();
        for provider in &providers {
            let diag = gather_provider_doctor_data(conn, provider.as_ref());

            let tailer = summarize_tailer_health(&diag);
            if !json_output {
                tailer.print_respecting(quiet);
            }
            report.record(&tailer);
            checks.push(tailer);

            let visibility = summarize_transcript_visibility(&diag);
            if !json_output {
                visibility.print_respecting(quiet);
            }
            report.record(&visibility);
            checks.push(visibility);
        }
    }

    if json_output {
        let body = DoctorJson {
            all_pass: report.fails == 0 && report.warns == 0,
            checks: checks.iter().map(CheckResultJson::from).collect(),
        };
        super::print_json(&body)?;
        // Exit code matrix matches text mode: warnings are not failures,
        // so we only escalate on an actual FAIL row. Use process::exit(2)
        // (matching `cloud sync --format json`) so callers can branch on
        // status without parsing stderr.
        if report.fails > 0 {
            std::process::exit(2);
        }
        return Ok(());
    }

    println!();
    if report.fails == 0 && report.warns == 0 {
        // #487: on a clean green run, `--quiet` suppresses every
        // individual PASS line above; the final summary stays as-is
        // so the operator still gets the one-line all-clear signal.
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

#[derive(Debug, Serialize)]
struct DoctorJson {
    all_pass: bool,
    checks: Vec<CheckResultJson>,
}

#[derive(Debug, Serialize)]
struct CheckResultJson {
    name: String,
    status: &'static str,
    detail: String,
}

impl From<&CheckResult> for CheckResultJson {
    fn from(value: &CheckResult) -> Self {
        Self {
            name: value.label.clone(),
            status: match value.state {
                CheckState::Pass => "pass",
                CheckState::Warn => "warn",
                CheckState::Fail => "fail",
            },
            detail: value.detail.clone(),
        }
    }
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

    /// Print unless `quiet` is set and the state is `Pass`. Keeps
    /// WARN / FAIL lines visible in every mode so an operator never
    /// misses a real problem under `--quiet`; exists as a named
    /// helper (rather than an inline `if`) so #487's contract is
    /// testable at the unit level and so new callers can't forget
    /// to respect `--quiet`. Green-path PASS lines + summary line
    /// are still printed on the default (non-quiet) path.
    fn print_respecting(&self, quiet: bool) {
        if quiet && self.state == CheckState::Pass {
            return;
        }
        self.print();
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
                    "Run `budi db repair` after backing up the database, then rerun `budi doctor`."
                        .to_string(),
                ),
            ),
            conn: Some(conn),
        },
        Err(e) => SchemaCheck {
            result: CheckResult::fail(
                "schema drift",
                format!("could not run {mode} on {} ({e})", db_path.display()),
                Some("Run `budi db repair` or recreate the database with `budi init`.".to_string()),
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

fn check_legacy_proxy_residue() -> CheckResult {
    let scan = match legacy_proxy::scan() {
        Ok(scan) => scan,
        Err(e) => {
            return CheckResult::warn(
                "leftover proxy config",
                format!("could not inspect legacy proxy residue ({e})"),
                Some("Run `budi init --cleanup` once the affected files are readable.".to_string()),
            );
        }
    };

    if !scan.has_any_residue() {
        return CheckResult::pass(
            "leftover proxy config",
            "no legacy proxy-routing residue detected in known config files or the current shell",
        );
    }

    let mut details = Vec::new();
    let managed_paths = scan
        .files
        .iter()
        .filter(|file| file.has_managed_blocks())
        .map(|file| file.path.display().to_string())
        .collect::<Vec<_>>();
    if !managed_paths.is_empty() {
        details.push(format!(
            "managed Budi proxy block(s) remain in {}",
            managed_paths.join(", ")
        ));
    }

    if scan.total_fuzzy_findings() > 0 {
        let fuzzy_paths = scan
            .files
            .iter()
            .filter(|file| file.has_fuzzy_findings())
            .map(|file| file.path.display().to_string())
            .collect::<Vec<_>>();
        details.push(format!(
            "manual edits still reference the old proxy in {}",
            fuzzy_paths.join(", ")
        ));
    }

    if !scan.exported_env_vars.is_empty() {
        let rendered = scan
            .exported_env_vars
            .iter()
            .map(|entry| format!("{}={}", entry.key, entry.value))
            .collect::<Vec<_>>()
            .join(", ");
        details.push(format!("current shell still exports {rendered}"));
    }

    CheckResult::warn(
        "leftover proxy config",
        details.join("; "),
        Some(
            "Run `budi init --cleanup` to review and remove old 8.0/8.1 proxy residue with explicit consent."
                .to_string(),
        ),
    )
}

/// Warn when Claude Code is installed on this machine but the Budi
/// statusline is not wired into `~/.claude/settings.json`. Tracks the
/// fresh-user contract pinned by #454: `budi init` should leave Claude
/// Code with a working Budi statusline, and a diverged state must be
/// surfaced with the exact command to repair it.
fn check_claude_statusline_integration() -> CheckResult {
    let home = match config::home_dir() {
        Ok(h) => h,
        Err(_) => {
            return CheckResult::pass(
                "Claude statusline",
                "home directory could not be resolved; statusline state is not checked",
            );
        }
    };

    let claude_dir = home.join(".claude");
    if !claude_dir.exists() {
        return CheckResult::pass(
            "Claude statusline",
            "Claude Code not detected (~/.claude is absent); statusline install is not required",
        );
    }

    if super::integrations::claude_statusline_installed() {
        return CheckResult::pass(
            "Claude statusline",
            "budi statusline is wired into ~/.claude/settings.json",
        );
    }

    CheckResult::warn(
        "Claude statusline",
        "Claude Code is installed but the Budi statusline is not wired into ~/.claude/settings.json — Claude Code will render no cost display",
        Some(
            "Run `budi integrations install --with claude-code-statusline` (or plain `budi integrations install` to apply all recommended integrations), then restart Claude Code.".to_string(),
        ),
    )
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
            Some("Run `budi db repair`, then rerun `budi doctor`.".to_string()),
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
                "Run `budi init` or `budi db repair` with the current 8.2 build to remove the old `proxy_events` table."
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
    latest_file_tail_seen: Option<DateTime<Utc>>,
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
        latest_file_tail_seen: None,
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
                    data.latest_file_tail_seen = state.last_seen;
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
                "Make sure `budi-daemon` is running so the tailer can seed this file, then rerun `budi doctor`. Run `budi db import` if you also need older history backfilled."
                    .to_string(),
            ),
        );
    };

    let gap = file_len.saturating_sub(offset);
    classify_transcript_visibility(
        label,
        latest_file,
        gap,
        diag.latest_file_tail_seen,
        diag.latest_file_mtime,
        Utc::now(),
    )
}

/// Small gap that a live transcript can generate between tailer reads.
const LIVE_WRITE_GAP_BYTES: u64 = 1024 * 1024; // 1 MB
/// Gap above which the tailer is treated as genuinely wedged regardless of activity recency.
const WEDGE_GAP_BYTES: u64 = 10 * 1024 * 1024; // 10 MB
/// Window within which tailer activity counts as "live".
const RECENT_ACTIVITY_SECS: i64 = 60;
/// Window beyond which tailer silence counts as "stale" — only a FAIL when paired with a
/// currently-being-written file (mtime within `RECENT_ACTIVITY_SECS`).
const STALE_ACTIVITY_SECS: i64 = 300; // 5 minutes

fn classify_transcript_visibility(
    label: String,
    latest_file: &Path,
    gap: u64,
    latest_file_tail_seen: Option<DateTime<Utc>>,
    latest_file_mtime: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> CheckResult {
    if gap == 0 {
        return CheckResult::pass(
            label,
            format!(
                "latest transcript is {} and the tailer is caught up (0 B behind)",
                latest_file.display()
            ),
        );
    }

    let tail_age_secs =
        latest_file_tail_seen.map(|ts| now.signed_duration_since(ts).num_seconds().max(0));
    let file_age_secs =
        latest_file_mtime.map(|ts| now.signed_duration_since(ts).num_seconds().max(0));

    let tail_activity_label = match tail_age_secs {
        Some(secs) => format!("tailer last read {} ago", format_relative_age_secs(secs)),
        None => "tailer has not recorded activity on this file yet".to_string(),
    };

    if gap > WEDGE_GAP_BYTES {
        return CheckResult::fail(
            label,
            format!(
                "latest transcript is {} and the tailer is {} behind (above the {} wedge threshold); {}",
                latest_file.display(),
                format_bytes(gap),
                format_bytes(WEDGE_GAP_BYTES),
                tail_activity_label
            ),
            Some(
                "Check `~/.local/share/budi/logs/daemon.log` for tailer errors and rerun `budi doctor` once the daemon has caught up."
                    .to_string(),
            ),
        );
    }

    if let (Some(tail_age), Some(file_age)) = (tail_age_secs, file_age_secs)
        && tail_age > STALE_ACTIVITY_SECS
        && file_age < RECENT_ACTIVITY_SECS
    {
        return CheckResult::fail(
            label,
            format!(
                "latest transcript is {} and is actively being written (modified {}s ago), but the tailer has not read it in {}",
                latest_file.display(),
                file_age,
                format_relative_age_secs(tail_age)
            ),
            Some(
                "Check `~/.local/share/budi/logs/daemon.log` for tailer errors and rerun `budi doctor` once the daemon has caught up."
                    .to_string(),
            ),
        );
    }

    let tailer_recently_active = tail_age_secs
        .map(|secs| secs <= RECENT_ACTIVITY_SECS)
        .unwrap_or(false);

    if gap <= LIVE_WRITE_GAP_BYTES && tailer_recently_active {
        return CheckResult::pass(
            label,
            format!(
                "latest transcript is {} and the tailer is {} behind a live write ({}); gap typically closes within a few seconds",
                latest_file.display(),
                format_bytes(gap),
                tail_activity_label
            ),
        );
    }

    CheckResult::warn(
        label,
        format!(
            "latest transcript is {} and the tailer is {} behind; {}",
            latest_file.display(),
            format_bytes(gap),
            tail_activity_label
        ),
        Some(
            "This usually closes on its own for a live transcript. If it persists for several minutes, check `~/.local/share/budi/logs/daemon.log`."
                .to_string(),
        ),
    )
}

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if bytes < KB {
        format!("{bytes} B")
    } else if bytes < MB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else if bytes < GB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    }
}

fn format_relative_age_secs(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 60 * 60 {
        format!("{}m", secs / 60)
    } else if secs < 60 * 60 * 48 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
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

    /// #588: `budi doctor --format json` lock-in. The JSON contract is
    /// `{checks: [{name, status, detail}], all_pass}` with `status`
    /// drawn from a fixed vocabulary of `pass | warn | fail`. Scripted
    /// callers branch on this shape — a future rename would silently
    /// break them.
    #[test]
    fn doctor_json_locks_schema_and_status_vocabulary() {
        let checks = [
            CheckResult::pass("daemon health", "responding on http://127.0.0.1:7878"),
            CheckResult::warn("tailer providers", "no enabled providers", None),
            CheckResult::fail(
                "schema",
                "missing column `tags`",
                Some("budi db check --fix".into()),
            ),
        ];
        let body = DoctorJson {
            all_pass: false,
            checks: checks.iter().map(CheckResultJson::from).collect(),
        };
        let v = serde_json::to_value(&body).expect("serialise");

        let mut top_keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        top_keys.sort();
        assert_eq!(top_keys, vec!["all_pass", "checks"]);
        assert_eq!(v["all_pass"], serde_json::json!(false));

        let arr = v["checks"].as_array().expect("checks array");
        assert_eq!(arr.len(), 3);
        for entry in arr {
            let mut keys: Vec<&str> = entry
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            keys.sort();
            assert_eq!(keys, vec!["detail", "name", "status"]);
        }
        assert_eq!(arr[0]["status"], serde_json::json!("pass"));
        assert_eq!(arr[1]["status"], serde_json::json!("warn"));
        assert_eq!(arr[2]["status"], serde_json::json!("fail"));
        // `fix` is intentionally not part of the JSON shape — it's a
        // text-mode-only operator hint, not part of the wire contract.
        assert!(arr[2].as_object().unwrap().get("fix").is_none());
    }

    #[test]
    fn doctor_json_all_pass_true_when_all_checks_pass() {
        let checks = [CheckResult::pass("a", "ok"), CheckResult::pass("b", "ok")];
        let body = DoctorJson {
            all_pass: true,
            checks: checks.iter().map(CheckResultJson::from).collect(),
        };
        let v = serde_json::to_value(&body).expect("serialise");
        assert_eq!(v["all_pass"], serde_json::json!(true));
    }

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
            latest_file_tail_seen: Some(Utc::now()),
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
                .contains("budi db repair")
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
    fn transcript_visibility_passes_on_small_gap_with_recent_tailer_activity() {
        let mut data = diag("Claude Code");
        data.latest_tail_offset = Some(96);

        let result = summarize_transcript_visibility(&data);

        assert_eq!(result.state, CheckState::Pass);
        assert!(result.detail.contains("24 B behind a live write"));
        assert!(result.detail.contains("tailer last read"));
    }

    fn path() -> &'static Path {
        Path::new("/tmp/watch/session.jsonl")
    }

    #[test]
    fn visibility_passes_when_caught_up() {
        let now = Utc::now();
        let result = classify_transcript_visibility(
            "transcript visibility / Cursor".to_string(),
            path(),
            0,
            Some(now),
            Some(now),
            now,
        );
        assert_eq!(result.state, CheckState::Pass);
        assert!(result.detail.contains("caught up"));
        assert!(result.fix.is_none());
    }

    #[test]
    fn visibility_passes_on_live_write_drift() {
        // Precisely the 2026-04-20 fresh-user repro: 2551 B gap, tailer 1s ago.
        let now = Utc::now();
        let result = classify_transcript_visibility(
            "transcript visibility / Cursor".to_string(),
            path(),
            2551,
            Some(now - chrono::Duration::seconds(1)),
            Some(now - chrono::Duration::seconds(1)),
            now,
        );
        assert_eq!(result.state, CheckState::Pass);
        assert!(result.detail.contains("2.5 KB behind a live write"));
        assert!(result.fix.is_none());
    }

    #[test]
    fn visibility_warns_when_tailer_activity_is_stale_but_gap_is_small() {
        let now = Utc::now();
        let result = classify_transcript_visibility(
            "transcript visibility / Cursor".to_string(),
            path(),
            4096,
            Some(now - chrono::Duration::seconds(120)),
            Some(now - chrono::Duration::seconds(120)),
            now,
        );
        assert_eq!(result.state, CheckState::Warn);
        assert!(result.detail.contains("4.0 KB behind"));
        assert!(
            result
                .fix
                .as_deref()
                .unwrap_or_default()
                .contains("daemon.log")
        );
    }

    #[test]
    fn visibility_warns_when_gap_exceeds_live_write_threshold() {
        let now = Utc::now();
        let result = classify_transcript_visibility(
            "transcript visibility / Cursor".to_string(),
            path(),
            2 * 1024 * 1024,
            Some(now - chrono::Duration::seconds(2)),
            Some(now - chrono::Duration::seconds(2)),
            now,
        );
        assert_eq!(result.state, CheckState::Warn);
        assert!(result.detail.contains("2.0 MB behind"));
    }

    #[test]
    fn visibility_fails_when_gap_exceeds_wedge_threshold() {
        let now = Utc::now();
        let result = classify_transcript_visibility(
            "transcript visibility / Cursor".to_string(),
            path(),
            20 * 1024 * 1024,
            Some(now - chrono::Duration::seconds(1)),
            Some(now - chrono::Duration::seconds(1)),
            now,
        );
        assert_eq!(result.state, CheckState::Fail);
        assert!(result.detail.contains("20.0 MB behind"));
        assert!(result.detail.contains("wedge threshold"));
        assert!(
            result
                .fix
                .as_deref()
                .unwrap_or_default()
                .contains("daemon.log")
        );
    }

    #[test]
    fn visibility_fails_when_file_is_actively_written_but_tailer_is_idle() {
        let now = Utc::now();
        let result = classify_transcript_visibility(
            "transcript visibility / Cursor".to_string(),
            path(),
            4096,
            Some(now - chrono::Duration::seconds(600)),
            Some(now - chrono::Duration::seconds(2)),
            now,
        );
        assert_eq!(result.state, CheckState::Fail);
        assert!(result.detail.contains("actively being written"));
        assert!(result.detail.contains("has not read it in"));
    }

    #[test]
    fn visibility_does_not_suggest_restart_with_budi_init() {
        // Regression guard for #438 — the legacy FAIL message told users to
        // restart with `budi init` on harmless live-write drift. Never do that.
        let now = Utc::now();
        for gap in [0u64, 2_551, 4096, 2 * 1024 * 1024, 20 * 1024 * 1024] {
            let result = classify_transcript_visibility(
                "transcript visibility / Cursor".to_string(),
                path(),
                gap,
                Some(now - chrono::Duration::seconds(1)),
                Some(now - chrono::Duration::seconds(1)),
                now,
            );
            let fix = result.fix.as_deref().unwrap_or_default();
            assert!(
                !fix.contains("budi init"),
                "fix copy should not suggest `budi init` (gap={gap}): {fix:?}"
            );
        }
    }

    #[test]
    fn format_bytes_rounds_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2551), "2.5 KB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(format_bytes(10 * 1024 * 1024), "10.0 MB");
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
