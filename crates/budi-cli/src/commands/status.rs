use anyhow::Result;

use crate::StatsPeriod;
use crate::client::DaemonClient;
use crate::commands::stats::{format_cost_cents, format_tokens, period_date_range};

use super::ansi;

/// Quick operational overview: daemon, proxy, today's cost.
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

    // Proxy health: try connecting to the proxy port
    let proxy_port = config.proxy.effective_port();
    let proxy_ok = check_proxy_port(proxy_port);
    if proxy_ok {
        println!("  {green}✓{reset} {bold}Proxy{reset}    running (port {proxy_port})");
    } else if config.proxy.effective_enabled() {
        println!(
            "  {red}✗{reset} {bold}Proxy{reset}    not running (expected on port {proxy_port})"
        );
    } else {
        println!("  {dim}-{reset} {bold}Proxy{reset}    disabled");
    }

    // Today's cost summary
    let client = match DaemonClient::connect() {
        Ok(c) => c,
        Err(_) => {
            println!();
            return Ok(());
        }
    };

    let (since, _until) = period_date_range(StatsPeriod::Today);
    if let Ok(summary) = client.summary(since.as_deref(), None, None)
        && let Ok(cost) = client.cost(since.as_deref(), None, None)
    {
        println!();
        println!(
            "  {bold}Today{reset}    {yellow}{}{reset}  ({} messages, {} in, {} out)",
            format_cost_cents(cost.total_cost * 100.0),
            summary.total_messages,
            format_tokens(summary.total_input_tokens),
            format_tokens(summary.total_output_tokens),
        );
    }

    // Active agents
    if let Ok(providers) = client.providers(since.as_deref(), None)
        && !providers.is_empty()
    {
        let names: Vec<&str> = providers.iter().map(|p| p.display_name.as_str()).collect();
        println!("  {bold}Agents{reset}   {}", names.join(", "));
    }

    println!();
    Ok(())
}

/// TCP probe to check if the proxy is listening on the given port.
fn check_proxy_port(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        std::time::Duration::from_millis(500),
    )
    .is_ok()
}
