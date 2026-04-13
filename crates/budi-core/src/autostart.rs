//! Platform-native daemon autostart management.
//!
//! Installs a user-level service so `budi-daemon` starts automatically at login:
//! - macOS: launchd LaunchAgent
//! - Linux: systemd user service
//! - Windows: Task Scheduler
//!
//! See ADR-0087 §8 for the design rationale.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

/// Reverse-DNS label used for the launchd LaunchAgent on macOS.
#[cfg(target_os = "macos")]
const LAUNCHD_LABEL: &str = "dev.getbudi.budi-daemon";

/// Systemd unit name (without the `.service` suffix).
#[cfg(target_os = "linux")]
const SYSTEMD_UNIT: &str = "budi-daemon";

/// Windows Task Scheduler task name.
#[cfg(target_os = "windows")]
const SCHTASK_NAME: &str = "BudiDaemon";

/// Whether the autostart service is installed and/or running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceStatus {
    /// No service file found.
    NotInstalled,
    /// Service file exists but the daemon process is not running.
    Installed,
    /// Service file exists and the daemon process is running.
    Running,
}

impl std::fmt::Display for ServiceStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServiceStatus::NotInstalled => write!(f, "not installed"),
            ServiceStatus::Installed => write!(f, "installed (not running)"),
            ServiceStatus::Running => write!(f, "installed and running"),
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Install the autostart service for the current platform.
///
/// `daemon_bin` is the absolute path to the `budi-daemon` binary.
/// The service is configured to start the daemon with the given host and port
/// at user login, and restart on crash with backoff.
///
/// Idempotent — safe to call multiple times (overwrites existing service file).
pub fn install_service(daemon_bin: &Path, host: &str, port: u16) -> Result<()> {
    #[cfg(target_os = "macos")]
    return install_launchd(daemon_bin, host, port);

    #[cfg(target_os = "linux")]
    return install_systemd(daemon_bin, host, port);

    #[cfg(target_os = "windows")]
    return install_schtask(daemon_bin, host, port);

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = (daemon_bin, host, port);
        anyhow::bail!("Autostart is not supported on this platform")
    }
}

/// Uninstall the autostart service. Returns `true` if a service was found and removed.
pub fn uninstall_service() -> Result<bool> {
    #[cfg(target_os = "macos")]
    return uninstall_launchd();

    #[cfg(target_os = "linux")]
    return uninstall_systemd();

    #[cfg(target_os = "windows")]
    return uninstall_schtask();

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    Ok(false)
}

/// Check whether the autostart service is installed and/or running.
pub fn service_status() -> ServiceStatus {
    #[cfg(target_os = "macos")]
    return launchd_status();

    #[cfg(target_os = "linux")]
    return systemd_status();

    #[cfg(target_os = "windows")]
    return schtask_status();

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    ServiceStatus::NotInstalled
}

/// Return the path where the service file is (or would be) installed.
pub fn service_file_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    return launchd_plist_path().ok();

    #[cfg(target_os = "linux")]
    return systemd_unit_path().ok();

    #[cfg(target_os = "windows")]
    return None; // Task Scheduler has no single file path

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    None
}

/// Return a human-readable description of the service mechanism for this platform.
pub fn service_mechanism() -> &'static str {
    #[cfg(target_os = "macos")]
    return "launchd LaunchAgent";

    #[cfg(target_os = "linux")]
    return "systemd user service";

    #[cfg(target_os = "windows")]
    return "Task Scheduler";

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    "unsupported"
}

/// Clean up legacy service files from older budi versions.
/// Returns `true` if any legacy files were removed.
pub fn cleanup_legacy_services() -> bool {
    #[cfg(target_os = "macos")]
    return cleanup_legacy_launchd();

    #[cfg(not(target_os = "macos"))]
    false
}

// ===========================================================================
// macOS — launchd LaunchAgent
// ===========================================================================

#[cfg(target_os = "macos")]
fn launchd_plist_path() -> Result<PathBuf> {
    let home = crate::config::home_dir()?;
    Ok(home.join("Library/LaunchAgents/dev.getbudi.budi-daemon.plist"))
}

#[cfg(target_os = "macos")]
fn launchd_log_path() -> Result<PathBuf> {
    let home = crate::config::home_dir()?;
    Ok(home.join("Library/Logs/budi-daemon.log"))
}

#[cfg(target_os = "macos")]
fn install_launchd(daemon_bin: &Path, host: &str, port: u16) -> Result<()> {
    let plist_path = launchd_plist_path()?;
    let log_path = launchd_log_path()?;

    // Ensure parent directories exist
    if let Some(parent) = plist_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }

    // Unload existing service before overwriting (ignore errors — may not be loaded)
    if plist_path.exists() {
        let _ = Command::new("launchctl")
            .args(["unload", &plist_path.to_string_lossy()])
            .output();
    }

    let plist = generate_launchd_plist(daemon_bin, host, port, &log_path);
    std::fs::write(&plist_path, plist)
        .with_context(|| format!("Failed to write {}", plist_path.display()))?;

    // Load the service
    let output = Command::new("launchctl")
        .args(["load", &plist_path.to_string_lossy()])
        .output()
        .context("Failed to run launchctl load")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("launchctl load failed: {stderr}");
    }

    // Clean up legacy service files from older versions
    cleanup_legacy_launchd();

    Ok(())
}

#[cfg(target_os = "macos")]
fn generate_launchd_plist(daemon_bin: &Path, host: &str, port: u16, log_path: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{bin}</string>
        <string>serve</string>
        <string>--host</string>
        <string>{host}</string>
        <string>--port</string>
        <string>{port}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ThrottleInterval</key>
    <integer>10</integer>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#,
        label = LAUNCHD_LABEL,
        bin = daemon_bin.display(),
        host = host,
        port = port,
        log = log_path.display(),
    )
}

#[cfg(target_os = "macos")]
fn uninstall_launchd() -> Result<bool> {
    let plist_path = launchd_plist_path()?;
    if !plist_path.exists() {
        // Also clean up legacy files
        let cleaned = cleanup_legacy_launchd();
        return Ok(cleaned);
    }

    // Unload the service
    let _ = Command::new("launchctl")
        .args(["unload", &plist_path.to_string_lossy()])
        .output();

    std::fs::remove_file(&plist_path)
        .with_context(|| format!("Failed to remove {}", plist_path.display()))?;

    // Also clean up legacy files
    cleanup_legacy_launchd();

    Ok(true)
}

#[cfg(target_os = "macos")]
fn launchd_status() -> ServiceStatus {
    let plist_path = match launchd_plist_path() {
        Ok(p) => p,
        Err(_) => return ServiceStatus::NotInstalled,
    };
    if !plist_path.exists() {
        return ServiceStatus::NotInstalled;
    }

    // Check if the service is loaded and running via launchctl list
    let output = Command::new("launchctl")
        .args(["list", LAUNCHD_LABEL])
        .output();

    match output {
        Ok(o) if o.status.success() => ServiceStatus::Running,
        _ => ServiceStatus::Installed,
    }
}

/// Remove legacy LaunchAgent plists from older budi versions (com.siropkin.budi.*).
#[cfg(target_os = "macos")]
fn cleanup_legacy_launchd() -> bool {
    let home = match crate::config::home_dir() {
        Ok(h) => h,
        Err(_) => return false,
    };
    let launch_agents_dir = home.join("Library/LaunchAgents");
    let entries = match std::fs::read_dir(&launch_agents_dir) {
        Ok(e) => e,
        Err(_) => return false,
    };

    let mut removed_any = false;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with("com.siropkin.budi.") && name_str.ends_with(".plist") {
            let _ = Command::new("launchctl")
                .args(["unload", &entry.path().to_string_lossy()])
                .output();
            if std::fs::remove_file(entry.path()).is_ok() {
                removed_any = true;
            }
        }
    }
    removed_any
}

// ===========================================================================
// Linux — systemd user service
// ===========================================================================

#[cfg(target_os = "linux")]
fn systemd_unit_path() -> Result<PathBuf> {
    let home = crate::config::home_dir()?;
    Ok(home.join(format!(".config/systemd/user/{SYSTEMD_UNIT}.service")))
}

#[cfg(target_os = "linux")]
fn install_systemd(daemon_bin: &Path, host: &str, port: u16) -> Result<()> {
    let unit_path = systemd_unit_path()?;

    if let Some(parent) = unit_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }

    let unit = generate_systemd_unit(daemon_bin, host, port);
    std::fs::write(&unit_path, unit)
        .with_context(|| format!("Failed to write {}", unit_path.display()))?;

    // Reload and enable
    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .output();

    let output = Command::new("systemctl")
        .args(["--user", "enable", "--now", SYSTEMD_UNIT])
        .output()
        .context("Failed to run systemctl --user enable")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // systemctl may fail in containers without a real user session — warn but don't bail
        tracing::warn!("systemctl --user enable failed (may be expected in containers): {stderr}");
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn generate_systemd_unit(daemon_bin: &Path, host: &str, port: u16) -> String {
    format!(
        r#"[Unit]
Description=budi daemon — AI cost analytics proxy
After=network.target

[Service]
ExecStart={bin} serve --host {host} --port {port}
Restart=on-failure
RestartSec=5
Environment=RUST_LOG=warn

[Install]
WantedBy=default.target
"#,
        bin = daemon_bin.display(),
        host = host,
        port = port,
    )
}

#[cfg(target_os = "linux")]
fn uninstall_systemd() -> Result<bool> {
    let unit_path = systemd_unit_path()?;
    if !unit_path.exists() {
        return Ok(false);
    }

    let _ = Command::new("systemctl")
        .args(["--user", "disable", "--now", SYSTEMD_UNIT])
        .output();

    std::fs::remove_file(&unit_path)
        .with_context(|| format!("Failed to remove {}", unit_path.display()))?;

    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .output();

    Ok(true)
}

#[cfg(target_os = "linux")]
fn systemd_status() -> ServiceStatus {
    let unit_path = match systemd_unit_path() {
        Ok(p) => p,
        Err(_) => return ServiceStatus::NotInstalled,
    };
    if !unit_path.exists() {
        return ServiceStatus::NotInstalled;
    }

    let output = Command::new("systemctl")
        .args(["--user", "is-active", SYSTEMD_UNIT])
        .output();

    match output {
        Ok(o) if o.status.success() => ServiceStatus::Running,
        _ => ServiceStatus::Installed,
    }
}

// ===========================================================================
// Windows — Task Scheduler
// ===========================================================================

#[cfg(target_os = "windows")]
fn install_schtask(daemon_bin: &Path, host: &str, port: u16) -> Result<()> {
    let bin = daemon_bin.to_string_lossy();
    let tr = format!("\"{bin}\" serve --host {host} --port {port}");

    let output = Command::new("schtasks")
        .args([
            "/Create",
            "/SC",
            "ONLOGON",
            "/TN",
            SCHTASK_NAME,
            "/TR",
            &tr,
            "/RL",
            "LIMITED",
            "/F", // overwrite existing
        ])
        .output()
        .context("Failed to run schtasks /Create")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("schtasks /Create failed: {stderr}");
    }

    Ok(())
}

#[cfg(target_os = "windows")]
fn uninstall_schtask() -> Result<bool> {
    let output = Command::new("schtasks")
        .args(["/Delete", "/TN", SCHTASK_NAME, "/F"])
        .output()
        .context("Failed to run schtasks /Delete")?;

    Ok(output.status.success())
}

#[cfg(target_os = "windows")]
fn schtask_status() -> ServiceStatus {
    let output = Command::new("schtasks")
        .args(["/Query", "/TN", SCHTASK_NAME])
        .output();

    match output {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            if stdout.contains("Running") {
                ServiceStatus::Running
            } else {
                ServiceStatus::Installed
            }
        }
        _ => ServiceStatus::NotInstalled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_status_display() {
        assert_eq!(ServiceStatus::NotInstalled.to_string(), "not installed");
        assert_eq!(
            ServiceStatus::Installed.to_string(),
            "installed (not running)"
        );
        assert_eq!(ServiceStatus::Running.to_string(), "installed and running");
    }

    #[test]
    fn service_mechanism_returns_known_value() {
        let mech = service_mechanism();
        assert!(
            [
                "launchd LaunchAgent",
                "systemd user service",
                "Task Scheduler",
                "unsupported"
            ]
            .contains(&mech)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn launchd_plist_contains_expected_fields() {
        let log = PathBuf::from("/Users/test/Library/Logs/budi-daemon.log");
        let plist = generate_launchd_plist(
            Path::new("/usr/local/bin/budi-daemon"),
            "127.0.0.1",
            7878,
            &log,
        );
        assert!(plist.contains("<string>dev.getbudi.budi-daemon</string>"));
        assert!(plist.contains("<string>/usr/local/bin/budi-daemon</string>"));
        assert!(plist.contains("<string>serve</string>"));
        assert!(plist.contains("<string>127.0.0.1</string>"));
        assert!(plist.contains("<string>7878</string>"));
        assert!(plist.contains("<key>RunAtLoad</key>"));
        assert!(plist.contains("<key>KeepAlive</key>"));
        assert!(plist.contains("<key>ThrottleInterval</key>"));
        assert!(plist.contains("<string>/Users/test/Library/Logs/budi-daemon.log</string>"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn launchd_plist_path_under_launch_agents() {
        let path = launchd_plist_path().unwrap();
        assert!(
            path.to_string_lossy()
                .contains("Library/LaunchAgents/dev.getbudi.budi-daemon.plist")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn systemd_unit_contains_expected_fields() {
        let unit = generate_systemd_unit(
            Path::new("/home/test/.local/bin/budi-daemon"),
            "127.0.0.1",
            7878,
        );
        assert!(
            unit.contains("/home/test/.local/bin/budi-daemon serve --host 127.0.0.1 --port 7878")
        );
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("RestartSec=5"));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn systemd_unit_path_under_config_dir() {
        let path = systemd_unit_path().unwrap();
        assert!(
            path.to_string_lossy()
                .contains(".config/systemd/user/budi-daemon.service")
        );
    }
}
