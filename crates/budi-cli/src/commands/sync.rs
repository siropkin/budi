use anyhow::Result;
use budi_core::analytics;

pub fn init_auto_sync() -> Result<(usize, usize)> {
    let db_path = analytics::db_path()?;
    let mut conn = analytics::open_db_with_migration(&db_path)?;
    analytics::sync_all(&mut conn)
}

pub fn cmd_sync_with_options(backfill_tags: bool) -> Result<()> {
    let db_path = analytics::db_path()?;
    let mut conn = analytics::open_db_with_migration(&db_path)?;

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

    if backfill_tags {
        println!("Regenerating tags...");
        let tag_count = analytics::backfill_tags(&mut conn)?;
        println!("Generated \x1b[1m{}\x1b[0m tags.", tag_count);
    }

    // Git enrichment: process all sessions (no batch limit for manual sync)
    println!("Enriching git data...");
    let mut total_git_commits = 0;
    let mut total_git_sessions = 0;
    loop {
        match budi_core::git::enrich_git_batch(&mut conn, 100) {
            Ok(r) => {
                total_git_commits += r.commits_found;
                total_git_sessions += r.sessions_processed;
                if r.sessions_remaining == 0 {
                    break;
                }
            }
            Err(e) => {
                eprintln!("Git enrichment error: {e}");
                break;
            }
        }
    }
    if total_git_sessions > 0 {
        println!(
            "Git: \x1b[1m{}\x1b[0m commits from \x1b[1m{}\x1b[0m sessions.",
            total_git_commits, total_git_sessions
        );
    }

    println!("Database: {}", db_path.display());
    Ok(())
}
