//! `budi hook` — receive hook events from Claude Code and Cursor via stdin,
//! POST them to the daemon. Fire-and-forget: never errors or slows the host editor.

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

    // Fire-and-forget with a short timeout. Silent on all errors.
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build();
    if let Ok(client) = client {
        let _ = client
            .post(&url)
            .header("Content-Type", "application/json")
            .body(input)
            .send();
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
