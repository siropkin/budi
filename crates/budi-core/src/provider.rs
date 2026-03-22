//! Provider trait for agent-agnostic analytics.
//!
//! Every coding agent (Claude Code, Cursor, Copilot, etc.) implements this
//! trait. The sync engine, dashboard, and CLI use it to discover and process
//! data from any supported agent.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::claude_data::{PlanFile, PromptEntry};
use crate::hooks;

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

/// Setup data a provider can expose for the dashboard Setup page.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProviderSetupData {
    pub activity: Option<crate::claude_data::ActivityTimeline>,
    pub plugins: Vec<crate::claude_data::PluginInfo>,
    pub active_sessions: Vec<crate::claude_data::ActiveSession>,
    pub memory_files: Vec<crate::claude_data::MemoryFile>,
    pub permissions: Option<crate::claude_data::PermissionsSummary>,
}

/// Hook handler trait for providers that support hooks.
pub trait HookHandler: Send + Sync {
    fn handle_prompt_submit(
        &self,
        input: &hooks::UserPromptSubmitInput,
    ) -> hooks::UserPromptSubmitOutput;
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
    fn pricing_for_model(&self, model: &str) -> ModelPricing;

    // === Optional capabilities ===

    /// Config files, plugins, permissions, memory — for Setup page.
    fn setup_data(&self) -> Option<ProviderSetupData> {
        None
    }

    /// Plans/tasks — for Plans page.
    fn discover_plans(&self) -> Result<Vec<PlanFile>> {
        Ok(vec![])
    }

    /// Prompt history — for Prompts page.
    fn prompt_history(&self, _limit: usize) -> Result<Vec<PromptEntry>> {
        Ok(vec![])
    }

    /// Hook integration — for statusline and pre-filtering.
    fn hook_support(&self) -> Option<Box<dyn HookHandler>> {
        None
    }

    /// Pre-filter logic for this provider's system messages.
    fn system_message_patterns(&self) -> Vec<&str> {
        vec![]
    }
}

/// Returns all registered providers (whether or not their data is present).
pub fn all_providers() -> Vec<Box<dyn Provider>> {
    vec![
        Box::new(crate::providers::claude_code::ClaudeCodeProvider),
        // Cursor provider is implemented but not yet registered — Cursor's
        // JSONL transcripts lack token counts and model names, so the data
        // isn't useful enough to show alongside Claude Code's detailed stats.
        // See providers/cursor.rs for the implementation.
    ]
}

/// Returns only providers that have data available on this machine.
pub fn available_providers() -> Vec<Box<dyn Provider>> {
    all_providers()
        .into_iter()
        .filter(|p| p.is_available())
        .collect()
}
