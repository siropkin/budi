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

#[derive(Debug, Clone, Copy, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Period {
    Today,
    Week,
    #[default]
    Month,
    All,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PeriodRequest {
    /// Time period bucket. Valid values: today, week, month, all. Default: month.
    #[serde(default)]
    pub period: Period,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BranchRequest {
    /// Git branch name to query
    pub branch: String,
    /// Optional repository filter (recommended when branch names exist in multiple repos)
    pub repo_id: Option<String>,
    /// Time period bucket. Valid values: today, week, month, all. Default: month.
    #[serde(default)]
    pub period: Period,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TagRequest {
    /// Tag key to break down by (e.g. "ticket_id", "activity", "user", "composer_mode", "permission_mode", "duration", "tool", "cost_confidence")
    pub key: String,
    /// Time period bucket. Valid values: today, week, month, all. Default: month.
    #[serde(default)]
    pub period: Period,
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

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SessionHealthRequest {
    /// Session ID to check. If omitted, uses the most recent session.
    pub session_id: Option<String>,
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
        let (since, until) = period_to_dates(params.0.period);
        let period_label = period_label(params.0.period);

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
        let (since, until) = period_to_dates(params.0.period);
        let period_label = period_label(params.0.period);

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
        let (since, until) = period_to_dates(params.0.period);
        let period_label = period_label(params.0.period);

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
        let (since, until) = period_to_dates(params.0.period);
        let period_label = period_label(params.0.period);

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
        let (since, until) = period_to_dates(params.0.period);
        let period_label = period_label(params.0.period);
        let branch = &params.0.branch;

        let mut query = build_params(since.as_deref(), until.as_deref());
        if let Some(repo) = params.0.repo_id.as_deref() {
            query.push(("repo_id".to_string(), repo.to_string()));
        }
        let url = format!("/analytics/branches/{}", urlencoding_simple(branch));
        let result: Value = match self.daemon_get_raw(&url, &query) {
            Ok((status, body)) => {
                if status == reqwest::StatusCode::NOT_FOUND {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "No data found for branch '{branch}' ({period_label}). Run `budi sync` if you haven't synced recently."
                    ))]));
                }
                serde_json::from_str(&body)
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
        description = "Get cost breakdown by tag. Tags include: ticket_id, activity (bugfix/feature/refactor/question/ops), user, composer_mode, permission_mode, duration (short/medium/long), tool, cost_confidence, and custom tags."
    )]
    async fn get_tag_breakdown(
        &self,
        params: Parameters<TagRequest>,
    ) -> Result<CallToolResult, McpError> {
        let (since, until) = period_to_dates(params.0.period);
        let period_label = period_label(params.0.period);
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
        let (since, until) = period_to_dates(params.0.period);
        let period_label = period_label(params.0.period);

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
        let (since, until) = period_to_dates(params.0.period);
        let period_label = period_label(params.0.period);

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
        let (since, until) = period_to_dates(params.0.period);
        let period_label = period_label(params.0.period);

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
            Ok((status, body)) if status.is_success() => {
                let val: Value = serde_json::from_str(&body).unwrap_or_default();
                let version = val.get("version").and_then(|v| v.as_str()).unwrap_or("?");
                text.push_str(&format!("Status: running (v{version})\n"));
            }
            _ => {
                text.push_str("Status: not running\n");
            }
        }

        // Schema version
        if let Ok(sv) = self.daemon_get::<Value>("/admin/schema", &[]) {
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

        let mut warnings = Vec::new();
        let reserved_keys = &[
            // Canonical dimensions backed by message/session columns.
            "repo",
            "repo_id",
            "branch",
            "git_branch",
            // Auto-generated tags.
            "ticket_id",
            "ticket_prefix",
            "user",
            "machine",
            "session_title",
            "provider",
            "model",
            "speed",
            "cost_confidence",
            "composer_mode",
            "permission_mode",
            "activity",
            "user_email",
            "duration",
            "tool",
        ];
        for rule in &config.rules {
            if rule.key.is_empty() || rule.value.is_empty() {
                warnings.push(format!(
                    "Rule has empty key or value: key={:?} value={:?}",
                    rule.key, rule.value
                ));
            }
            if reserved_keys.contains(&rule.key.as_str()) {
                warnings.push(format!(
                    "Key {:?} collides with a built-in analytics dimension/tag; custom value may conflict",
                    rule.key
                ));
            }
        }

        let path = budi_core::config::tags_config_path()
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        }
        std::fs::write(&path, &params.0.toml_content)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let mut text = format!(
            "Tag rules saved to {}\n{} rule(s) configured.\nRun `budi sync --force` to re-tag existing messages.",
            path.display(),
            config.rules.len()
        );
        for w in &warnings {
            text.push_str(&format!("\nWarning: {w}"));
        }
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

    #[tool(description = "Trigger a data sync to refresh analytics with latest transcripts.")]
    async fn sync_data(&self) -> Result<CallToolResult, McpError> {
        const SYNC_PATH: &str = "/sync";
        let (status, response_body) = tokio::task::block_in_place(|| {
            let resp = self
                .client
                .post(format!("{}{}", self.base_url, SYNC_PATH))
                .json(&serde_json::json!({ "migrate": false }))
                .timeout(Duration::from_secs(120))
                .send()
                .map_err(|e| {
                    McpError::internal_error(daemon_unreachable_message(&self.base_url, e), None)
                })?;
            let status = resp.status();
            let text = resp.text().unwrap_or_default();
            Ok::<_, McpError>((status, text))
        })?;

        if !status.is_success() {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "Sync failed: {}",
                daemon_http_error_message(status, &response_body, SYNC_PATH)
            ))]));
        }

        let body: Value = serde_json::from_str(&response_body)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let files = body
            .get("files_synced")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let messages = body
            .get("messages_ingested")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

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
            Ok((status, body)) if status.is_success() => {
                let val: Value = serde_json::from_str(&body).unwrap_or_default();
                let version = val.get("version").and_then(|v| v.as_str()).unwrap_or("?");
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
        if let Ok(sv) = self.daemon_get::<Value>("/admin/schema", &[]) {
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

    #[tool(
        description = "Check session health: prompt growth, cache reuse, retry loops, and cost acceleration. Returns overall state (green/yellow/red/gray), vitals breakdown, and provider-aware tips. Gray means not enough data yet. Use to decide when to start fresh or trim context."
    )]
    async fn session_health(
        &self,
        params: Parameters<SessionHealthRequest>,
    ) -> Result<CallToolResult, McpError> {
        let mut query: Vec<(String, String)> = Vec::new();
        if let Some(ref sid) = params.0.session_id {
            query.push(("session_id".to_string(), sid.clone()));
        }
        let health: budi_core::analytics::SessionHealth =
            self.daemon_get("/analytics/session-health", &query)?;
        let text = format_session_health_text(&health, params.0.session_id.as_deref());
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
        tokio::task::block_in_place(|| {
            let resp = self
                .client
                .get(format!("{}{}", self.base_url, path))
                .query(params)
                .send()
                .map_err(|e| {
                    McpError::internal_error(daemon_unreachable_message(&self.base_url, e), None)
                })?;

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().unwrap_or_default();
                return Err(McpError::internal_error(
                    daemon_http_error_message(status, &body, path),
                    None,
                ));
            }

            resp.json()
                .map_err(|e| McpError::internal_error(format!("Invalid response: {e}"), None))
        })
    }

    fn daemon_get_raw(
        &self,
        path: &str,
        params: &[(String, String)],
    ) -> Result<(reqwest::StatusCode, String), McpError> {
        tokio::task::block_in_place(|| {
            let resp = self
                .client
                .get(format!("{}{}", self.base_url, path))
                .query(params)
                .send()
                .map_err(|e| {
                    McpError::internal_error(daemon_unreachable_message(&self.base_url, e), None)
                })?;
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            Ok((status, body))
        })
    }
}

fn period_to_dates(period: Period) -> (Option<String>, Option<String>) {
    let today = Local::now().date_naive();
    match period {
        Period::Today => {
            let since = local_midnight_to_utc(today);
            (Some(since), None)
        }
        Period::Week => {
            let weekday = today.weekday().num_days_from_monday();
            let monday = today - chrono::Duration::days(weekday as i64);
            let since = local_midnight_to_utc(monday);
            (Some(since), None)
        }
        Period::Month => {
            let first = NaiveDate::from_ymd_opt(today.year(), today.month(), 1)
                .expect("valid first-of-month date");
            let since = local_midnight_to_utc(first);
            (Some(since), None)
        }
        Period::All => (None, None),
    }
}

fn local_midnight_to_utc(date: NaiveDate) -> String {
    let local_dt = Local
        .from_local_datetime(&date.and_hms_opt(0, 0, 0).unwrap())
        .latest()
        .unwrap_or_else(|| chrono::Utc::now().with_timezone(&Local));
    local_dt.with_timezone(&chrono::Utc).to_rfc3339()
}

fn period_label(period: Period) -> &'static str {
    match period {
        Period::Today => "Today",
        Period::Week => "This week",
        Period::Month => "This month",
        Period::All => "All time",
    }
}

fn daemon_unreachable_message(base_url: &str, err: reqwest::Error) -> String {
    format!(
        "budi daemon is unreachable at {base_url}. Run `budi init` (or `budi doctor`) to start/repair it. Error: {err}"
    )
}

fn daemon_http_error_message(status: reqwest::StatusCode, body: &str, path: &str) -> String {
    let detail = daemon_error_detail(body);
    match status {
        reqwest::StatusCode::CONFLICT => format!(
            "Daemon is busy (often an existing sync/migration in progress). Wait for it to finish, then retry `{path}`. Details: {detail}"
        ),
        reqwest::StatusCode::SERVICE_UNAVAILABLE => format!(
            "Daemon is not ready yet. Wait a few seconds, then retry `{path}`. Details: {detail}"
        ),
        reqwest::StatusCode::NOT_FOUND => format!(
            "Daemon endpoint `{path}` was not found. Restart budi to refresh daemon/CLI compatibility. Details: {detail}"
        ),
        _ if status.is_server_error() => {
            format!("Daemon returned server error {status} for `{path}`. Details: {detail}")
        }
        _ => format!("Daemon returned {status} for `{path}`. Details: {detail}"),
    }
}

fn daemon_error_detail(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return "no response body".to_string();
    }
    let parsed: Result<Value, _> = serde_json::from_str(trimmed);
    if let Ok(val) = parsed {
        if let Some(msg) = val.get("error").and_then(|v| v.as_str()) {
            return msg.to_string();
        }
        if let Some(msg) = val.get("message").and_then(|v| v.as_str()) {
            return msg.to_string();
        }
    }
    trimmed.to_string()
}

fn format_session_health_text(
    health: &budi_core::analytics::SessionHealth,
    session_id: Option<&str>,
) -> String {
    let mut text = String::from("Session Health\n");
    text.push_str(&format!(
        "Session: {}\n",
        session_id.unwrap_or("latest active session")
    ));
    text.push_str(&format!("State: {}\n", health.state.to_uppercase()));
    text.push_str(&format!("Messages: {}\n", health.message_count));
    text.push_str(&format!(
        "Total cost: {}\n",
        format_dollars(health.total_cost_cents / 100.0)
    ));
    text.push_str(&format!("Tip: {}\n", health.tip));

    text.push_str("\nVitals:\n");
    push_vital_line(
        &mut text,
        "Context Growth",
        health.vitals.context_drag.as_ref(),
    );
    push_vital_line(
        &mut text,
        "Cache Reuse",
        health.vitals.cache_efficiency.as_ref(),
    );
    push_vital_line(&mut text, "Retry Loops", health.vitals.thrashing.as_ref());
    push_vital_line(
        &mut text,
        "Cost Acceleration",
        health.vitals.cost_acceleration.as_ref(),
    );

    if !health.details.is_empty() {
        text.push_str("\nDetails:\n");
        for detail in &health.details {
            text.push_str(&format!(
                "- {} [{}]: {}\n",
                detail.vital,
                detail.state.to_uppercase(),
                detail.tip
            ));
            if !detail.actions.is_empty() {
                text.push_str(&format!("  Actions: {}\n", detail.actions.join("; ")));
            }
        }
    }

    text
}

fn push_vital_line(
    text: &mut String,
    name: &str,
    vital: Option<&budi_core::analytics::VitalScore>,
) {
    match vital {
        Some(v) => text.push_str(&format!(
            "- {name}: {} ({})\n",
            v.state.to_uppercase(),
            v.label
        )),
        None => text.push_str(&format!("- {name}: GRAY (not enough data yet)\n")),
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

#[cfg(test)]
mod tests {
    use budi_core::analytics::{HealthDetail, SessionHealth, SessionVitals, VitalScore};
    use serde_json::json;

    use super::*;

    #[test]
    fn period_defaults_to_month() {
        let req: PeriodRequest = serde_json::from_value(json!({})).expect("valid default request");
        assert!(matches!(req.period, Period::Month));
    }

    #[test]
    fn period_rejects_unknown_values() {
        let err = serde_json::from_value::<PeriodRequest>(json!({ "period": "quarter" }))
            .expect_err("unknown period should fail");
        assert!(
            err.to_string().contains("unknown variant"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn daemon_error_prefers_json_error_field() {
        let detail = daemon_error_detail(r#"{ "error": "sync already running" }"#);
        assert_eq!(detail, "sync already running");
    }

    #[test]
    fn conflict_error_message_is_actionable() {
        let msg = daemon_http_error_message(
            reqwest::StatusCode::CONFLICT,
            r#"{ "error": "sync already in progress" }"#,
            "/sync",
        );
        assert!(msg.contains("Daemon is busy"));
        assert!(msg.contains("sync already in progress"));
    }

    #[test]
    fn session_health_text_includes_summary_vitals_and_actions() {
        let health = SessionHealth {
            state: "yellow".to_string(),
            message_count: 42,
            total_cost_cents: 123.45,
            vitals: SessionVitals {
                context_drag: Some(VitalScore {
                    state: "yellow".to_string(),
                    label: "Context grew quickly".to_string(),
                }),
                cache_efficiency: None,
                thrashing: Some(VitalScore {
                    state: "green".to_string(),
                    label: "No loops detected".to_string(),
                }),
                cost_acceleration: None,
            },
            tip: "Trim context with /compact soon.".to_string(),
            details: vec![HealthDetail {
                vital: "context_drag".to_string(),
                state: "yellow".to_string(),
                label: "Context growth".to_string(),
                tip: "Compact now to reduce prompt bloat.".to_string(),
                actions: vec!["Run /compact".to_string(), "Split tasks".to_string()],
            }],
        };

        let text = format_session_health_text(&health, Some("session-123"));
        assert!(text.contains("Session: session-123"));
        assert!(text.contains("State: YELLOW"));
        assert!(text.contains("Total cost: $1.23"));
        assert!(text.contains("- Context Growth: YELLOW (Context grew quickly)"));
        assert!(text.contains("- Cache Reuse: GRAY (not enough data yet)"));
        assert!(text.contains("Actions: Run /compact; Split tasks"));
    }
}
