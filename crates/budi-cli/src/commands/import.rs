use std::io::Write;
use std::time::Instant;

use anyhow::Result;

use crate::client::DaemonClient;

pub fn cmd_import(force: bool) -> Result<()> {
    let client = DaemonClient::connect()?;

    if force {
        print!("Force re-importing all data (this may take a while)...");
    } else {
        print!("Importing historical transcripts (this may take a while)...");
    }
    let _ = std::io::stdout().flush();
    let start = Instant::now();
    let result = if force {
        client.sync_reset()?
    } else {
        client.history()?
    };
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
        "{green}✓{reset} Imported {bold}{}{reset} messages from {bold}{}{reset} files.",
        msgs, files
    );
    print_warnings(&result);
    Ok(())
}

fn print_warnings(result: &serde_json::Value) {
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
