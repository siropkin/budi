use anyhow::Result;

use crate::StatsPeriod;
use crate::client::DaemonClient;
use crate::commands::stats::{format_cost_cents, format_tokens, period_date_range};

use super::ansi;

/// Quick operational overview: daemon and today's cost.
pub fn cmd_status() -> Result<()> {
    let bold_cyan = ansi("\x1b[1;36m");
    let bold = ansi("\x1b[1m");
    let dim = ansi("\x1b[90m");
    let green = ansi("\x1b[32m");
    let red = ansi("\x1b[31m");
    let yellow = ansi("\x1b[33m");
    let reset = ansi("\x1b[0m");

    println!();
    println!("  {bold_cyan} budi status{reset}");
    println!("  {dim}{}{reset}", "─".repeat(40));

    // Daemon health
    let config = DaemonClient::load_config();
    let daemon_ok = crate::daemon::daemon_health(&config);
    if daemon_ok {
        let version = env!("CARGO_PKG_VERSION");
        println!(
            "  {green}✓{reset} {bold}Daemon{reset}   running (v{version}, port {})",
            config.daemon_port
        );
    } else {
        println!(
            "  {red}✗{reset} {bold}Daemon{reset}   not running (port {})",
            config.daemon_port
        );
        println!("    Run `budi init` to start the daemon.");
        println!();
        return Ok(());
    }

    println!("  {dim}-{reset} {bold}Proxy{reset}    removed in 8.2 (tailer is live)");

    // Today's cost summary
    let client = match DaemonClient::connect() {
        Ok(c) => c,
        Err(_) => {
            println!();
            return Ok(());
        }
    };

    let (since, _until) = period_date_range(StatsPeriod::Today);
    let summary = client.summary(since.as_deref(), None, None).ok();
    let cost = client.cost(since.as_deref(), None, None).ok();
    let providers = client.providers(since.as_deref(), None).ok();

    if let (Some(summary), Some(cost)) = (&summary, &cost) {
        println!();
        println!(
            "  {bold}Today{reset}    {yellow}{}{reset}  ({} messages, {} in, {} out)",
            format_cost_cents(cost.total_cost * 100.0),
            summary.total_messages,
            format_tokens(summary.total_input_tokens),
            format_tokens(summary.total_output_tokens),
        );
        if let Some(providers) = &providers
            && !providers.is_empty()
        {
            let names: Vec<&str> = providers.iter().map(|p| p.display_name.as_str()).collect();
            println!("  {bold}Agents{reset}   {}", names.join(", "));
        }
    }

    // First-run friendly hint when setup looks healthy but no activity recorded yet today.
    let no_activity_today = summary.as_ref().is_some_and(|s| s.total_messages == 0);
    if no_activity_today {
        println!();
        println!(
            "  {dim}No activity recorded today yet. Open your agent (`claude`, `codex`, `cursor`, `gh copilot`) and send a prompt — it will show up here.{reset}"
        );
        println!("  {dim}Run `budi doctor` to verify your setup end-to-end.{reset}");
    }

    println!();
    Ok(())
}
