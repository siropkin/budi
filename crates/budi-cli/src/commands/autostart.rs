use anyhow::Result;
use budi_core::autostart;
use budi_core::config;
use serde::Serialize;

use crate::StatsFormat;
use crate::daemon;

/// `budi autostart status` — show current autostart state.
pub fn cmd_autostart_status(format: StatsFormat) -> Result<()> {
    if matches!(format, StatsFormat::Json) {
        return cmd_autostart_status_json();
    }
    cmd_autostart_status_text()
}

fn cmd_autostart_status_text() -> Result<()> {
    let mechanism = autostart::service_mechanism();
    let status = autostart::service_status();
    let green = super::ansi("\x1b[32m");
    let yellow = super::ansi("\x1b[33m");
    let red = super::ansi("\x1b[31m");
    let reset = super::ansi("\x1b[0m");

    match status {
        autostart::ServiceStatus::Running => {
            println!("{green}✓{reset} Autostart: {status} ({mechanism})");
        }
        autostart::ServiceStatus::Installed => {
            println!("{yellow}!{reset} Autostart: {status} ({mechanism})");
            println!("  The service file exists but the daemon is not running.");
            println!("  Try: budi autostart install (to re-register and start)");
        }
        autostart::ServiceStatus::NotInstalled => {
            println!("{red}✗{reset} Autostart: {status}");
            println!("  Run: budi autostart install");
        }
    }

    if let Some(path) = autostart::service_file_path() {
        println!("  Service file: {}", path.display());
    }

    Ok(())
}

#[derive(Debug, Serialize)]
struct AutostartStatusJson {
    installed: bool,
    running: bool,
    /// Service file path on disk. `null` on platforms (Windows Task Scheduler)
    /// where the mechanism doesn't expose a single backing file.
    service_path: Option<String>,
    /// Stable platform-keyword tag: `launchd` (macOS), `systemd` (Linux),
    /// `task_scheduler` (Windows), or `unsupported` on other platforms.
    platform: &'static str,
}

fn cmd_autostart_status_json() -> Result<()> {
    let status = autostart::service_status();
    let body = AutostartStatusJson {
        installed: matches!(
            status,
            autostart::ServiceStatus::Installed | autostart::ServiceStatus::Running
        ),
        running: matches!(status, autostart::ServiceStatus::Running),
        service_path: autostart::service_file_path().map(|p| p.display().to_string()),
        platform: autostart_platform_tag(),
    };
    super::print_json(&body)?;
    Ok(())
}

/// Stable platform keyword for the JSON contract — separate from
/// `service_mechanism()` (which returns a free-form human label like
/// `"launchd LaunchAgent"`) so scripted callers can dispatch on a fixed
/// vocabulary.
fn autostart_platform_tag() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "launchd"
    }
    #[cfg(target_os = "linux")]
    {
        "systemd"
    }
    #[cfg(target_os = "windows")]
    {
        "task_scheduler"
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        "unsupported"
    }
}

/// `budi autostart install` — install the autostart service.
pub fn cmd_autostart_install() -> Result<()> {
    let green = super::ansi("\x1b[32m");
    let yellow = super::ansi("\x1b[33m");
    let reset = super::ansi("\x1b[0m");

    let daemon_bin = match daemon::resolve_daemon_binary() {
        Ok(p) => p,
        Err(e) => {
            anyhow::bail!("Could not resolve daemon binary: {e}");
        }
    };

    let repo_root = super::try_resolve_repo_root(None);
    let cfg = match &repo_root {
        Some(root) => config::load_or_default(root)?,
        None => config::BudiConfig::default(),
    };

    match autostart::install_service(&daemon_bin, &cfg.daemon_host, cfg.daemon_port) {
        Ok(()) => {
            let mechanism = autostart::service_mechanism();
            println!("{green}✓{reset} Autostart service installed ({mechanism}).");
            if let Some(path) = autostart::service_file_path() {
                println!("  Service file: {}", path.display());
            }
            println!("  The daemon will start automatically at login.");
        }
        Err(e) => {
            eprintln!("{yellow}Warning:{reset} autostart install failed: {e}");
            anyhow::bail!("Autostart installation failed. Check permissions and try again.");
        }
    }

    Ok(())
}

/// `budi autostart uninstall` — remove the autostart service.
pub fn cmd_autostart_uninstall() -> Result<()> {
    let green = super::ansi("\x1b[32m");
    let reset = super::ansi("\x1b[0m");

    match autostart::uninstall_service() {
        Ok(true) => {
            println!("{green}✓{reset} Autostart service removed.");
            println!("  The daemon will no longer start automatically at login.");
            println!("  To re-enable: budi autostart install");
        }
        Ok(false) => {
            println!("No autostart service found. Nothing to remove.");
        }
        Err(e) => {
            anyhow::bail!("Failed to remove autostart service: {e}");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lock the keys + value shape of the `--format json` contract so a
    /// future refactor can't silently rename a field that scripted
    /// callers grep for.
    #[test]
    fn autostart_status_json_locks_schema() {
        let body = AutostartStatusJson {
            installed: true,
            running: true,
            service_path: Some(
                "/Users/test/Library/LaunchAgents/dev.getbudi.budi-daemon.plist".to_string(),
            ),
            platform: "launchd",
        };
        let v = serde_json::to_value(&body).expect("serialise");
        let obj = v.as_object().expect("object");
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(
            keys,
            vec!["installed", "platform", "running", "service_path"]
        );
        assert!(v["installed"].is_boolean());
        assert!(v["running"].is_boolean());
        assert!(v["service_path"].is_string());
        assert!(v["platform"].is_string());
    }

    #[test]
    fn autostart_status_json_emits_null_service_path_when_absent() {
        // Windows Task Scheduler has no single backing file. The JSON
        // contract surfaces that as `null`, not as an empty string.
        let body = AutostartStatusJson {
            installed: false,
            running: false,
            service_path: None,
            platform: "task_scheduler",
        };
        let v = serde_json::to_value(&body).expect("serialise");
        assert!(v["service_path"].is_null());
    }

    #[test]
    fn autostart_platform_tag_is_keyword_only() {
        // The JSON contract promises a fixed vocabulary; the tag must
        // be a single word with no spaces (unlike `service_mechanism()`
        // which returns "launchd LaunchAgent" / "systemd user service").
        let tag = autostart_platform_tag();
        assert!(
            !tag.contains(' '),
            "autostart_platform_tag must be a single keyword, got {tag:?}"
        );
        assert!(matches!(
            tag,
            "launchd" | "systemd" | "task_scheduler" | "unsupported"
        ));
    }
}
