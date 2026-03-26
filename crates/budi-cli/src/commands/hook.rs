//! `budi hook` — receive hook events from Claude Code and Cursor via stdin,
//! POST them to the daemon. Fire-and-forget: never errors or slows the host editor.
//!
//! Set BUDI_HOOK_DEBUG=1 to log failures to ~/.local/share/budi/hook-debug.log.

use std::io::Read;

use budi_core::config;

pub fn cmd_hook() -> anyhow::Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    if input.trim().is_empty() {
        return Ok(());
    }

    let base_url = load_daemon_url();
    let url = format!("{base_url}/hooks/ingest");

    // Fire-and-forget with a short timeout. Silent on all errors unless debug mode.
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build();
    let result = match client {
        Ok(client) => client
            .post(&url)
            .header("Content-Type", "application/json")
            .body(input)
            .send()
            .map(|_| ())
            .map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    };

    // Debug logging: set BUDI_HOOK_DEBUG=1 to diagnose hook delivery issues.
    // Logs are written to ~/.local/share/budi/hook-debug.log.
    // Mention this env var in `budi doctor` output when hooks look misconfigured.
    if let Err(ref err) = result
        && std::env::var("BUDI_HOOK_DEBUG").is_ok_and(|v| v == "1")
        && let Ok(log_dir) = config::budi_home_dir()
    {
        let log_path = log_dir.join("hook-debug.log");
        let ts = chrono::Utc::now().to_rfc3339();
        let line = format!("[{ts}] hook POST to {url} failed: {err}\n");
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
    }

    Ok(())
}

/// Load daemon URL from config, falling back to defaults.
fn load_daemon_url() -> String {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| config::find_repo_root(&cwd).ok())
        .and_then(|root| config::load_or_default(&root).ok())
        .unwrap_or_default()
        .daemon_base_url()
}
