use anyhow::Result;
use budi_core::analytics;

pub fn init_auto_sync() -> Result<(usize, usize)> {
    let db_path = analytics::db_path()?;
    let mut conn = analytics::open_db(&db_path)?;
    analytics::sync_all(&mut conn)
}

pub fn cmd_sync() -> Result<()> {
    let db_path = analytics::db_path()?;
    let mut conn = analytics::open_db(&db_path)?;

    println!("Syncing transcripts...");
    let (files_synced, messages_ingested) = analytics::sync_all(&mut conn)?;

    if files_synced == 0 && messages_ingested == 0 {
        println!("Already up to date.");
    } else {
        println!(
            "Synced \x1b[1m{}\x1b[0m new messages from \x1b[1m{}\x1b[0m files.",
            messages_ingested, files_synced
        );
    }
    println!("Database: {}", db_path.display());
    Ok(())
}
