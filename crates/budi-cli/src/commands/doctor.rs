use std::path::{Path, PathBuf};

use anyhow::Result;
use budi_core::config;

use crate::daemon::{daemon_health, ensure_daemon_running, resolve_daemon_binary};

pub fn cmd_doctor(repo_root: Option<PathBuf>, deep: bool) -> Result<()> {
    let repo_root = super::try_resolve_repo_root(repo_root);

    let config = match &repo_root {
        Some(root) => config::load_or_default(root)?,
        None => config::BudiConfig::default(),
    };
    let mut issues: Vec<String> = Vec::new();
    // Track whether the local database shows any assistant activity yet so we
    // can add a friendly first-run hint when everything is healthy but the
    // user hasn't sent their first prompt. A single positive signal from any
    // attribution check flips this off.
    let mut has_any_assistant_activity = false;

    let green = super::ansi("\x1b[32m");
    let red = super::ansi("\x1b[31m");
    let dim = super::ansi("\x1b[90m");
    let reset = super::ansi("\x1b[0m");

    if let Some(ref root) = repo_root {
        println!("budi doctor — {}", root.display());
    } else {
        println!("budi doctor — global mode");
    }
    println!();

    if let Some(ref root) = repo_root {
        let has_git = root.join(".git").exists();
        doctor_check("git repo", has_git, None);
        if !has_git {
            issues.push("Not a git repository.".into());
        }

        let paths = config::repo_paths(root)?;
        let has_config = paths.config_file.exists();
        if has_config {
            doctor_check("config", true, Some(&paths.config_file));
        } else {
            println!("  {green}\u{2713}{reset} config: using defaults");
        }
    } else {
        println!("  {dim}-{reset} git repo: not in a git repository (global mode)");
        println!("  {green}\u{2713}{reset} config: using defaults");
    }

    // Check daemon binary resolution using the same strategy as runtime startup.
    let cli_version = env!("CARGO_PKG_VERSION");
    let daemon_bin = match resolve_daemon_binary() {
        Ok(path) => path,
        Err(e) => {
            issues.push(format!("Could not resolve daemon binary: {e}"));
            PathBuf::from("budi-daemon")
        }
    };
    let daemon_output = std::process::Command::new(&daemon_bin)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success());
    let daemon_bin_found = daemon_output.is_some();
    doctor_check("budi-daemon binary", daemon_bin_found, Some(&daemon_bin));
    if !daemon_bin_found {
        issues.push(format!(
            "budi-daemon binary was not executable at '{}' — copy it alongside budi, set BUDI_DAEMON_BIN, or add it to PATH",
            daemon_bin.display()
        ));
    }

    let daemon_version = daemon_output.map(|o| {
        let raw = String::from_utf8_lossy(&o.stdout);
        let trimmed = raw.trim();
        trimmed
            .strip_prefix("budi-daemon ")
            .unwrap_or(trimmed)
            .to_string()
    });
    match daemon_version {
        Some(ref dv) if dv == cli_version => {
            println!(
                "  {green}\u{2713}{reset} version: v{cli_version} (CLI and daemon match; checked via {})",
                daemon_bin.display()
            );
        }
        Some(ref dv) => {
            let yellow = super::ansi("\x1b[33m");
            println!(
                "  {yellow}!{reset} version: CLI v{cli_version} != daemon v{dv} (checked via {})",
                daemon_bin.display()
            );
            issues.push(format!(
                "Version mismatch: CLI v{cli_version} but daemon v{dv}. Run `budi update` or reinstall."
            ));
        }
        None if daemon_bin_found => {
            println!(
                "  {dim}-{reset} version: v{cli_version} (could not read daemon version via {})",
                daemon_bin.display()
            );
        }
        None => {} // Already reported as missing binary
    }

    let health = daemon_health(&config);
    doctor_check("daemon", health, None);
    if !health {
        println!("  Attempting daemon start...");
        match ensure_daemon_running(repo_root.as_deref(), &config) {
            Ok(()) => {
                let retry = daemon_health(&config);
                doctor_check("daemon (retry)", retry, None);
                if !retry {
                    issues.push(
                        "Daemon failed to start. Check logs with `RUST_LOG=debug budi doctor`."
                            .to_string(),
                    );
                }
            }
            Err(e) => {
                doctor_check("daemon start", false, None);
                println!("    {e}");
                issues.push(format!("Daemon start error: {e}"));
            }
        }
    }

    // Database file existence, readability, and integrity check
    if let Ok(db_path) = budi_core::analytics::db_path() {
        if db_path.exists() {
            match std::fs::File::open(&db_path) {
                Ok(_) => {
                    println!(
                        "  {green}\u{2713}{reset} database file: readable at {}",
                        db_path.display()
                    );
                    // Integrity check: verify DB is not corrupted
                    match budi_core::analytics::open_db(&db_path) {
                        Ok(conn) => {
                            let pragma = integrity_check_pragma(deep);
                            let mode = integrity_check_mode_label(deep);
                            match conn.query_row(pragma, [], |row| row.get::<_, String>(0)) {
                                Ok(ref result) if result == "ok" => {
                                    println!(
                                        "  {green}\u{2713}{reset} database integrity ({mode}): ok"
                                    );
                                }
                                Ok(result) => {
                                    println!(
                                        "  {red}\u{2717}{reset} database integrity ({mode}): {result}"
                                    );
                                    issues
                                        .push(format!("Database integrity check failed: {result}"));
                                }
                                Err(e) => {
                                    println!(
                                        "  {red}\u{2717}{reset} database integrity ({mode}): could not check ({e})"
                                    );
                                    issues.push(format!("Database integrity check error: {e}"));
                                }
                            }
                        }
                        Err(e) => {
                            println!(
                                "  {red}\u{2717}{reset} database open: failed ({e}). Try `budi migrate`"
                            );
                            issues.push(format!("Database cannot be opened: {e}"));
                        }
                    }
                }
                Err(e) => {
                    println!(
                        "  {red}\u{2717}{reset} database file: not readable at {} ({e})",
                        db_path.display()
                    );
                    issues.push(format!("Database file is not readable: {e}"));
                }
            }
        } else {
            println!(
                "  {dim}-{reset} database file: not yet created at {}",
                db_path.display()
            );
        }
    }

    // Disk space check (warn if < 100MB available)
    {
        let check_path = budi_core::analytics::db_path()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .or_else(|| budi_core::config::budi_home_dir().ok());
        if let Some(ref dir) = check_path {
            match check_available_disk_mb(dir) {
                Some(mb) if mb < 100 => {
                    let yellow = super::ansi("\x1b[33m");
                    println!("  {yellow}!{reset} disk space: {mb} MB available (< 100 MB)");
                    issues.push(format!("Low disk space: only {mb} MB available"));
                }
                Some(mb) => {
                    println!("  {green}\u{2713}{reset} disk space: {mb} MB available");
                }
                None => {
                    println!("  {dim}-{reset} disk space: could not determine");
                }
            }
        }
    }

    // Database schema check (via daemon if healthy, otherwise check file existence)
    if daemon_health(&config) {
        if let Ok(client) = crate::client::DaemonClient::connect()
            && let Ok(sv) = client.schema_version()
        {
            let exists = sv.get("exists").and_then(|v| v.as_bool()).unwrap_or(false);
            let current = sv.get("current").and_then(|v| v.as_u64()).unwrap_or(0);
            let target = sv.get("target").and_then(|v| v.as_u64()).unwrap_or(0);
            if !exists {
                println!("  {red}\u{2717}{reset} database: not created yet");
                issues.push("No database. Run `budi import` to create it.".into());
            } else if current == target {
                println!("  {green}\u{2713}{reset} database schema: v{}", current);
            } else {
                println!(
                    "  {red}\u{2717}{reset} database schema: v{} (needs v{})",
                    current, target
                );
                issues.push(format!(
                    "Database needs migration (v{} → v{}). Run `budi init` or `budi update`.",
                    current, target
                ));
            }
        }
    } else {
        // Daemon is down — check if the database file at least exists
        if let Ok(db_path) = budi_core::analytics::db_path() {
            if db_path.exists() {
                println!(
                    "  {dim}-{reset} database: file exists at {} (daemon down, cannot check schema)",
                    db_path.display()
                );
            } else {
                println!(
                    "  {red}\u{2717}{reset} database: not found at {}",
                    db_path.display()
                );
                issues.push("Database not found. Run `budi import` to create it.".into());
            }
        }
    }

    // Per-agent enablement status
    {
        let agents = budi_core::config::load_agents_config();
        match agents {
            Some(ref cfg) => {
                let enabled: Vec<&str> = [
                    cfg.claude_code.enabled.then_some("Claude Code"),
                    cfg.codex_cli.enabled.then_some("Codex CLI"),
                    cfg.cursor.enabled.then_some("Cursor"),
                    cfg.copilot_cli.enabled.then_some("Copilot CLI"),
                ]
                .into_iter()
                .flatten()
                .collect();
                let disabled: Vec<&str> = [
                    (!cfg.claude_code.enabled).then_some("Claude Code"),
                    (!cfg.codex_cli.enabled).then_some("Codex CLI"),
                    (!cfg.cursor.enabled).then_some("Cursor"),
                    (!cfg.copilot_cli.enabled).then_some("Copilot CLI"),
                ]
                .into_iter()
                .flatten()
                .collect();
                if !enabled.is_empty() {
                    println!(
                        "  {green}\u{2713}{reset} agents enabled: {}",
                        enabled.join(", ")
                    );
                }
                if !disabled.is_empty() {
                    println!(
                        "  {dim}-{reset} agents disabled: {} {dim}(data not collected){reset}",
                        disabled.join(", ")
                    );
                }
            }
            None => {
                println!(
                    "  {dim}-{reset} agents: no agents.toml found — all available agents enabled (legacy mode)"
                );
            }
        }
    }

    // Session visibility: catch a recurrence of R1.0.1 (#302) where assistant
    // rows exist for a window but `budi sessions` returns empty because the
    // session_id was dropped on write. Mismatch is a hard error for the
    // developer-first story, so we add it to `issues`.
    if let Ok(db_path) = budi_core::analytics::db_path()
        && db_path.exists()
        && let Ok(conn) = budi_core::analytics::open_db(&db_path)
    {
        match budi_core::analytics::session_visibility(&conn) {
            Ok(windows) => {
                let yellow = super::ansi("\x1b[33m");
                let mut any_mismatch = false;
                if windows.iter().any(|w| w.assistant_messages > 0) {
                    has_any_assistant_activity = true;
                }
                for window in &windows {
                    let mark = if window.has_mismatch() {
                        any_mismatch = true;
                        format!("{red}\u{2717}{reset}")
                    } else if window.assistant_messages == 0 {
                        format!("{dim}-{reset}")
                    } else if window.assistant_messages_with_session < window.assistant_messages {
                        format!("{yellow}!{reset}")
                    } else {
                        format!("{green}\u{2713}{reset}")
                    };
                    println!(
                        "  {mark} sessions visibility ({}): assistant={} with_session={} distinct={} returned={}",
                        window.label,
                        window.assistant_messages,
                        window.assistant_messages_with_session,
                        window.distinct_sessions,
                        window.returned_sessions,
                    );
                }
                if any_mismatch {
                    issues.push(
                        "Sessions visibility mismatch: assistant messages exist in a window but `budi sessions` returns none. See #302.".into(),
                    );
                }
            }
            Err(e) => {
                println!("  {dim}-{reset} sessions visibility: could not compute ({e})");
            }
        }
    }

    // Branch attribution: catch a recurrence of R1.0.2 (#303) where live
    // proxy traffic lands in the database without `git_branch`, collapsing
    // `budi stats --branches` into `(untagged)`. A single provider with a
    // significant missing-branch ratio points at a broken attribution path
    // for that provider even if totals are healthy overall.
    if let Ok(db_path) = budi_core::analytics::db_path()
        && db_path.exists()
        && let Ok(conn) = budi_core::analytics::open_db(&db_path)
    {
        match budi_core::analytics::branch_attribution_stats(&conn) {
            Ok(stats) if stats.is_empty() => {
                println!("  {dim}-{reset} branch attribution (7d): no assistant activity yet");
            }
            Ok(stats) => {
                let yellow = super::ansi("\x1b[33m");
                let mut any_red = false;
                for row in &stats {
                    let pct = row.missing_branch_ratio() * 100.0;
                    let mark = if pct > 50.0 {
                        any_red = true;
                        format!("{red}\u{2717}{reset}")
                    } else if pct > 10.0 {
                        format!("{yellow}!{reset}")
                    } else {
                        format!("{green}\u{2713}{reset}")
                    };
                    println!(
                        "  {mark} branch attribution ({}, 7d): assistant={} missing_branch={} ({:.0}%) missing_repo={} missing_cwd={}",
                        row.provider,
                        row.total_assistant,
                        row.missing_branch,
                        pct,
                        row.missing_repo,
                        row.missing_cwd,
                    );
                }
                if any_red {
                    issues.push(
                        "Branch attribution is broken for at least one provider (>50% of assistant rows have no git_branch). `budi stats --branches` will show `(untagged)`. See #303.".into(),
                    );
                }
            }
            Err(e) => {
                println!("  {dim}-{reset} branch attribution: could not compute ({e})");
            }
        }
    }

    // Activity attribution: surface a recurrence of a silent classifier
    // regression (#305 ships `activity` as a first-class dimension; if the
    // classifier breaks, `budi stats --activities` collapses into
    // `(untagged)` without anything else catching it). 100% missing for
    // a provider with traffic is almost always a bug; a mid-range ratio
    // is expected because short prompts and slash commands never carry
    // an `activity` tag by design.
    if let Ok(db_path) = budi_core::analytics::db_path()
        && db_path.exists()
        && let Ok(conn) = budi_core::analytics::open_db(&db_path)
    {
        match budi_core::analytics::activity_attribution_stats(&conn) {
            Ok(stats) if stats.is_empty() => {
                println!("  {dim}-{reset} activity attribution (7d): no assistant activity yet");
            }
            Ok(stats) => {
                let yellow = super::ansi("\x1b[33m");
                let mut any_red = false;
                for row in &stats {
                    let pct = row.missing_activity_ratio() * 100.0;
                    // Only flag fully-silent providers with non-trivial
                    // traffic — enough volume to rule out "everybody
                    // typed a one-word prompt" coincidence.
                    let fully_silent = pct >= 99.9 && row.total_assistant >= 5;
                    let mark = if fully_silent {
                        any_red = true;
                        format!("{red}\u{2717}{reset}")
                    } else if pct > 90.0 {
                        format!("{yellow}!{reset}")
                    } else {
                        format!("{green}\u{2713}{reset}")
                    };
                    println!(
                        "  {mark} activity attribution ({}, 7d): assistant={} missing_activity={} ({:.0}%)",
                        row.provider, row.total_assistant, row.missing_activity, pct,
                    );
                }
                if any_red {
                    issues.push(
                        "Activity classification is silent for at least one provider (100% of recent assistant rows have no `activity` tag). `budi stats --activities` will show only `(untagged)`. See #305.".into(),
                    );
                }
            }
            Err(e) => {
                println!("  {dim}-{reset} activity attribution: could not compute ({e})");
            }
        }
    }

    // Auto-proxy configuration checks (shell profile + IDE config files)
    {
        let agents = budi_core::config::load_agents_config()
            .unwrap_or_else(budi_core::config::AgentsConfig::all_enabled);
        let proxy_issues =
            super::proxy_install::doctor_auto_proxy_issues(&agents, config.proxy.effective_port());
        if proxy_issues.is_empty() {
            println!(
                "  {green}\u{2713}{reset} auto-proxy config: shell profile and IDE settings look good"
            );
        } else {
            println!(
                "  {red}\u{2717}{reset} auto-proxy config: {} issue(s)",
                proxy_issues.len()
            );
            for issue in proxy_issues {
                println!("    - {issue}");
                issues.push(issue);
            }
        }
    }

    // Proxy health check
    {
        let proxy_port = config.proxy.effective_port();
        let proxy_enabled = config.proxy.effective_enabled();
        if proxy_enabled {
            let proxy_ok = check_proxy_port(proxy_port);
            if proxy_ok {
                println!("  {green}\u{2713}{reset} proxy: running on port {proxy_port}");
            } else {
                println!("  {red}\u{2717}{reset} proxy: not responding on port {proxy_port}");
                issues.push(format!(
                    "Proxy not running on port {proxy_port}. Start budi daemon with `budi init`."
                ));
            }
        } else {
            println!("  {dim}-{reset} proxy: disabled in config");
        }
    }

    // Autostart service check
    {
        let mechanism = budi_core::autostart::service_mechanism();
        let status = budi_core::autostart::service_status();
        match status {
            budi_core::autostart::ServiceStatus::Running => {
                println!("  {green}\u{2713}{reset} autostart: {status} ({mechanism})");
            }
            budi_core::autostart::ServiceStatus::Installed => {
                let yellow = super::ansi("\x1b[33m");
                println!("  {yellow}!{reset} autostart: {status} ({mechanism})");
            }
            budi_core::autostart::ServiceStatus::NotInstalled => {
                println!("  {red}\u{2717}{reset} autostart: {status} ({mechanism})");
                issues.push(
                    "Autostart service not installed. Run `budi autostart install` to install it."
                        .into(),
                );
            }
        }
    }

    println!();
    if issues.is_empty() {
        println!("All checks passed.");
        if !has_any_assistant_activity {
            println!();
            println!(
                "  {dim}No assistant activity yet. Open your agent (`claude`, `codex`, `cursor`, `gh copilot`) and send a prompt — then re-run `budi doctor` to see attribution health.{reset}"
            );
        }
    } else {
        println!("Issues found:");
        for issue in &issues {
            println!("  - {issue}");
        }
        anyhow::bail!("{} issue(s) found", issues.len());
    }
    Ok(())
}

fn doctor_check(label: &str, ok: bool, path: Option<&Path>) {
    let (mark, color) = if ok {
        ("\u{2713}", "\x1b[32m")
    } else {
        ("\u{2717}", "\x1b[31m")
    };
    let c = super::ansi(color);
    let reset = super::ansi("\x1b[0m");
    if let Some(p) = path {
        println!("  {c}{mark}{reset} {label}: {}", p.display());
    } else {
        println!("  {c}{mark}{reset} {label}");
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

/// TCP probe to check if the proxy is listening on the given port.
fn check_proxy_port(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        std::time::Duration::from_millis(500),
    )
    .is_ok()
}

/// Check available disk space in MB. Uses `df -k` on Unix, skips on Windows.
fn check_available_disk_mb(path: &Path) -> Option<u64> {
    if cfg!(target_os = "windows") {
        // df is not available on Windows; skip disk space check.
        return None;
    }
    let output = std::process::Command::new("df")
        .arg("-k")
        .arg(path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    // df -k output: second line, fourth column is available KB
    let line = stdout.lines().nth(1)?;
    let available_kb: u64 = line.split_whitespace().nth(3)?.parse().ok()?;
    Some(available_kb / 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
