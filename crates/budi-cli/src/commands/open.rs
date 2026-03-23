use std::process::Command;

use anyhow::Result;
use budi_core::config;

pub fn cmd_open() -> Result<()> {
    let url = format!(
        "http://{}:{}/dashboard",
        config::DEFAULT_DAEMON_HOST,
        config::DEFAULT_DAEMON_PORT,
    );
    println!("{}", url);
    // Try to open in browser
    let _ = Command::new("open").arg(&url).spawn();
    Ok(())
}
