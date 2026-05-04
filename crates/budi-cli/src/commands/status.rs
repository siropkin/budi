use anyhow::Result;
use serde::Serialize;

use crate::StatsPeriod;
use crate::client::DaemonClient;
use crate::commands::stats::{format_cost_cents, format_tokens, period_date_range};
use crate::{StatsFormat, commands};

use super::ansi;

/// Quick operational overview: daemon and today's cost.
pub fn cmd_status(format: StatsFormat) -> Result<()> {
    if matches!(format, StatsFormat::Json) {
        return cmd_status_json();
    }
    cmd_status_text()
}

fn cmd_status_text() -> Result<()> {
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

    // Today's cost summary — single snapshot call (#619) so cost,
    // messages, and providers come from one DB connection.
    let client = match DaemonClient::connect() {
        Ok(c) => c,
        Err(_) => {
            println!();
            return Ok(());
        }
    };

    let (since, _until) = period_date_range(StatsPeriod::Today);
    let snap = client
        .status_snapshot(since.as_deref(), None, None)
        .ok();

    if let Some(snap) = &snap {
        println!();
        println!(
            "  {bold}Today{reset}    {yellow}{}{reset}  ({} messages, {} in, {} out)",
            format_cost_cents(snap.cost.total_cost * 100.0),
            snap.summary.total_messages,
            format_tokens(snap.summary.total_input_tokens),
            format_tokens(snap.summary.total_output_tokens),
        );
        if !snap.providers.is_empty() {
            let names: Vec<&str> = snap
                .providers
                .iter()
                .map(|p| p.display_name.as_str())
                .collect();
            println!("  {bold}Agents{reset}   {}", names.join(", "));
        }
    }

    // First-run friendly hint when setup looks healthy but no activity recorded yet today.
    let no_activity_today = snap.as_ref().is_some_and(|s| s.summary.total_messages == 0);
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

#[derive(Debug, Serialize)]
struct StatusJson {
    daemon: DaemonJson,
    today: Option<TodayJson>,
}

#[derive(Debug, Serialize)]
struct DaemonJson {
    running: bool,
    version: String,
    port: u16,
}

#[derive(Debug, Serialize)]
struct TodayJson {
    cost_cents: f64,
    messages: u64,
    providers: Vec<String>,
}

fn cmd_status_json() -> Result<()> {
    let config = DaemonClient::load_config();
    let daemon_ok = crate::daemon::daemon_health(&config);
    let daemon = DaemonJson {
        running: daemon_ok,
        version: env!("CARGO_PKG_VERSION").to_string(),
        port: config.daemon_port,
    };

    let today = if daemon_ok {
        match DaemonClient::connect() {
            Ok(client) => {
                let (since, _until) = period_date_range(StatsPeriod::Today);
                client
                    .status_snapshot(since.as_deref(), None, None)
                    .ok()
                    .map(|snap| TodayJson {
                        cost_cents: snap.cost.total_cost * 100.0,
                        messages: snap.summary.total_messages,
                        providers: snap
                            .providers
                            .into_iter()
                            .map(|p| p.provider)
                            .collect(),
                    })
            }
            Err(_) => None,
        }
    } else {
        None
    };

    commands::print_json(&StatusJson { daemon, today })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lock the JSON shape: `{daemon: {running, version, port}, today:
    /// {cost_cents, messages, providers}}`. Scripted callers — CI gates,
    /// dashboards, the cloud surface — depend on these names.
    #[test]
    fn status_json_locks_schema_with_today_present() {
        let body = StatusJson {
            daemon: DaemonJson {
                running: true,
                version: "8.3.14".to_string(),
                port: 7878,
            },
            today: Some(TodayJson {
                cost_cents: 12345.6,
                messages: 42,
                providers: vec!["claude_code".to_string(), "cursor".to_string()],
            }),
        };
        let v = serde_json::to_value(&body).expect("serialise");
        let mut top_keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        top_keys.sort();
        assert_eq!(top_keys, vec!["daemon", "today"]);

        // Daemon block.
        let daemon = v["daemon"].as_object().expect("daemon is object");
        let mut keys: Vec<&str> = daemon.keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(keys, vec!["port", "running", "version"]);
        assert!(v["daemon"]["running"].is_boolean());
        assert!(v["daemon"]["version"].is_string());
        assert!(v["daemon"]["port"].is_number());

        // Today block + the cents-key normalisation contract from #445.
        let today = v["today"].as_object().expect("today is object");
        let mut keys: Vec<&str> = today.keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(keys, vec!["cost_cents", "messages", "providers"]);
        // After commands::print_json runs the cents normaliser the float
        // would round to an integer; here we serialise without the
        // helper so the raw float should still pass through. The
        // round_cents_to_integer test in mod.rs covers that conversion.
        assert!(v["today"]["cost_cents"].is_number());
        assert!(v["today"]["messages"].is_u64());
        assert!(v["today"]["providers"].is_array());
    }

    #[test]
    fn status_json_today_is_null_when_daemon_down() {
        // When the daemon is unreachable we still emit `today` as a
        // top-level key (so the shape stays stable for `jq .today`),
        // but as `null` rather than zero-valued so callers can detect
        // "no data" without false positives.
        let body = StatusJson {
            daemon: DaemonJson {
                running: false,
                version: "8.3.14".to_string(),
                port: 7878,
            },
            today: None,
        };
        let v = serde_json::to_value(&body).expect("serialise");
        assert!(v["today"].is_null());
        assert_eq!(v["daemon"]["running"], serde_json::json!(false));
    }
}
