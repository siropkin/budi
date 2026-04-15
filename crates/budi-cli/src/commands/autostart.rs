use anyhow::Result;
use budi_core::autostart;
use budi_core::config;

use crate::daemon;

/// `budi autostart status` — show current autostart state.
pub fn cmd_autostart_status() -> Result<()> {
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
