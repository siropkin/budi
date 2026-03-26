use std::io::Write;
use std::time::Instant;

use anyhow::Result;

use crate::client::DaemonClient;

pub fn init_auto_sync() -> Result<(usize, usize)> {
    let client = DaemonClient::connect()?;
    // Full history sync on first install so dashboard has all data
    let result = client.history()?;
    let files = result
        .get("files_synced")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;
    let msgs = result
        .get("messages_ingested")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;
    Ok((files, msgs))
}

pub fn cmd_sync() -> Result<()> {
    let client = DaemonClient::connect()?;

    print!("Syncing recent transcripts...");
    let _ = std::io::stdout().flush();
    let start = Instant::now();
    let result = client.sync(true)?;
    println!(" done in {:.1}s", start.elapsed().as_secs_f64());

    print_sync_result(&result);
    Ok(())
}

pub fn cmd_history() -> Result<()> {
    let client = DaemonClient::connect()?;

    println!("Syncing full history (this may take a while)...");
    let _ = std::io::stdout().flush();
    let start = Instant::now();
    let result = client.history()?;
    let elapsed = start.elapsed().as_secs_f64();

    let files = result.get("files_synced").and_then(|v| v.as_u64()).unwrap_or(0);
    let msgs = result.get("messages_ingested").and_then(|v| v.as_u64()).unwrap_or(0);

    let bold = super::ansi("\x1b[1m");
    let green = super::ansi("\x1b[32m");
    let reset = super::ansi("\x1b[0m");

    println!("{green}✓{reset} Full history sync complete in {:.1}s.", elapsed);
    println!(
        "  {bold}{}{reset} messages from {bold}{}{reset} files.",
        msgs, files
    );
    Ok(())
}

fn print_sync_result(result: &serde_json::Value) {
    let files_synced = result
        .get("files_synced")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let messages_ingested = result
        .get("messages_ingested")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    if files_synced == 0 && messages_ingested == 0 {
        println!("Already up to date.");
    } else {
        let bold = super::ansi("\x1b[1m");
        let reset = super::ansi("\x1b[0m");
        println!(
            "Synced {bold}{}{reset} new messages from {bold}{}{reset} files.",
            messages_ingested, files_synced
        );
    }
}
