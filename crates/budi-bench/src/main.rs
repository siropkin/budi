use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use budi_core::config;
use budi_core::rpc::QueryRequest;
use clap::Parser;
use reqwest::blocking::Client;
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(name = "budi-bench")]
#[command(about = "Benchmark budi retrieval latency and context size")]
#[command(version)]
struct Cli {
    #[arg(long)]
    repo_root: Option<PathBuf>,
    #[arg(long)]
    prompt: String,
    #[arg(long, default_value_t = 20)]
    iterations: usize,
}

#[derive(Debug, Serialize)]
struct BenchReport {
    repo_root: String,
    iterations: usize,
    latency_ms_p50: f64,
    latency_ms_p95: f64,
    avg_context_chars: f64,
    avg_context_tokens_estimate: f64,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let repo_root = match cli.repo_root {
        Some(v) => v,
        None => config::find_repo_root(&std::env::current_dir()?)?,
    };
    let budi_config = config::load_or_default(&repo_root)?;
    let client = Client::builder().build()?;
    let query_url = format!("{}/query", budi_config.daemon_base_url());

    let mut latencies = Vec::new();
    let mut chars = Vec::new();
    for _ in 0..cli.iterations {
        let start = Instant::now();
        let resp = client
            .post(&query_url)
            .json(&QueryRequest {
                repo_root: repo_root.display().to_string(),
                prompt: cli.prompt.clone(),
                cwd: Some(repo_root.display().to_string()),
            })
            .send()
            .context("Failed to call daemon query endpoint")?
            .error_for_status()
            .context("Daemon query endpoint failed")?;
        let body: serde_json::Value = resp.json().context("Failed to parse query JSON response")?;
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        latencies.push(elapsed);
        let context_chars = body
            .get("context")
            .and_then(|v| v.as_str())
            .map(|s| s.len())
            .unwrap_or(0);
        chars.push(context_chars as f64);
    }

    latencies.sort_by(|a, b| a.total_cmp(b));
    let p50 = percentile(&latencies, 0.50);
    let p95 = percentile(&latencies, 0.95);
    let avg_chars = average(&chars);
    let avg_tokens = avg_chars / 4.0;

    let report = BenchReport {
        repo_root: repo_root.display().to_string(),
        iterations: cli.iterations,
        latency_ms_p50: p50,
        latency_ms_p95: p95,
        avg_context_chars: avg_chars,
        avg_context_tokens_estimate: avg_tokens,
    };

    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn percentile(values: &[f64], p: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let rank = ((values.len() as f64 - 1.0) * p).round() as usize;
    values[rank]
}

fn average(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.iter().sum::<f64>() / values.len() as f64
}
