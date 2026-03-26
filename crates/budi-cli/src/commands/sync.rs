use std::io::Write;
use std::time::Instant;

use anyhow::Result;

use crate::client::DaemonClient;

pub fn init_quick_sync() -> Result<(usize, usize)> {
    let client = DaemonClient::connect()?;
    let start = std::time::Instant::now();
    let result = client.sync(true)?;
    let elapsed = start.elapsed().as_secs_f64();
    let files = result
        .get("files_synced")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;
    let msgs = result
        .get("messages_ingested")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;
    let bold = super::ansi("\x1b[1m");
    let reset = super::ansi("\x1b[0m");
    println!(
        "  Sync: done in {bold}{:.1}s{reset} ({} messages from {} files)",
        elapsed, msgs, files
    );
    print_sync_warnings(&result);
    Ok((files, msgs))
}

pub fn init_full_sync() -> Result<(usize, usize)> {
    let client = DaemonClient::connect()?;
    let start = std::time::Instant::now();
    let result = client.history()?;
    let elapsed = start.elapsed().as_secs_f64();
    let files = result
        .get("files_synced")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;
    let msgs = result
        .get("messages_ingested")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;
    let bold = super::ansi("\x1b[1m");
    let reset = super::ansi("\x1b[0m");
    println!(
        "  Sync: done in {bold}{:.1}s{reset} ({} messages from {} files)",
        elapsed, msgs, files
    );
    print_sync_warnings(&result);
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
    print_sync_warnings(&result);
    Ok(())
}

pub fn cmd_history() -> Result<()> {
    let client = DaemonClient::connect()?;

    print!("Syncing full history (this may take a while)...");
    let _ = std::io::stdout().flush();
    let start = Instant::now();
    let result = client.history()?;
    let elapsed = start.elapsed().as_secs_f64();
    println!(" done in {:.1}s", elapsed);

    let files = result
        .get("files_synced")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let msgs = result
        .get("messages_ingested")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let bold = super::ansi("\x1b[1m");
    let green = super::ansi("\x1b[32m");
    let reset = super::ansi("\x1b[0m");

    println!(
        "{green}✓{reset} {bold}{}{reset} messages from {bold}{}{reset} files.",
        msgs, files
    );
    print_sync_warnings(&result);
    Ok(())
}

fn print_sync_warnings(result: &serde_json::Value) {
    if let Some(warnings) = result.get("warnings").and_then(|v| v.as_array()) {
        let yellow = super::ansi("\x1b[33m");
        let reset = super::ansi("\x1b[0m");
        for w in warnings {
            let msg = w.as_str().map(|s| s.to_string()).unwrap_or_else(|| {
                serde_json::to_string(w).unwrap_or_else(|_| "(unparseable)".to_string())
            });
            eprintln!("{yellow}Warning:{reset} {}", msg);
        }
    }
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

    let bold = super::ansi("\x1b[1m");
    let green = super::ansi("\x1b[32m");
    let reset = super::ansi("\x1b[0m");

    if files_synced == 0 && messages_ingested == 0 {
        println!("Already up to date.");
    } else {
        println!(
            "{green}Done.{reset} {bold}{}{reset} files, {bold}{}{reset} messages.",
            files_synced, messages_ingested
        );
    }
}
