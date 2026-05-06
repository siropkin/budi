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
    #[allow(clippy::too_many_arguments)]
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

    /// Directories the daemon's filesystem tailer should watch for new and
    /// grown transcript files (see [ADR-0089] §1 and #318).
    ///
    /// The tailer registers a recursive `notify` watcher on each returned
    /// path; on each event it calls [`Provider::parse_file`] with the stored
    /// per-file offset and feeds the resulting messages through
    /// `Pipeline::default_pipeline()`. This is the single live ingestion
    /// path in 8.2+ — there is no proxy fallback.
    ///
    /// Returned paths must:
    /// - be absolute,
    /// - point at directories that currently exist on disk (the tailer skips
    ///   non-existent roots rather than blocking startup), and
    /// - cover the parent of every file [`Provider::discover_files`] would
    ///   return on the same machine.
    ///
    /// The default implementation derives roots by deduplicating the parent
    /// directories of [`Provider::discover_files`]. This keeps existing
    /// providers compiling, but every shipped provider should override with
    /// its well-known root(s) so the watcher can attach even before any
    /// transcripts have been written (e.g. on a freshly installed agent).
    ///
    /// Cursor's Usage API is **not** a watch root. It is a pull-mode
    /// reconciliation handled by [`Provider::sync_direct`] and scheduled
    /// independently of the tailer (see #321).
    ///
    /// [ADR-0089]: https://github.com/siropkin/budi/blob/main/docs/adr/0089-reverse-proxy-first-jsonl-tailing-as-sole-live-path.md
    fn watch_roots(&self) -> Vec<PathBuf> {
        let Ok(files) = self.discover_files() else {
            return Vec::new();
        };
        let mut roots: Vec<PathBuf> = files
            .into_iter()
            .filter_map(|f| f.path.parent().map(Path::to_path_buf))
            .collect();
        roots.sort();
        roots.dedup();
        roots
    }
}

/// Returns all registered providers (whether or not their data is present).
pub fn all_providers() -> Vec<Box<dyn Provider>> {
    vec![
        Box::new(crate::providers::claude_code::ClaudeCodeProvider),
        Box::new(crate::providers::codex::CodexProvider),
        Box::new(crate::providers::copilot::CopilotProvider),
        Box::new(crate::providers::copilot_chat::CopilotChatProvider),
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

/// Returns only providers that the user has explicitly enabled.
///
/// If no `agents.toml` exists (legacy install), falls back to
/// `available_providers()` for backward compatibility.
pub fn enabled_providers() -> Vec<Box<dyn Provider>> {
    let agents_config = crate::config::load_agents_config();
    match agents_config {
        Some(config) => all_providers()
            .into_iter()
            .filter(|p| p.is_available() && config.is_agent_enabled(p.name()))
            .collect(),
        None => available_providers(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal Provider that only implements the required surface so we can
    /// exercise the default `watch_roots()` implementation.
    struct StubProvider {
        files: Vec<PathBuf>,
    }

    impl Provider for StubProvider {
        fn name(&self) -> &'static str {
            "stub"
        }
        fn display_name(&self) -> &'static str {
            "Stub"
        }
        fn is_available(&self) -> bool {
            true
        }
        fn discover_files(&self) -> Result<Vec<DiscoveredFile>> {
            Ok(self
                .files
                .iter()
                .cloned()
                .map(|path| DiscoveredFile { path })
                .collect())
        }
        fn parse_file(
            &self,
            _path: &Path,
            _content: &str,
            _offset: usize,
        ) -> Result<(Vec<crate::jsonl::ParsedMessage>, usize)> {
            Ok((Vec::new(), 0))
        }
    }

    #[test]
    fn default_watch_roots_dedups_parent_dirs() {
        let provider = StubProvider {
            files: vec![
                PathBuf::from("/tmp/budi-stub/a/one.jsonl"),
                PathBuf::from("/tmp/budi-stub/a/two.jsonl"),
                PathBuf::from("/tmp/budi-stub/b/three.jsonl"),
            ],
        };
        let roots = provider.watch_roots();
        assert_eq!(
            roots,
            vec![
                PathBuf::from("/tmp/budi-stub/a"),
                PathBuf::from("/tmp/budi-stub/b"),
            ]
        );
    }

    #[test]
    fn default_watch_roots_empty_when_no_files() {
        let provider = StubProvider { files: Vec::new() };
        assert!(provider.watch_roots().is_empty());
    }
}
