use anyhow::Result;
use budi_core::analytics::SessionHealth;

use crate::client::DaemonClient;

pub fn cmd_health(session_id: Option<String>) -> Result<()> {
    let client = DaemonClient::connect()?;
    let health = client.session_health(session_id.as_deref())?;
    render_health(&health);
    Ok(())
}

fn render_health(h: &SessionHealth) {
    let detail_name = |name: &str| -> String {
        match name {
            "context_drag" => "Prompt Growth".to_string(),
            "cache_efficiency" => "Cache Reuse".to_string(),
            "thrashing" => "Retry Loops".to_string(),
            "cost_acceleration" => "Cost Acceleration".to_string(),
            _ => name.to_string(),
        }
    };

    let bold = super::ansi("\x1b[1m");
    let dim = super::ansi("\x1b[90m");
    let reset = super::ansi("\x1b[0m");
    let green = super::ansi("\x1b[32m");
    let yellow = super::ansi("\x1b[33m");
    let red = super::ansi("\x1b[31m");

    let state_icon = |s: &str| -> &str {
        match s {
            "red" => "🔴",
            "yellow" => "🟡",
            "gray" => "⚪",
            _ => "🟢",
        }
    };
    let state_color = |s: &str| -> &str {
        match s {
            "red" => red,
            "yellow" => yellow,
            "gray" => dim,
            _ => green,
        }
    };

    let icon = state_icon(&h.state);
    let color = state_color(&h.state);
    println!(
        "{icon} {bold}Session Health: {color}{}{reset}",
        h.state.to_uppercase()
    );
    let cost_dollars = h.total_cost_cents / 100.0;
    let cost_display = if cost_dollars == 0.0 {
        0.0
    } else {
        cost_dollars
    };
    println!(
        "  {dim}{} messages · ${:.2} total{reset}",
        h.message_count, cost_display
    );
    println!();

    let vitals: Vec<(&str, &Option<budi_core::analytics::VitalScore>)> = vec![
        ("Prompt Growth", &h.vitals.context_drag),
        ("Cache Reuse", &h.vitals.cache_efficiency),
        ("Retry Loops", &h.vitals.thrashing),
        ("Cost Acceleration", &h.vitals.cost_acceleration),
    ];

    for (name, vital) in &vitals {
        match vital {
            Some(v) => {
                let vi = state_icon(&v.state);
                let vc = state_color(&v.state);
                println!("  {vi} {bold}{name}{reset}: {vc}{}{reset}", v.label);
            }
            None => {
                println!("  {dim}⚪ {name}: N/A{reset}");
            }
        }
    }

    if !h.details.is_empty() {
        println!();
        for d in &h.details {
            let di = state_icon(&d.state);
            let dc = state_color(&d.state);
            println!(
                "{di} {dc}{bold}{} ({}):{reset}",
                detail_name(&d.vital),
                d.label
            );
            println!("  {}", d.tip);
            for action in &d.actions {
                println!("  - {action}");
            }
            println!();
        }
    }

    if h.state == "green" {
        println!();
        println!("  {green}{}{reset}", h.tip);
    }
}
