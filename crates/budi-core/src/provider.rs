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
    fn sync_direct(
        &self,
        _conn: &mut Connection,
        _pipeline: &mut crate::pipeline::Pipeline,
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
