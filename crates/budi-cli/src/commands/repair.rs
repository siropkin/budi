use anyhow::Result;

use crate::client::DaemonClient;

pub fn cmd_repair() -> Result<()> {
    let client = DaemonClient::connect()?;
    let result = client.repair()?;

    let from = result
        .get("from_version")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let to = result
        .get("to_version")
        .and_then(|v| v.as_u64())
        .unwrap_or(from);
    let migrated = result
        .get("migrated")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let repaired = result
        .get("repaired")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let added_columns: Vec<String> = result
        .get("added_columns")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(ToOwned::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let added_indexes: Vec<String> = result
        .get("added_indexes")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(ToOwned::to_owned))
                .collect()
        })
        .unwrap_or_default();

    if migrated {
        println!("Migrated database v{from} -> v{to}.");
    } else {
        println!("Database schema version is v{to}.");
    }

    if repaired {
        println!("Repaired schema drift:");
        if !added_columns.is_empty() {
            println!("Added missing columns:");
            for col in &added_columns {
                println!("  - {col}");
            }
        }
        if !added_indexes.is_empty() {
            println!("Recreated missing indexes:");
            for idx in &added_indexes {
                println!("  - {idx}");
            }
        }
    } else {
        println!("No schema drift detected.");
    }

    let green = crate::commands::ansi("\x1b[32m");
    let reset = crate::commands::ansi("\x1b[0m");
    println!("{green}✓{reset} Repair complete.");
    Ok(())
}
