//! Claude Code provider — implements the Provider trait by delegating to
//! existing modules (jsonl, cost, claude_data, pre_filter, hooks).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::claude_data::{self, PlanFile, PromptEntry};
use crate::hooks;
use crate::jsonl::{self, ParsedMessage};
use crate::pre_filter;
use crate::provider::{DiscoveredFile, HookHandler, ModelPricing, Provider, ProviderSetupData};

/// The Claude Code provider.
pub struct ClaudeCodeProvider;

impl Provider for ClaudeCodeProvider {
    fn name(&self) -> &'static str {
        "claude_code"
    }

    fn display_name(&self) -> &'static str {
        "Claude Code"
    }

    fn is_available(&self) -> bool {
        claude_home().map(|p| p.exists()).unwrap_or(false)
    }

    fn discover_files(&self) -> Result<Vec<DiscoveredFile>> {
        let files = discover_jsonl_files()?;
        Ok(files
            .into_iter()
            .map(|path| DiscoveredFile { path })
            .collect())
    }

    fn parse_file(
        &self,
        _path: &Path,
        content: &str,
        offset: usize,
    ) -> Result<(Vec<ParsedMessage>, usize)> {
        Ok(jsonl::parse_transcript(content, offset))
    }

    fn pricing_for_model(&self, model: &str) -> ModelPricing {
        claude_pricing_for_model(model)
    }

    fn setup_data(&self) -> Option<ProviderSetupData> {
        Some(ProviderSetupData {
            activity: claude_data::read_activity_timeline().ok(),
            plugins: claude_data::read_installed_plugins().unwrap_or_default(),
            active_sessions: claude_data::read_active_sessions().unwrap_or_default(),
            memory_files: claude_data::read_memory_files().unwrap_or_default(),
            permissions: claude_data::read_permissions().ok(),
        })
    }

    fn discover_plans(&self) -> Result<Vec<PlanFile>> {
        claude_data::read_plans()
    }

    fn prompt_history(&self, limit: usize) -> Result<Vec<PromptEntry>> {
        let history = claude_data::read_prompt_history(limit)?;
        Ok(history.entries)
    }

    fn hook_support(&self) -> Option<Box<dyn HookHandler>> {
        Some(Box::new(ClaudeHookHandler))
    }

    fn system_message_patterns(&self) -> Vec<&str> {
        vec![
            "<task-notification>",
            "<system-reminder>",
            "<function_calls>",
            "<function_results>",
        ]
    }
}

/// Claude Code hook handler.
struct ClaudeHookHandler;

impl HookHandler for ClaudeHookHandler {
    fn handle_prompt_submit(
        &self,
        _input: &hooks::UserPromptSubmitInput,
    ) -> hooks::UserPromptSubmitOutput {
        hooks::UserPromptSubmitOutput::allow_with_context(String::new())
    }
}

// ---------------------------------------------------------------------------
// Extracted helpers (previously in analytics.rs and cost.rs)
// ---------------------------------------------------------------------------

fn claude_home() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".claude"))
}

/// Discover all Claude Code JSONL transcript files under `~/.claude/projects/`.
pub fn discover_jsonl_files() -> Result<Vec<PathBuf>> {
    let claude_dir = claude_home()?.join("projects");
    let mut files = Vec::new();
    collect_jsonl_recursive(&claude_dir, &mut files, 0);
    files.sort();
    Ok(files)
}

fn collect_jsonl_recursive(dir: &Path, files: &mut Vec<PathBuf>, depth: u32) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().map(|n| n == "subagents").unwrap_or(false) {
                continue;
            }
            collect_jsonl_recursive(&path, files, depth + 1);
        } else if path.extension().is_some_and(|e| e == "jsonl") {
            files.push(path);
        }
    }
}

/// Claude model pricing lookup.
pub fn claude_pricing_for_model(model: &str) -> ModelPricing {
    let m = model.to_lowercase();
    if m.contains("opus-4-6") || m.contains("opus-4-5") {
        ModelPricing {
            input: 5.0,
            output: 25.0,
            cache_write: 6.25,
            cache_read: 0.50,
        }
    } else if m.contains("opus") {
        ModelPricing {
            input: 15.0,
            output: 75.0,
            cache_write: 18.75,
            cache_read: 1.50,
        }
    } else if m.contains("sonnet") {
        ModelPricing {
            input: 3.0,
            output: 15.0,
            cache_write: 3.75,
            cache_read: 0.30,
        }
    } else if m.contains("haiku") {
        ModelPricing {
            input: 1.0,
            output: 5.0,
            cache_write: 1.25,
            cache_read: 0.10,
        }
    } else {
        // Unknown model — use sonnet pricing as a reasonable default
        ModelPricing {
            input: 3.0,
            output: 15.0,
            cache_write: 3.75,
            cache_read: 0.30,
        }
    }
}

/// Check if a prompt should be skipped by Claude Code pre-filter logic.
pub fn should_skip_prompt(prompt: &str) -> bool {
    pre_filter::is_obviously_non_code(prompt) || pre_filter::is_conversational_followup(prompt)
}
