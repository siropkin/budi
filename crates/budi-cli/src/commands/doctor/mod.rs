use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

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
    let daemon_outage = daemon.outage;

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

            // R1.3 (#670): zero-rows-from-tailer AMBER. The 8.4.0 Copilot
            // Chat parser regression shipped with both checks above PASS:
            // bytes were flowing, the tailer was caught up. The signal that
            // would have flipped doctor to AMBER on the release driver's
            // machine before the tag is "tailer advanced N bytes in the
            // last 30 min but no rows for this provider landed" — see
            // ADR-0092 §2.6.
            let rows = summarize_tailer_rows(conn, provider.as_ref(), Utc::now());
            if !json_output {
                rows.print_respecting(quiet);
            }
            report.record(&rows);
            checks.push(rows);

            // #693: discoverability hint when `seed_offsets` ran against
            // pre-existing transcripts on first boot but no `messages` rows
            // have ever landed for this provider. Distinct from R1.3 above
            // (window-scoped, AMBER) — this is a lifetime check that fires
            // INFO and stays put until the user runs `budi db import` or
            // live ingestion produces rows.
            let history = summarize_pre_boot_history(conn, provider.as_ref());
            if !json_output {
                history.print_respecting(quiet);
            }
            report.record(&history);
            checks.push(history);
        }
    }

    // R1.6 (#653): "Detected providers" section. Lists every Provider where
    // `is_available()` is true, regardless of `agents.toml` enablement, so a
    // user troubleshooting "I installed Copilot but the statusline shows
    // zero" can see at a glance whether the daemon recognises their data.
    let host_hints = read_host_extension_hints();
    let all_providers = budi_core::provider::all_providers();
    for detection in summarize_detected_providers(&all_providers, &host_hints) {
        if !json_output {
            detection.print_respecting(quiet);
        }
        report.record(&detection);
        checks.push(detection);
    }

    if json_output {
        let mut json_checks: Vec<CheckResultJson> =
            checks.iter().map(CheckResultJson::from).collect();
        if let (Some(outage), Some(entry)) = (
            &daemon_outage,
            json_checks.iter_mut().find(|c| c.name == "daemon health"),
        ) {
            entry.auto_recovered = Some(true);
            entry.previous_outage = Some(PreviousOutageJson {
                last_log_entry: outage.last_log_entry.clone(),
                gap_seconds: outage.gap_seconds,
                supervisor: outage.supervisor.clone(),
            });
        }
        let body = DoctorJson {
            all_pass: report.fails == 0 && report.warns == 0,
            checks: json_checks,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    auto_recovered: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_outage: Option<PreviousOutageJson>,
}

#[derive(Debug, Serialize)]
struct PreviousOutageJson {
    #[serde(skip_serializing_if = "Option::is_none")]
    last_log_entry: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    gap_seconds: Option<u64>,
    supervisor: String,
}

impl From<&CheckResult> for CheckResultJson {
    fn from(value: &CheckResult) -> Self {
        Self {
            name: value.label.clone(),
            status: match value.state {
                CheckState::Pass => "pass",
                CheckState::Info => "info",
                CheckState::Warn => "warn",
                CheckState::Fail => "fail",
            },
            detail: value.detail.clone(),
            auto_recovered: None,
            previous_outage: None,
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
            CheckState::Pass | CheckState::Info => {}
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
    /// Informational hint — surfaces an actionable but non-erroneous
    /// observation (e.g. pre-boot transcript history available to backfill
    /// via `budi db import`). Does not count toward `--format json`'s
    /// `all_pass = false` and does not flip the exit code; a green doctor
    /// run with `Info` rows still ends with "All checks passed." See #693.
    Info,
    Warn,
    Fail,
}

impl CheckState {
    fn label(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Fail => "FAIL",
        }
    }

    fn color(self) -> &'static str {
        match self {
            Self::Pass => "\x1b[32m",
            Self::Info => "\x1b[36m",
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

    /// Informational hint — visible inline in text mode and serialised as
    /// `status: "info"` in `--format json`. Does not count as a warning or
    /// failure for exit-code purposes. Used for the `pre-boot history
    /// detected / <provider>` discoverability signal (#693).
    fn info(label: impl Into<String>, detail: impl Into<String>, fix: Option<String>) -> Self {
        Self {
            state: CheckState::Info,
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
    outage: Option<OutageSummary>,
}

struct OutageSummary {
    last_log_entry: Option<String>,
    gap_seconds: Option<u64>,
    supervisor: String,
}

fn daemon_outage_data() -> OutageSummary {
    let supervisor = format!(
        "{}: {}",
        budi_core::autostart::service_mechanism(),
        budi_core::autostart::service_status(),
    );

    let path = match budi_core::autostart::service_log_path() {
        Some(p) => p,
        None => {
            return OutageSummary {
                last_log_entry: None,
                gap_seconds: None,
                supervisor,
            };
        }
    };
    let modified = match fs::metadata(&path).and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(_) => {
            return OutageSummary {
                last_log_entry: None,
                gap_seconds: None,
                supervisor,
            };
        }
    };
    let elapsed = match SystemTime::now().duration_since(modified) {
        Ok(d) => d,
        Err(_) => {
            return OutageSummary {
                last_log_entry: None,
                gap_seconds: None,
                supervisor,
            };
        }
    };

    let mtime: DateTime<Utc> = modified.into();

    OutageSummary {
        last_log_entry: Some(mtime.to_rfc3339()),
        gap_seconds: Some(elapsed.as_secs()),
        supervisor,
    }
}

fn format_outage_display(outage: &OutageSummary) -> String {
    let mut parts = Vec::new();

    if let Some(secs) = outage.gap_seconds {
        let pretty = if secs < 90 {
            format!("{secs}s")
        } else if secs < 90 * 60 {
            format!("{}m", secs / 60)
        } else if secs < 36 * 3600 {
            format!("{}h", secs / 3600)
        } else {
            format!("{}d", secs / 86400)
        };
        parts.push(format!("last log entry ~{pretty} ago"));
    }

    parts.push(format!("supervisor: {}", outage.supervisor));

    format!(" — {}", parts.join("; "))
}

fn check_daemon_health(repo_root: Option<&Path>, config: &config::BudiConfig) -> DaemonCheck {
    let base_url = config.daemon_base_url();
    let daemon_bin_override = std::env::var_os("BUDI_DAEMON_BIN");
    if daemon_health(config) {
        return DaemonCheck {
            result: CheckResult::pass("daemon health", format!("responding on {base_url}")),
            started_this_run: false,
            outage: None,
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
                outage: None,
            };
        }
    };

    match ensure_daemon_running(repo_root, config) {
        Ok(()) if daemon_health(config) => {
            let outage = daemon_outage_data();
            DaemonCheck {
                result: CheckResult::warn(
                    "daemon health",
                    format!(
                        "auto-recovered: was NOT running on first probe; doctor started it on {base_url} (binary: {}){}",
                        daemon_bin.display(),
                        format_outage_display(&outage),
                    ),
                    Some(
                        "Previous outage may indicate a supervisor problem (see #611 for the macOS \
                         launchd kickstart fix). If this recurs, run `budi autostart status` to \
                         verify the supervisor is actually managing the daemon."
                            .to_string(),
                    ),
                ),
                started_this_run: true,
                outage: Some(outage),
            }
        }
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
            outage: None,
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
            outage: None,
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
                    "Run `budi db check --fix` after backing up the database, then rerun `budi doctor`."
                        .to_string(),
                ),
            ),
            conn: Some(conn),
        },
        Err(e) => SchemaCheck {
            result: CheckResult::fail(
                "schema drift",
                format!("could not run {mode} on {} ({e})", db_path.display()),
                Some(
                    "Run `budi db check --fix` or recreate the database with `budi init`."
                        .to_string(),
                ),
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
                Some(
                    "Re-run once the affected files are readable, or run `budi uninstall` to remove managed blocks."
                        .to_string(),
                ),
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
            "Remove the listed managed blocks by hand, or run `budi uninstall` (which strips managed 8.0/8.1 proxy residue as part of teardown)."
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
            Some("Run `budi db check --fix`, then rerun `budi doctor`.".to_string()),
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
                "Run `budi init` or `budi db check --fix` with the current 8.2 build to remove the old `proxy_events` table."
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

// ---------------------------------------------------------------------------
// R1.3 (#670): zero-rows-from-tailer AMBER signal.
//
// The 8.4.0 Copilot Chat parser regression shipped with `tailer health` PASS
// and `transcript visibility` PASS — both run on byte-flow signals, neither
// guarantees the parser actually emits rows. The signal below pairs the
// existing `tail_offsets` snapshot with a `messages` row count over the same
// window: if the tailer advanced bytes for a provider but zero rows for that
// provider landed, the parser is almost certainly broken against a new
// upstream shape (see ADR-0092 §2.6 / `MIN_API_VERSION`).
//
// AMBER (Warn), not FAIL: a generally-healthy install briefly between
// extension format flips should not break for an end user, and a workspace
// with non-AI background writes (logs, embeddings) can legitimately advance
// bytes without producing message rows. The release-side smoke gate (R1.5,
// #672) escalates AMBER to a release blocker so this stays a useful signal
// without being a noisy failure for end users.
// ---------------------------------------------------------------------------

const ZERO_ROWS_WINDOW_MINUTES: i64 = 30;

#[derive(Debug, Clone)]
struct TailerRowsActivity {
    advanced_bytes: u64,
    last_seen: Option<DateTime<Utc>>,
    rows_in_window: usize,
    db_error: Option<String>,
}

fn summarize_tailer_rows(
    conn: Option<&Connection>,
    provider: &dyn Provider,
    now: DateTime<Utc>,
) -> CheckResult {
    let label = format!("tailer rows / {}", provider.display_name());
    let activity = match conn {
        Some(conn) => load_tailer_rows_activity(conn, provider.name(), now),
        None => TailerRowsActivity {
            advanced_bytes: 0,
            last_seen: None,
            rows_in_window: 0,
            db_error: Some("database connection unavailable".to_string()),
        },
    };
    classify_tailer_rows(label, provider.name(), &activity, now)
}

fn load_tailer_rows_activity(
    conn: &Connection,
    provider: &str,
    now: DateTime<Utc>,
) -> TailerRowsActivity {
    let mut activity = TailerRowsActivity {
        advanced_bytes: 0,
        last_seen: None,
        rows_in_window: 0,
        db_error: None,
    };

    match conn.query_row(
        "SELECT COALESCE(SUM(byte_offset), 0), MAX(last_seen)
         FROM tail_offsets
         WHERE provider = ?1",
        params![provider],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
    ) {
        Ok((bytes, last_seen)) => {
            activity.advanced_bytes = bytes.max(0) as u64;
            activity.last_seen = last_seen.and_then(|value| parse_rfc3339_utc(&value));
        }
        Err(e) => {
            activity.db_error = Some(format!("could not read tail_offsets ({e})"));
            return activity;
        }
    }

    let cutoff = now - chrono::Duration::minutes(ZERO_ROWS_WINDOW_MINUTES);
    let cutoff_iso = cutoff.to_rfc3339();
    match conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE provider = ?1 AND timestamp >= ?2",
        params![provider, cutoff_iso],
        |row| row.get::<_, i64>(0),
    ) {
        Ok(count) => activity.rows_in_window = count.max(0) as usize,
        Err(e) => {
            activity.db_error = Some(format!("could not count messages ({e})"));
        }
    }

    activity
}

fn classify_tailer_rows(
    label: String,
    provider_name: &str,
    activity: &TailerRowsActivity,
    now: DateTime<Utc>,
) -> CheckResult {
    if let Some(ref error) = activity.db_error {
        return CheckResult::pass(label, format!("zero-rows check skipped — {error}"));
    }

    let last_seen_recent = activity
        .last_seen
        .map(|ts| {
            let age = now.signed_duration_since(ts).num_minutes();
            (0..=ZERO_ROWS_WINDOW_MINUTES).contains(&age)
        })
        .unwrap_or(false);

    if !last_seen_recent || activity.advanced_bytes == 0 {
        return CheckResult::pass(
            label,
            format!(
                "no recent tailer advance for {provider_name} in the last {ZERO_ROWS_WINDOW_MINUTES} min — nothing to compare against"
            ),
        );
    }

    if activity.rows_in_window > 0 {
        return CheckResult::pass(
            label,
            format!(
                "tailer consumed {} for {provider_name} and {} row(s) landed in the last {ZERO_ROWS_WINDOW_MINUTES} min",
                format_bytes(activity.advanced_bytes),
                activity.rows_in_window
            ),
        );
    }

    // AMBER: bytes flowing, no rows emitting. Likely a parser shape regression.
    let detail = if provider_name == "copilot_chat" {
        format!(
            "tailer advanced {} in the last {ZERO_ROWS_WINDOW_MINUTES} min but no {provider_name} rows landed in the database. Likely a parser shape regression — see ADR-0092 §2.6 / MIN_API_VERSION.",
            format_bytes(activity.advanced_bytes),
        )
    } else {
        format!(
            "tailer advanced {} in the last {ZERO_ROWS_WINDOW_MINUTES} min but no {provider_name} rows landed in the database.",
            format_bytes(activity.advanced_bytes),
        )
    };

    CheckResult::warn(
        label,
        detail,
        Some(
            "Check `~/.local/share/budi/logs/daemon.log` for parser warnings (e.g. `*_unknown_record_shape`) and rerun `budi doctor` after restarting the daemon."
                .to_string(),
        ),
    )
}

// ---------------------------------------------------------------------------
// #693: pre-boot history INFO signal.
//
// Distinct from R1.3 (`tailer rows / X` AMBER) — that check fires when bytes
// flow but no rows land in a 30-min window, the broken-parser pattern from
// 8.4.0. The signal here is the dual: `tail_offsets` rows exist with
// positive byte_offsets (i.e. `seed_offsets` ran on first boot against
// pre-existing transcripts) but the `messages` table is empty for that
// provider lifetime. The escape hatch is `budi db import`; this check
// surfaces it.
//
// INFO (Hint), not WARN: per ADR-0089 §1, `seed_offsets` keeping pre-boot
// files at EOF is the right default. The user opting in to backfill is the
// remediation; warning would imply the daemon is broken when it is in fact
// behaving as designed. Idempotent: once any messages row lands for the
// provider (live or backfilled), the check returns PASS and stays silent.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct PreBootHistoryActivity {
    seeded_files: usize,
    advanced_bytes: u64,
    lifetime_messages: usize,
    db_error: Option<String>,
}

fn summarize_pre_boot_history(conn: Option<&Connection>, provider: &dyn Provider) -> CheckResult {
    let label = format!("pre-boot history detected / {}", provider.display_name());
    let activity = match conn {
        Some(conn) => load_pre_boot_history_activity(conn, provider.name()),
        None => PreBootHistoryActivity {
            seeded_files: 0,
            advanced_bytes: 0,
            lifetime_messages: 0,
            db_error: Some("database connection unavailable".to_string()),
        },
    };
    classify_pre_boot_history(label, &activity)
}

fn load_pre_boot_history_activity(conn: &Connection, provider: &str) -> PreBootHistoryActivity {
    let mut activity = PreBootHistoryActivity {
        seeded_files: 0,
        advanced_bytes: 0,
        lifetime_messages: 0,
        db_error: None,
    };

    match conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(byte_offset), 0)
         FROM tail_offsets
         WHERE provider = ?1 AND byte_offset > 0",
        params![provider],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
    ) {
        Ok((count, bytes)) => {
            activity.seeded_files = count.max(0) as usize;
            activity.advanced_bytes = bytes.max(0) as u64;
        }
        Err(e) => {
            activity.db_error = Some(format!("could not read tail_offsets ({e})"));
            return activity;
        }
    }

    match conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE provider = ?1",
        params![provider],
        |row| row.get::<_, i64>(0),
    ) {
        Ok(count) => activity.lifetime_messages = count.max(0) as usize,
        Err(e) => {
            activity.db_error = Some(format!("could not count messages ({e})"));
        }
    }

    activity
}

fn classify_pre_boot_history(label: String, activity: &PreBootHistoryActivity) -> CheckResult {
    if let Some(ref error) = activity.db_error {
        return CheckResult::pass(label, format!("pre-boot history check skipped — {error}"));
    }

    // No seeded transcripts → the daemon never observed pre-existing
    // history for this provider; nothing to backfill.
    if activity.seeded_files == 0 {
        return CheckResult::pass(label, "no pre-boot transcripts seeded for this provider");
    }

    // Live or already-imported rows present → backfill already happened (or
    // is unnecessary). Idempotent silence per the ticket.
    if activity.lifetime_messages > 0 {
        return CheckResult::pass(
            label,
            format!(
                "{} pre-boot transcript(s) tracked; {} message row(s) already in the database — nothing to backfill",
                activity.seeded_files, activity.lifetime_messages,
            ),
        );
    }

    // tail_offsets rows present, messages empty → discoverability gap.
    let detail = format!(
        "{} transcript(s) seeded as history ({} pre-dating budi installation). Run `budi db import` to backfill.",
        activity.seeded_files,
        format_bytes(activity.advanced_bytes),
    );
    CheckResult::info(
        label,
        detail,
        Some(
            "Run `budi db import` to backfill pre-existing transcripts the daemon seeded as history. Pass `--force` to re-ingest after upgrading budi."
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

// ---------------------------------------------------------------------------
// "Detected providers" section (R1.6 / #653)
// ---------------------------------------------------------------------------

/// One detection line per `Provider::is_available()` host, plus a summary
/// row when nothing is detected. The section is intentionally separate from
/// the "tailer health / transcript visibility" rows above because those run
/// only against `agents.toml`-enabled providers — this one runs against
/// every registered provider so a user can see *why* a host like Copilot
/// Chat isn't being tracked even before they touch agents.toml.
fn summarize_detected_providers(
    providers: &[Box<dyn Provider>],
    host_hints: &HostExtensionHints,
) -> Vec<CheckResult> {
    let mut detected: Vec<CheckResult> = Vec::new();
    let mut detected_count = 0usize;

    for provider in providers {
        if !provider.is_available() {
            continue;
        }
        detected_count += 1;
        detected.push(summarize_provider_detection(provider.as_ref(), host_hints));
    }

    if detected_count == 0 {
        // Per ticket: warn (not fail) when no providers are detected — the
        // daemon may be healthy and simply have nothing to tail yet.
        return vec![CheckResult::warn(
            "detected providers",
            "no AI editor data detected on this host yet (Cursor, VS Code + Copilot Chat, Claude Code, Codex, Copilot CLI all report `is_available() == false`)",
            Some(
                "Open one of your AI editors so it creates its local data directory, then rerun `budi doctor`."
                    .to_string(),
            ),
        )];
    }

    detected
}

/// Per-provider detection one-liner. Mirrors the examples in #653:
/// `copilot_chat — VS Code (workspaceStorage detected, N session files, last write Tm ago)`.
/// Format is generic so the same code path handles every provider; per-host
/// hints (e.g. "VS Code Insiders") come from path inspection.
fn summarize_provider_detection(
    provider: &dyn Provider,
    host_hints: &HostExtensionHints,
) -> CheckResult {
    let label = format!("detected providers / {}", provider.display_name());

    let watch_roots = provider.watch_roots();
    let discovered = provider.discover_files().ok().unwrap_or_default();
    let session_count = discovered.len();

    let latest_mtime = discovered
        .iter()
        .filter_map(|f| std::fs::metadata(&f.path).ok())
        .filter_map(|m| m.modified().ok())
        .map(DateTime::<Utc>::from)
        .max();

    let host_label = host_hint_from_paths(&watch_roots);

    let mut parts: Vec<String> = Vec::new();
    if session_count == 0 {
        parts.push(format!(
            "{} watch root(s) detected, no sessions yet",
            watch_roots.len()
        ));
    } else {
        let word = if session_count == 1 { "file" } else { "files" };
        parts.push(format!("{session_count} session {word}"));
    }
    if let Some(ts) = latest_mtime {
        parts.push(format!("last write {} ago", format_relative_age(ts)));
    }
    if let Some(extensions) = host_hints.extensions_for(provider.name()) {
        parts.push(format!(
            "installed extension hints: {}",
            extensions.join(", ")
        ));
    }

    let detail = match host_label {
        Some(host) => format!("{host} ({})", parts.join(", ")),
        None => parts.join(", "),
    };

    CheckResult::pass(label, detail)
}

/// Match well-known editor-host markers in any of the provider's watch-root
/// path components. Returns `None` when the provider doesn't live under a
/// host-scoped directory (e.g. Claude Code lives under `~/.claude`).
fn host_hint_from_paths(roots: &[PathBuf]) -> Option<String> {
    // Order matters — longer, more specific tokens are listed before their
    // generic prefixes so "Code - Insiders" doesn't get misclassified as
    // plain "Code".
    const KNOWN_HOSTS: &[(&str, &str)] = &[
        ("Code - Insiders", "VS Code Insiders"),
        ("Code - Exploration", "VS Code Exploration"),
        ("VSCodium", "VSCodium"),
        ("Cursor", "Cursor"),
        (".vscode-server-insiders", "VS Code Server (Insiders)"),
        (".vscode-server", "VS Code Server"),
        (".vscode-remote", "VS Code Remote"),
        ("Code", "VS Code"),
    ];

    let mut hosts: Vec<&'static str> = Vec::new();
    for root in roots {
        let s = root.display().to_string();
        for (token, label) in KNOWN_HOSTS {
            if s.contains(token) && !hosts.contains(label) {
                hosts.push(label);
                break;
            }
        }
    }
    if hosts.is_empty() {
        None
    } else {
        Some(hosts.join(" / "))
    }
}

/// Optional UX hints loaded from the `cursor-sessions.json` v1 file (and a
/// sibling for VS Code if budi-cursor begins writing one). Per ADR-0086 §3.4
/// the v1 schema is `{active_session_id, updated_at}`; this loader is
/// deliberately permissive and ignores any field it doesn't recognise so a
/// future schema bump that adds an `installed_extensions` array becomes a
/// no-op upgrade for older binaries.
#[derive(Debug, Default)]
struct HostExtensionHints {
    /// Map from `Provider::name()` (e.g. `"copilot_chat"`) to a deduped list
    /// of installed-extension identifiers reported by the host.
    by_provider: std::collections::HashMap<String, Vec<String>>,
}

impl HostExtensionHints {
    fn extensions_for(&self, provider: &str) -> Option<&Vec<String>> {
        self.by_provider.get(provider).filter(|v| !v.is_empty())
    }
}

fn read_host_extension_hints() -> HostExtensionHints {
    let mut hints = HostExtensionHints::default();
    let Ok(home) = budi_core::config::budi_home_dir() else {
        return hints;
    };
    // Both the existing Cursor session file and a future VS Code sibling
    // share the same permissive shape; iterate every candidate so a budi-
    // cursor release that adds `vscode-sessions.json` lights up immediately.
    for filename in ["cursor-sessions.json", "vscode-sessions.json"] {
        let path = home.join(filename);
        if let Some(parsed) = read_session_hint_file(&path) {
            merge_hint_extensions(&mut hints, parsed);
        }
    }
    hints
}

fn read_session_hint_file(path: &Path) -> Option<serde_json::Value> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice::<serde_json::Value>(&bytes).ok()
}

fn merge_hint_extensions(hints: &mut HostExtensionHints, doc: serde_json::Value) {
    // Two recognised shapes:
    //   1. {"installed_extensions": {"copilot_chat": ["github.copilot-chat", ...]}}
    //   2. {"installed_extensions": ["github.copilot-chat", "continue.continue"]}
    // Shape (2) is mapped to provider buckets via a static manifest of
    // well-known extension ids → provider names.
    let Some(value) = doc.get("installed_extensions") else {
        return;
    };
    if let Some(map) = value.as_object() {
        for (provider, ids) in map {
            if let Some(arr) = ids.as_array() {
                for id in arr {
                    if let Some(s) = id.as_str() {
                        push_unique(&mut hints.by_provider, provider, s);
                    }
                }
            }
        }
        return;
    }
    if let Some(arr) = value.as_array() {
        for id in arr {
            if let Some(s) = id.as_str()
                && let Some(provider) = provider_for_extension_id(s)
            {
                push_unique(&mut hints.by_provider, provider, s);
            }
        }
    }
}

fn push_unique(
    map: &mut std::collections::HashMap<String, Vec<String>>,
    provider: &str,
    extension_id: &str,
) {
    let bucket = map.entry(provider.to_string()).or_default();
    if !bucket.iter().any(|existing| existing == extension_id) {
        bucket.push(extension_id.to_string());
    }
}

/// Map well-known marketplace extension ids to budi `Provider::name()`
/// values. Only covers the ids relevant in 8.4.0; unknown ids are dropped
/// so the doctor output stays tight.
fn provider_for_extension_id(id: &str) -> Option<&'static str> {
    let lower = id.to_ascii_lowercase();
    match lower.as_str() {
        "github.copilot-chat" | "github.copilot" => Some("copilot_chat"),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
