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
            _ => green,
        }
    };

    let icon = state_icon(&h.state);
    let color = state_color(&h.state);
    println!(
        "{icon} {bold}Session Health: {color}{}{reset}",
        h.state.to_uppercase()
    );
    println!(
        "  {dim}{} messages · ${:.2} total{reset}",
        h.message_count,
        h.total_cost_cents / 100.0
    );
    println!();

    // Vitals table
    let vitals: Vec<(&str, &Option<budi_core::analytics::VitalScore>)> = vec![
        ("Context Drag", &h.vitals.context_drag),
        ("Cache Efficiency", &h.vitals.cache_efficiency),
        ("Agent Thrashing", &h.vitals.thrashing),
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
            println!("{di} {dc}{bold}{} ({}):{reset}", d.vital.replace('_', " "), d.label);
            for line in d.tip.lines() {
                println!("  {line}");
            }
            println!();
        }
    }

    if h.state == "green" {
        println!();
        println!("  {green}{}{reset}", h.tip);
    }
}
