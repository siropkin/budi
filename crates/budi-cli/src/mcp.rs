//! MCP (Model Context Protocol) server for budi.
//!
//! Exposes budi analytics and configuration as MCP tools so AI agents
//! can query cost data and manage budi directly from conversation.
//! The server is a thin HTTP client — all data comes from budi-daemon.

use std::time::Duration;

use chrono::{Datelike, Local, NaiveDate, TimeZone};
use reqwest::blocking::Client;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::{ErrorData as McpError, tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::client::DaemonClient;

// ─── Request types ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PeriodRequest {
    /// Time period: "today", "week", "month", "all". Default: "month"
    #[serde(default = "default_period")]
    pub period: String,
}

fn default_period() -> String {
    "month".into()
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BranchRequest {
    /// Git branch name to query
    pub branch: String,
    /// Time period: "today", "week", "month", "all". Default: "month"
    #[serde(default = "default_period")]
    pub period: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TagRequest {
    /// Tag key to break down by (e.g. "ticket_id", "activity", "user", "composer_mode", "permission_mode", "duration", "dominant_tool", "cost_confidence")
    pub key: String,
    /// Time period: "today", "week", "month", "all". Default: "month"
    #[serde(default = "default_period")]
    pub period: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TagRulesRequest {
    /// TOML content for tag rules. Each rule has key, value, and optional match_repo.
    /// Example: [[rules]]\nkey = "team"\nvalue = "backend"\nmatch_repo = "*api*"
    pub toml_content: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StatuslineConfigRequest {
    /// TOML content for statusline config.
    /// Example: slots = ["today", "week", "month"]\nformat = "{today} | {week}"
    pub toml_content: String,
}

// ─── MCP Server ─────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct BudiMcpServer {
    tool_router: ToolRouter<Self>,
    base_url: String,
    client: Client,
}

#[tool_router]
impl BudiMcpServer {
    pub fn new() -> Self {
        let config = DaemonClient::load_config();
        let base_url = config.daemon_base_url();
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("failed to build HTTP client");
        Self {
            tool_router: Self::tool_router(),
            base_url,
            client,
        }
    }

    // ─── Analytics tools ────────────────────────────────────────────

    #[tool(
        description = "Get AI coding cost summary: total cost, tokens, and messages for a time period. Returns estimated cost with breakdown by input/output/cache tokens."
    )]
    async fn get_cost_summary(
        &self,
        params: Parameters<PeriodRequest>,
    ) -> Result<CallToolResult, McpError> {
        let (since, until) = period_to_dates(&params.0.period);
        let period_label = period_label(&params.0.period);

        let summary: Value = self.daemon_get(
            "/analytics/summary",
            &build_params(since.as_deref(), until.as_deref()),
        )?;
        let cost: Value = self.daemon_get(
            "/analytics/cost",
            &build_params(since.as_deref(), until.as_deref()),
        )?;

        let total_cost = cost
            .get("total_cost")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let input_cost = cost
            .get("input_cost")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let output_cost = cost
            .get("output_cost")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let cache_write_cost = cost
            .get("cache_write_cost")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let cache_read_cost = cost
            .get("cache_read_cost")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let cache_savings = cost
            .get("cache_savings")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);

        let msgs = summary
            .get("total_messages")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let user_msgs = summary
            .get("total_user_messages")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let asst_msgs = summary
            .get("total_assistant_messages")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let input_tok = summary
            .get("total_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let output_tok = summary
            .get("total_output_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let cache_create = summary
            .get("total_cache_creation_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let cache_read = summary
            .get("total_cache_read_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        let mut text = format!("Cost Summary ({period_label})\n");
        text.push_str(&format!("Total cost:     {}\n", format_dollars(total_cost)));
        text.push_str(&format!(
            "Total messages: {msgs} ({user_msgs} user, {asst_msgs} assistant)\n"
        ));
        text.push_str(&format!(
            "Total tokens:   {}\n",
            format_tokens(input_tok + output_tok + cache_create + cache_read)
        ));
        text.push('\n');
        text.push_str("Cost breakdown:\n");
        text.push_str(&format!("  Input:       {}\n", format_dollars(input_cost)));
        text.push_str(&format!("  Output:      {}\n", format_dollars(output_cost)));
        text.push_str(&format!(
            "  Cache write: {}\n",
            format_dollars(cache_write_cost)
        ));
        text.push_str(&format!(
            "  Cache read:  {}\n",
            format_dollars(cache_read_cost)
        ));
        if cache_savings > 0.0 {
            text.push_str(&format!(
                "  Cache savings: {}\n",
                format_dollars(cache_savings)
            ));
        }
        text.push('\n');
        text.push_str("Token breakdown:\n");
        text.push_str(&format!("  Input:       {}\n", format_tokens(input_tok)));
        text.push_str(&format!("  Output:      {}\n", format_tokens(output_tok)));
        text.push_str(&format!("  Cache write: {}\n", format_tokens(cache_create)));
        text.push_str(&format!("  Cache read:  {}\n", format_tokens(cache_read)));

        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[tool(
        description = "Get cost breakdown by AI model (e.g. Claude Opus, Sonnet, Haiku, GPT-4, etc). Shows message count, tokens, and cost per model."
    )]
    async fn get_model_breakdown(
        &self,
        params: Parameters<PeriodRequest>,
    ) -> Result<CallToolResult, McpError> {
        let (since, until) = period_to_dates(&params.0.period);
        let period_label = period_label(&params.0.period);

        let models: Vec<Value> = self.daemon_get(
            "/analytics/models",
            &build_params(since.as_deref(), until.as_deref()),
        )?;

        let mut text = format!("Model Breakdown ({period_label})\n");
        if models.is_empty() {
            text.push_str("No data for this period.");
        } else {
            text.push_str(&format!(
                "{:<30} {:>8} {:>10} {:>10}\n",
                "MODEL", "MSGS", "TOKENS", "COST"
            ));
            for m in &models {
                let model = m.get("model").and_then(|v| v.as_str()).unwrap_or("?");
                let msgs = m.get("message_count").and_then(|v| v.as_u64()).unwrap_or(0);
                let tok = sum_tokens(m);
                let cost = m.get("cost_cents").and_then(|v| v.as_f64()).unwrap_or(0.0);
                text.push_str(&format!(
                    "{:<30} {:>8} {:>10} {:>10}\n",
                    model,
                    msgs,
                    format_tokens(tok),
                    format_dollars(cost / 100.0)
                ));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[tool(
        description = "Get cost breakdown by project/repository. Shows which repos are consuming the most AI tokens and money."
    )]
    async fn get_project_costs(
        &self,
        params: Parameters<PeriodRequest>,
    ) -> Result<CallToolResult, McpError> {
        let (since, until) = period_to_dates(&params.0.period);
        let period_label = period_label(&params.0.period);

        let mut query = build_params(since.as_deref(), until.as_deref());
        query.push(("limit".to_string(), "30".to_string()));
        let repos: Vec<Value> = self.daemon_get("/analytics/projects", &query)?;

        let mut text = format!("Project Costs ({period_label})\n");
        if repos.is_empty() {
            text.push_str("No data for this period.");
        } else {
            text.push_str(&format!("{:<30} {:>8} {:>10}\n", "PROJECT", "MSGS", "COST"));
            for r in &repos {
                let repo = r.get("repo_id").and_then(|v| v.as_str()).unwrap_or("?");
                let msgs = r.get("message_count").and_then(|v| v.as_u64()).unwrap_or(0);
                let cost = r.get("cost_cents").and_then(|v| v.as_f64()).unwrap_or(0.0);
                text.push_str(&format!(
                    "{:<30} {:>8} {:>10}\n",
                    repo,
                    msgs,
                    format_dollars(cost / 100.0)
                ));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[tool(
        description = "Get cost breakdown by git branch. Shows which branches are consuming the most AI tokens and money. Useful for understanding cost per feature."
    )]
    async fn get_branch_costs(
        &self,
        params: Parameters<PeriodRequest>,
    ) -> Result<CallToolResult, McpError> {
        let (since, until) = period_to_dates(&params.0.period);
        let period_label = period_label(&params.0.period);

        let branches: Vec<Value> = self.daemon_get(
            "/analytics/branches",
            &build_params(since.as_deref(), until.as_deref()),
        )?;

        let mut text = format!("Branch Costs ({period_label})\n");
        if branches.is_empty() {
            text.push_str("No data for this period.");
        } else {
            text.push_str(&format!(
                "{:<30} {:>8} {:>8} {:>10}\n",
                "BRANCH", "SESSIONS", "MSGS", "COST"
            ));
            for b in &branches {
                let branch = b.get("git_branch").and_then(|v| v.as_str()).unwrap_or("?");
                let branch = branch.strip_prefix("refs/heads/").unwrap_or(branch);
                let sessions = b.get("session_count").and_then(|v| v.as_u64()).unwrap_or(0);
                let msgs = b.get("message_count").and_then(|v| v.as_u64()).unwrap_or(0);
                let cost = b.get("cost_cents").and_then(|v| v.as_f64()).unwrap_or(0.0);
                text.push_str(&format!(
                    "{:<30} {:>8} {:>8} {:>10}\n",
                    branch,
                    sessions,
                    msgs,
                    format_dollars(cost / 100.0)
                ));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[tool(
        description = "Get detailed cost info for a specific git branch, including sessions, messages, tokens, and cost."
    )]
    async fn get_branch_detail(
        &self,
        params: Parameters<BranchRequest>,
    ) -> Result<CallToolResult, McpError> {
        let (since, until) = period_to_dates(&params.0.period);
        let period_label = period_label(&params.0.period);
        let branch = &params.0.branch;

        let query = build_params(since.as_deref(), until.as_deref());
        let url = format!("/analytics/branches/{}", urlencoding_simple(branch));
        let result: Value = match self.daemon_get_raw(&url, &query) {
            Ok(resp) => {
                if resp.status() == reqwest::StatusCode::NOT_FOUND {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "No data found for branch '{branch}' ({period_label}). Run `budi sync` if you haven't synced recently."
                    ))]));
                }
                resp.json()
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?
            }
            Err(e) => return Err(e),
        };

        if result.is_null() {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "No data found for branch '{branch}' ({period_label})."
            ))]));
        }

        let mut text = format!("Branch: {branch} ({period_label})\n");
        let repo = result.get("repo_id").and_then(|v| v.as_str()).unwrap_or("");
        if !repo.is_empty() {
            text.push_str(&format!("Repo:     {repo}\n"));
        }
        text.push_str(&format!(
            "Sessions: {}\n",
            result
                .get("session_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
        ));
        text.push_str(&format!(
            "Messages: {}\n",
            result
                .get("message_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
        ));
        let tok = sum_tokens(&result);
        text.push_str(&format!("Tokens:   {}\n", format_tokens(tok)));
        let cost = result
            .get("cost_cents")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        text.push_str(&format!("Cost:     {}\n", format_dollars(cost / 100.0)));

        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[tool(
        description = "Get cost breakdown by tag. Tags include: ticket_id, activity (bugfix/feature/refactor/question/ops), user, composer_mode, permission_mode, duration (short/medium/long), dominant_tool, cost_confidence, and custom tags."
    )]
    async fn get_tag_breakdown(
        &self,
        params: Parameters<TagRequest>,
    ) -> Result<CallToolResult, McpError> {
        let (since, until) = period_to_dates(&params.0.period);
        let period_label = period_label(&params.0.period);
        let key = &params.0.key;

        let mut query = build_params(since.as_deref(), until.as_deref());
        query.push(("key".to_string(), key.to_string()));
        query.push(("limit".to_string(), "30".to_string()));
        let tags: Vec<Value> = self.daemon_get("/analytics/tags", &query)?;

        let mut text = format!("Tag: {key} ({period_label})\n");
        if tags.is_empty() {
            text.push_str("No data for this tag/period combination.");
        } else {
            text.push_str(&format!(
                "{:<40} {:>8} {:>10}\n",
                "VALUE", "SESSIONS", "COST"
            ));
            for t in &tags {
                let val = t.get("value").and_then(|v| v.as_str()).unwrap_or("?");
                let sessions = t.get("session_count").and_then(|v| v.as_u64()).unwrap_or(0);
                let cost = t.get("cost_cents").and_then(|v| v.as_f64()).unwrap_or(0.0);
                text.push_str(&format!(
                    "{:<40} {:>8} {:>10}\n",
                    val,
                    sessions,
                    format_dollars(cost / 100.0)
                ));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[tool(
        description = "Get cost breakdown by AI coding agent/provider (e.g. Claude Code, Cursor). Shows tokens, messages, and cost per provider."
    )]
    async fn get_provider_breakdown(
        &self,
        params: Parameters<PeriodRequest>,
    ) -> Result<CallToolResult, McpError> {
        let (since, until) = period_to_dates(&params.0.period);
        let period_label = period_label(&params.0.period);

        let providers: Vec<Value> = self.daemon_get(
            "/analytics/providers",
            &build_params(since.as_deref(), until.as_deref()),
        )?;

        let mut text = format!("Provider Breakdown ({period_label})\n");
        if providers.is_empty() {
            text.push_str("No data for this period.");
        } else {
            text.push_str(&format!(
                "{:<16} {:>8} {:>10} {:>10}\n",
                "PROVIDER", "MSGS", "TOKENS", "COST"
            ));
            for p in &providers {
                let name = p
                    .get("display_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let msgs = p.get("message_count").and_then(|v| v.as_u64()).unwrap_or(0);
                let tok = p.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0)
                    + p.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0)
                    + p.get("cache_creation_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0)
                    + p.get("cache_read_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                let cost = p
                    .get("total_cost_cents")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let cost = if cost > 0.0 {
                    cost / 100.0
                } else {
                    p.get("estimated_cost")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0)
                };
                text.push_str(&format!(
                    "{:<16} {:>8} {:>10} {:>10}\n",
                    name,
                    msgs,
                    format_tokens(tok),
                    format_dollars(cost)
                ));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[tool(
        description = "Get tool call frequency stats from hook events. Shows which tools (Read, Edit, Bash, etc.) are used most."
    )]
    async fn get_tool_usage(
        &self,
        params: Parameters<PeriodRequest>,
    ) -> Result<CallToolResult, McpError> {
        let (since, until) = period_to_dates(&params.0.period);
        let period_label = period_label(&params.0.period);

        let tools: Vec<Value> = self.daemon_get(
            "/analytics/tools",
            &build_params(since.as_deref(), until.as_deref()),
        )?;

        let mut text = format!("Tool Usage ({period_label})\n");
        if tools.is_empty() {
            text.push_str("No tool usage data for this period.");
        } else {
            text.push_str(&format!("{:<30} {:>10}\n", "TOOL", "CALLS"));
            for t in &tools {
                let name = t.get("tool_name").and_then(|v| v.as_str()).unwrap_or("?");
                let count = t.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
                text.push_str(&format!("{:<30} {:>10}\n", name, count));
            }
        }

        // Also fetch MCP server stats
        let mcp_servers: Vec<Value> = self.daemon_get(
            "/analytics/mcp",
            &build_params(since.as_deref(), until.as_deref()),
        )?;

        if !mcp_servers.is_empty() {
            text.push_str("\nMCP Server Usage:\n");
            text.push_str(&format!("{:<30} {:>10}\n", "SERVER", "CALLS"));
            for s in &mcp_servers {
                let name = s.get("mcp_server").and_then(|v| v.as_str()).unwrap_or("?");
                let count = s.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
                text.push_str(&format!("{:<30} {:>10}\n", name, count));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[tool(
        description = "Get daily activity chart data: messages and cost per day for the selected period."
    )]
    async fn get_activity(
        &self,
        params: Parameters<PeriodRequest>,
    ) -> Result<CallToolResult, McpError> {
        let (since, until) = period_to_dates(&params.0.period);
        let period_label = period_label(&params.0.period);

        let activity: Vec<Value> = self.daemon_get(
            "/analytics/activity",
            &build_params(since.as_deref(), until.as_deref()),
        )?;

        let mut text = format!("Daily Activity ({period_label})\n");
        if activity.is_empty() {
            text.push_str("No activity data for this period.");
        } else {
            text.push_str(&format!("{:<12} {:>8} {:>10}\n", "DATE", "MSGS", "COST"));
            for day in &activity {
                let date = day.get("date").and_then(|v| v.as_str()).unwrap_or("?");
                let msgs = day
                    .get("message_count")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let cost = day
                    .get("cost_cents")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                text.push_str(&format!(
                    "{:<12} {:>8} {:>10}\n",
                    date,
                    msgs,
                    format_dollars(cost / 100.0)
                ));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    // ─── Configuration tools ────────────────────────────────────────

    #[tool(
        description = "Get current budi configuration, including daemon host/port, statusline config, tag rules, and installed hook status."
    )]
    async fn get_config(&self) -> Result<CallToolResult, McpError> {
        let mut text = String::from("Budi Configuration\n\n");

        // Daemon config
        text.push_str(&format!("Daemon: {}\n", self.base_url));

        // Health check
        match self.daemon_get_raw("/health", &[]) {
            Ok(resp) if resp.status().is_success() => {
                let body: Value = resp.json().unwrap_or_default();
                let version = body.get("version").and_then(|v| v.as_str()).unwrap_or("?");
                text.push_str(&format!("Status: running (v{version})\n"));
            }
            _ => {
                text.push_str("Status: not running\n");
            }
        }

        // Schema version
        if let Ok(sv) = self.daemon_get::<Value>("/analytics/schema-version", &[]) {
            let current = sv.get("current").and_then(|v| v.as_u64()).unwrap_or(0);
            text.push_str(&format!("Schema: v{current}\n"));
        }

        // Statusline config
        let sl_config = budi_core::config::load_statusline_config();
        text.push_str(&format!("\nStatusline slots: {:?}\n", sl_config.slots));
        if let Some(ref fmt) = sl_config.format {
            text.push_str(&format!("Statusline format: {fmt}\n"));
        }

        // Tag rules
        if let Some(tags_config) = budi_core::config::load_tags_config() {
            if tags_config.rules.is_empty() {
                text.push_str("\nTag rules: none configured\n");
            } else {
                text.push_str(&format!(
                    "\nTag rules: {} rule(s)\n",
                    tags_config.rules.len()
                ));
                for rule in &tags_config.rules {
                    text.push_str(&format!("  {} = \"{}\"", rule.key, rule.value));
                    if let Some(ref repo) = rule.match_repo {
                        text.push_str(&format!(" (repo: {repo})"));
                    }
                    text.push('\n');
                }
            }
        } else {
            text.push_str("\nTag rules: none configured\n");
        }

        // Config file paths
        text.push_str(&format!(
            "\nConfig files:\n  Tags: {}\n  Statusline: {}\n",
            budi_core::config::tags_config_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "?".to_string()),
            budi_core::config::statusline_config_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "?".to_string()),
        ));

        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[tool(
        description = "Set custom tag rules in ~/.config/budi/tags.toml. Tag rules automatically tag messages matching repo patterns. Example TOML:\n[[rules]]\nkey = \"team\"\nvalue = \"backend\"\nmatch_repo = \"*api*\""
    )]
    async fn set_tag_rules(
        &self,
        params: Parameters<TagRulesRequest>,
    ) -> Result<CallToolResult, McpError> {
        // Validate TOML first
        let config: budi_core::config::TagsConfig = toml::from_str(&params.0.toml_content)
            .map_err(|e| McpError::invalid_params(format!("Invalid TOML: {e}"), None))?;

        let path = budi_core::config::tags_config_path()
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        }
        std::fs::write(&path, &params.0.toml_content)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let text = format!(
            "Tag rules saved to {}\n{} rule(s) configured.\nRun `budi sync --force` to re-tag existing messages.",
            path.display(),
            config.rules.len()
        );
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[tool(
        description = "Configure the statusline in ~/.config/budi/statusline.toml. Controls which cost slots are shown in your shell prompt. Available slots: today, week, month, session, branch, project, provider. Example TOML:\nslots = [\"today\", \"week\", \"month\"]\nformat = \"{today} | {week} | {month}\""
    )]
    async fn set_statusline_config(
        &self,
        params: Parameters<StatuslineConfigRequest>,
    ) -> Result<CallToolResult, McpError> {
        // Validate TOML
        let config: budi_core::config::StatuslineConfig = toml::from_str(&params.0.toml_content)
            .map_err(|e| McpError::invalid_params(format!("Invalid TOML: {e}"), None))?;

        let path = budi_core::config::statusline_config_path()
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        }
        std::fs::write(&path, &params.0.toml_content)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let text = format!(
            "Statusline config saved to {}\nSlots: {:?}\nRestart Claude Code to see changes.",
            path.display(),
            config.required_slots()
        );
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[tool(
        description = "Trigger a data sync to refresh analytics with latest transcripts. Use force=true to re-ingest from scratch (after upgrades)."
    )]
    async fn sync_data(&self) -> Result<CallToolResult, McpError> {
        let resp = self
            .client
            .post(format!("{}/sync", self.base_url))
            .json(&serde_json::json!({ "migrate": false }))
            .timeout(Duration::from_secs(120))
            .send()
            .map_err(|e| McpError::internal_error(format!("Daemon not reachable: {e}"), None))?;

        if !resp.status().is_success() {
            let body = resp.text().unwrap_or_default();
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "Sync failed: {body}"
            ))]));
        }

        let body: Value = resp
            .json()
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let files = body.get("files").and_then(|v| v.as_u64()).unwrap_or(0);
        let messages = body.get("messages").and_then(|v| v.as_u64()).unwrap_or(0);

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Sync complete: {files} files processed, {messages} messages ingested."
        ))]))
    }

    #[tool(
        description = "Get budi health status: daemon, database, schema version, and sync state."
    )]
    async fn get_status(&self) -> Result<CallToolResult, McpError> {
        let mut text = String::from("Budi Status\n\n");

        // Health
        match self.daemon_get_raw("/health", &[]) {
            Ok(resp) if resp.status().is_success() => {
                let body: Value = resp.json().unwrap_or_default();
                let version = body.get("version").and_then(|v| v.as_str()).unwrap_or("?");
                text.push_str(&format!("Daemon: running (v{version})\n"));
            }
            _ => {
                text.push_str("Daemon: NOT running. Run `budi init` to start.\n");
                return Ok(CallToolResult::success(vec![Content::text(text)]));
            }
        }

        // Sync status
        if let Ok(sync) = self.daemon_get::<Value>("/sync/status", &[]) {
            let syncing = sync
                .get("syncing")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            text.push_str(&format!(
                "Sync: {}\n",
                if syncing { "in progress" } else { "idle" }
            ));
        }

        // Schema
        if let Ok(sv) = self.daemon_get::<Value>("/analytics/schema-version", &[]) {
            let current = sv.get("current").and_then(|v| v.as_u64()).unwrap_or(0);
            let target = sv.get("target").and_then(|v| v.as_u64()).unwrap_or(0);
            if current >= target {
                text.push_str(&format!("Schema: v{current} (up to date)\n"));
            } else {
                text.push_str(&format!(
                    "Schema: v{current} (needs migration to v{target})\n"
                ));
            }
        }

        // Database path
        if let Ok(db_path) = budi_core::analytics::db_path() {
            text.push_str(&format!("Database: {}\n", db_path.display()));
        }

        // Dashboard URL
        text.push_str(&format!("Dashboard: {}/dashboard\n", self.base_url));

        Ok(CallToolResult::success(vec![Content::text(text)]))
    }
}

// ─── ServerHandler impl ─────────────────────────────────────────────────────

#[rmcp::tool_handler]
impl rmcp::handler::server::ServerHandler for BudiMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder().enable_tools().build(),
        )
        .with_instructions(
            "Budi: local-first cost analytics for AI coding agents. \
             Query your Claude Code and Cursor spending by model, project, branch, tag, and time period. \
             Configure tag rules, statusline, and trigger data syncs.",
        )
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

impl BudiMcpServer {
    fn daemon_get<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        params: &[(String, String)],
    ) -> Result<T, McpError> {
        let resp = self
            .client
            .get(format!("{}{}", self.base_url, path))
            .query(params)
            .send()
            .map_err(|e| {
                McpError::internal_error(
                    format!(
                        "budi daemon not reachable at {}. Run `budi init` to start it. Error: {e}",
                        self.base_url
                    ),
                    None,
                )
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            return Err(McpError::internal_error(
                format!("Daemon returned {status}: {body}"),
                None,
            ));
        }

        resp.json()
            .map_err(|e| McpError::internal_error(format!("Invalid response: {e}"), None))
    }

    fn daemon_get_raw(
        &self,
        path: &str,
        params: &[(String, String)],
    ) -> Result<reqwest::blocking::Response, McpError> {
        self.client
            .get(format!("{}{}", self.base_url, path))
            .query(params)
            .send()
            .map_err(|e| {
                McpError::internal_error(
                    format!(
                        "budi daemon not reachable at {}. Run `budi init` to start it. Error: {e}",
                        self.base_url
                    ),
                    None,
                )
            })
    }
}

fn period_to_dates(period: &str) -> (Option<String>, Option<String>) {
    let today = Local::now().date_naive();
    match period {
        "today" => {
            let since = local_midnight_to_utc(today);
            (Some(since), None)
        }
        "week" => {
            let weekday = today.weekday().num_days_from_monday();
            let monday = today - chrono::Duration::days(weekday as i64);
            let since = local_midnight_to_utc(monday);
            (Some(since), None)
        }
        "month" => {
            let first = NaiveDate::from_ymd_opt(today.year(), today.month(), 1)
                .expect("valid first-of-month date");
            let since = local_midnight_to_utc(first);
            (Some(since), None)
        }
        "all" => (None, None),
        _ => {
            // Default to month
            let first = NaiveDate::from_ymd_opt(today.year(), today.month(), 1)
                .expect("valid first-of-month date");
            let since = local_midnight_to_utc(first);
            (Some(since), None)
        }
    }
}

fn local_midnight_to_utc(date: NaiveDate) -> String {
    let local_dt = Local
        .from_local_datetime(&date.and_hms_opt(0, 0, 0).unwrap())
        .latest()
        .unwrap_or_else(|| chrono::Utc::now().with_timezone(&Local));
    local_dt.with_timezone(&chrono::Utc).to_rfc3339()
}

fn period_label(period: &str) -> &str {
    match period {
        "today" => "Today",
        "week" => "This week",
        "month" => "This month",
        "all" => "All time",
        _ => "This month",
    }
}

fn build_params(since: Option<&str>, until: Option<&str>) -> Vec<(String, String)> {
    let mut params = Vec::new();
    if let Some(s) = since {
        params.push(("since".to_string(), s.to_string()));
    }
    if let Some(u) = until {
        params.push(("until".to_string(), u.to_string()));
    }
    params
}

fn format_dollars(d: f64) -> String {
    if d >= 1000.0 {
        format!("${:.1}K", d / 1000.0)
    } else if d >= 100.0 {
        format!("${:.0}", d)
    } else if d > 0.0 {
        format!("${:.2}", d)
    } else {
        "$0.00".to_string()
    }
}

fn format_tokens(n: u64) -> String {
    if n >= 1_000_000_000 {
        format!("{:.1}B", n as f64 / 1_000_000_000.0)
    } else if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        format!("{n}")
    }
}

fn sum_tokens(v: &Value) -> u64 {
    v.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0)
        + v.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0)
        + v.get("cache_read_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
        + v.get("cache_creation_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
}

/// Simple URL encoding for path segments (branch names can contain /).
fn urlencoding_simple(s: &str) -> String {
    s.replace('%', "%25")
        .replace('/', "%2F")
        .replace(' ', "%20")
        .replace('#', "%23")
        .replace('?', "%3F")
}
