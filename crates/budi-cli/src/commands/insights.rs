use anyhow::Result;
use budi_core::{analytics, insights};

use super::stats::{format_tokens, period_date_range, shorten_path};
use crate::StatsPeriod;

pub fn cmd_insights(period: StatsPeriod, json_output: bool) -> Result<()> {
    let db_path = analytics::db_path()?;
    if !db_path.exists() {
        println!("No analytics data yet. Run \x1b[1mbudi sync\x1b[0m to import transcripts.");
        return Ok(());
    }
    let conn = analytics::open_db(&db_path)?;
    let (since, until) = period_date_range(period);
    let ins = insights::generate_insights(&conn, since.as_deref(), until.as_deref(), 0)?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&ins)?);
        return Ok(());
    }

    let period_label = match period {
        StatsPeriod::Today => "Today",
        StatsPeriod::Week => "This week",
        StatsPeriod::Month => "This month",
        StatsPeriod::All => "All time",
    };

    println!("\x1b[1m  Insights — {}\x1b[0m", period_label);
    println!();

    // Search efficiency
    let se = &ins.search_efficiency;
    println!(
        "  \x1b[1mSearch Efficiency\x1b[0m  {} search / {} total tool calls ({:.0}%)",
        se.search_calls,
        se.total_calls,
        se.ratio * 100.0
    );
    if let Some(ref rec) = se.recommendation {
        let color = if se.ratio > 0.40 { "33" } else { "32" }; // yellow or green
        println!("    \x1b[{}m{}\x1b[0m", color, rec);
    }
    println!();

    // MCP tools
    if !ins.mcp_tools.is_empty() {
        println!("  \x1b[1mMCP Tool Usage\x1b[0m");
        for mcp in &ins.mcp_tools {
            println!("    \x1b[36m{}\x1b[0m  {} calls", mcp.tool, mcp.call_count);
        }
        println!();
    }

    // CLAUDE.md files
    if !ins.claude_md_files.is_empty() {
        println!("  \x1b[1mCLAUDE.md Files\x1b[0m");
        for f in &ins.claude_md_files {
            let size_label = if f.est_tokens >= 1000 {
                format!("~{}K tokens", f.est_tokens / 1000)
            } else {
                format!("~{} tokens", f.est_tokens)
            };
            println!(
                "    \x1b[36m{}\x1b[0m  {}",
                shorten_path(&f.path),
                size_label
            );
            if let Some(ref rec) = f.recommendation {
                println!("    \x1b[33m{}\x1b[0m", rec);
            }
        }
        println!();
    }

    // Cache efficiency
    let ce = &ins.cache_efficiency;
    if ce.total_input_tokens > 0 {
        println!(
            "  \x1b[1mCache Efficiency\x1b[0m  {:.0}% hit rate ({} cache reads / {} total input)",
            ce.hit_rate * 100.0,
            format_tokens(ce.total_cache_read_tokens),
            format_tokens(ce.total_input_tokens)
        );
        if let Some(ref rec) = ce.recommendation {
            let color = if ce.hit_rate < 0.30 { "33" } else { "32" };
            println!("    \x1b[{}m{}\x1b[0m", color, rec);
        }
        println!();
    }

    // Token-heavy sessions
    if !ins.token_heavy_sessions.is_empty() {
        println!("  \x1b[1mToken-Heavy Sessions\x1b[0m  (input/output ratio > 5x)");
        for s in ins.token_heavy_sessions.iter().take(5) {
            let project = s
                .repo_id
                .as_deref()
                .unwrap_or_else(|| s.project_dir.as_deref().unwrap_or(""));
            println!(
                "    \x1b[36m{}…\x1b[0m  {} in / {} out ({:.0}x)  {}",
                &s.session_id[..s.session_id.len().min(8)],
                format_tokens(s.input_tokens),
                format_tokens(s.output_tokens),
                s.ratio,
                project
            );
        }
        if !ins.token_heavy_sessions.is_empty() {
            println!(
                "    \x1b[33mHigh input/output ratio suggests large context. \
                 Try splitting tasks into smaller sessions.\x1b[0m"
            );
        }
        println!();
    }

    Ok(())
}
