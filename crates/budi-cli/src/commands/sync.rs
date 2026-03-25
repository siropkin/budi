use anyhow::Result;

use crate::client::DaemonClient;

pub fn init_auto_sync() -> Result<(usize, usize)> {
    let client = DaemonClient::connect()?;
    let result = client.sync(true)?;
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

    println!("Syncing recent transcripts...");
    let result = client.sync(true)?;

    print_sync_result(&result);
    Ok(())
}

pub fn cmd_history() -> Result<()> {
    let client = DaemonClient::connect()?;

    println!("Loading full transcript history (this may take a while)...");
    let result = client.history()?;

    print_sync_result(&result);
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
        println!(
            "Synced \x1b[1m{}\x1b[0m new messages from \x1b[1m{}\x1b[0m files.",
            messages_ingested, files_synced
        );
    }
}
