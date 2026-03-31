//! Provider trait for agent-agnostic analytics.
//!
//! Every coding agent (Claude Code, Cursor, Copilot, etc.) implements this
//! trait. The sync engine, dashboard, and CLI use it to discover and process
//! data from any supported agent.

use std::path::{Path, PathBuf};

use anyhow::Result;
use rusqlite::Connection;

/// Per-million-token pricing for a model.
#[derive(Debug, Clone)]
pub struct ModelPricing {
    pub input: f64,
    pub output: f64,
    pub cache_write: f64,
    pub cache_read: f64,
}

impl ModelPricing {
    /// Calculate cost in cents from token counts and pricing.
    /// Single source of truth for cost calculation — used by CostEnricher, OTEL ingestion, etc.
    pub fn calculate_cost_cents(
        &self,
        input_tokens: u64,
        output_tokens: u64,
        cache_creation_tokens: u64,
        cache_read_tokens: u64,
        cache_creation_1h_tokens: u64,
        speed: Option<&str>,
        web_search_requests: u64,
    ) -> f64 {
        let cache_5m_tokens = cache_creation_tokens.saturating_sub(cache_creation_1h_tokens);
        let cache_1h_rate = self.input * 2.0;

        let mut cost = input_tokens as f64 * self.input / 1_000_000.0
            + output_tokens as f64 * self.output / 1_000_000.0
            + cache_5m_tokens as f64 * self.cache_write / 1_000_000.0
            + cache_creation_1h_tokens as f64 * cache_1h_rate / 1_000_000.0
            + cache_read_tokens as f64 * self.cache_read / 1_000_000.0;

        if speed == Some("fast") {
            cost *= 6.0;
        }

        if web_search_requests > 0 {
            cost += web_search_requests as f64 * 0.01;
        }

        cost * 100.0
    }
}

/// Look up pricing for a model using the correct provider's pricing table.
pub fn pricing_for_model(model: &str, provider: &str) -> ModelPricing {
    match provider {
        "cursor" => crate::providers::cursor::cursor_pricing_for_model(model),
        _ => crate::providers::claude_code::claude_pricing_for_model(model),
    }
}

/// A transcript file discovered by a provider.
#[derive(Debug, Clone)]
pub struct DiscoveredFile {
    pub path: PathBuf,
}

/// The core provider trait. Every coding agent implements this.
pub trait Provider: Send + Sync {
    // === Required ===
    fn name(&self) -> &'static str;
    fn display_name(&self) -> &'static str;
    fn is_available(&self) -> bool;
    fn discover_files(&self) -> Result<Vec<DiscoveredFile>>;
    fn parse_file(
        &self,
        path: &Path,
        content: &str,
        offset: usize,
    ) -> Result<(Vec<crate::jsonl::ParsedMessage>, usize)>;

    /// Direct sync from a structured data source (e.g. SQLite database).
    /// Returns Some((files_synced, messages_ingested, warnings)) if this provider uses
    /// direct sync instead of file-based discovery. Returns None to fall back
    /// to discover_files() + parse_file().
    ///
    /// `max_age_days`: Some(N) for quick sync (recent data only), None for full history.
    fn sync_direct(
        &self,
        _conn: &mut Connection,
        _pipeline: &mut crate::pipeline::Pipeline,
        _max_age_days: Option<u64>,
    ) -> Option<Result<(usize, usize, Vec<String>)>> {
        None
    }
}

/// Returns all registered providers (whether or not their data is present).
pub fn all_providers() -> Vec<Box<dyn Provider>> {
    vec![
        Box::new(crate::providers::claude_code::ClaudeCodeProvider),
        Box::new(crate::providers::cursor::CursorProvider),
    ]
}

/// Returns only providers that have data available on this machine.
pub fn available_providers() -> Vec<Box<dyn Provider>> {
    all_providers()
        .into_iter()
        .filter(|p| p.is_available())
        .collect()
}
