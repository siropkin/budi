//! HTTP client for the budi daemon API.
//!
//! All analytics queries go through the daemon so it is the single owner of
//! the SQLite database.

use std::time::Duration;

use anyhow::{Context, Result};
use budi_core::analytics::{
    BranchCost, ModelUsage, ProviderStats, RepoUsage, TagCost, UsageSummary,
};
use budi_core::config::{self, BudiConfig};
use budi_core::cost::CostEstimate;
use reqwest::blocking::{Client, Response};
use serde_json::Value;

use crate::daemon::{daemon_health, ensure_daemon_running};

/// Produce a user-friendly error message based on the kind of reqwest error.
fn describe_send_error(e: reqwest::Error) -> anyhow::Error {
    if e.is_connect() {
        anyhow::anyhow!("daemon is not running — start it with `budi init`")
    } else if e.is_timeout() {
        anyhow::anyhow!("daemon timed out — for large syncs, this is normal; try again in a moment")
    } else {
        anyhow::anyhow!("cannot reach daemon: {e} — run `budi doctor` to diagnose")
    }
}

/// Check the response status and return a descriptive error for non-success codes.
fn check_response(resp: Response) -> Result<Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().unwrap_or_default();
    if body.is_empty() {
        anyhow::bail!("Daemon returned {status}");
    } else {
        anyhow::bail!("Daemon returned {status}: {body}");
    }
}

/// Thin HTTP client that talks to budi-daemon.
pub struct DaemonClient {
    base_url: String,
    client: Client,
}

impl DaemonClient {
    /// Create a new client, auto-starting the daemon if needed.
    pub fn connect() -> Result<Self> {
        let config = Self::load_config();
        let base_url = config.daemon_base_url();

        if !daemon_health(&config) {
            // Try to auto-start daemon
            let repo_root = std::env::current_dir()
                .ok()
                .and_then(|cwd| config::find_repo_root(&cwd).ok());
            ensure_daemon_running(repo_root.as_deref(), &config)
                .context("Failed to start budi daemon. Run `budi doctor` to diagnose")?;
        }

        let client = Client::builder()
            .timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(10))
            .build()?;

        Ok(Self { base_url, client })
    }

    pub(crate) fn load_config() -> BudiConfig {
        std::env::current_dir()
            .ok()
            .and_then(|cwd| config::find_repo_root(&cwd).ok())
            .and_then(|root| config::load_or_default(&root).ok())
            .unwrap_or_default()
    }

    // ─── Sync & Migration ────────────────────────────────────────────

    pub fn sync(&self, migrate: bool) -> Result<Value> {
        let timeout = if migrate { 600 } else { 60 };
        self.sync_with_params(migrate, timeout)
    }

    fn sync_with_params(&self, migrate: bool, timeout_secs: u64) -> Result<Value> {
        let resp = self
            .client
            .post(format!("{}/sync", self.base_url))
            .json(&serde_json::json!({
                "migrate": migrate,
            }))
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .send()
            .map_err(describe_send_error)?;
        let resp = check_response(resp)?;
        Ok(resp.json()?)
    }

    pub fn history(&self) -> Result<Value> {
        let resp = self
            .client
            .post(format!("{}/sync/all", self.base_url))
            .timeout(std::time::Duration::from_secs(600)) // History can take minutes
            .send()
            .map_err(describe_send_error)?;
        let resp = check_response(resp)?;
        Ok(resp.json()?)
    }

    pub fn migrate(&self) -> Result<Value> {
        let resp = self
            .client
            .post(format!("{}/analytics/migrate", self.base_url))
            .timeout(std::time::Duration::from_secs(600))
            .send()
            .map_err(describe_send_error)?;
        let resp = check_response(resp)?;
        Ok(resp.json()?)
    }

    pub fn schema_version(&self) -> Result<Value> {
        let resp = self
            .client
            .get(format!("{}/analytics/schema-version", self.base_url))
            .send()
            .map_err(describe_send_error)?;
        let resp = check_response(resp)?;
        Ok(resp.json()?)
    }

    // ─── Analytics ───────────────────────────────────────────────────

    pub fn summary(
        &self,
        since: Option<&str>,
        until: Option<&str>,
        provider: Option<&str>,
    ) -> Result<UsageSummary> {
        let mut params = Vec::new();
        if let Some(s) = since {
            params.push(("since", s));
        }
        if let Some(u) = until {
            params.push(("until", u));
        }
        if let Some(p) = provider {
            params.push(("provider", p));
        }
        let resp = self
            .client
            .get(format!("{}/analytics/summary", self.base_url))
            .query(&params)
            .send()
            .map_err(describe_send_error)?;
        let resp = check_response(resp)?;
        Ok(resp.json()?)
    }

    pub fn cost(
        &self,
        since: Option<&str>,
        until: Option<&str>,
        provider: Option<&str>,
    ) -> Result<CostEstimate> {
        let mut params = Vec::new();
        if let Some(s) = since {
            params.push(("since", s));
        }
        if let Some(u) = until {
            params.push(("until", u));
        }
        if let Some(p) = provider {
            params.push(("provider", p));
        }
        let resp = self
            .client
            .get(format!("{}/analytics/cost", self.base_url))
            .query(&params)
            .send()
            .map_err(describe_send_error)?;
        let resp = check_response(resp)?;
        Ok(resp.json()?)
    }

    pub fn projects(
        &self,
        since: Option<&str>,
        until: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RepoUsage>> {
        let mut params: Vec<(&str, String)> = Vec::new();
        if let Some(s) = since {
            params.push(("since", s.to_string()));
        }
        if let Some(u) = until {
            params.push(("until", u.to_string()));
        }
        params.push(("limit", limit.to_string()));
        let resp = self
            .client
            .get(format!("{}/analytics/projects", self.base_url))
            .query(&params)
            .send()
            .map_err(describe_send_error)?;
        let resp = check_response(resp)?;
        Ok(resp.json()?)
    }

    pub fn branches(&self, since: Option<&str>, until: Option<&str>) -> Result<Vec<BranchCost>> {
        let mut params = Vec::new();
        if let Some(s) = since {
            params.push(("since", s));
        }
        if let Some(u) = until {
            params.push(("until", u));
        }
        let resp = self
            .client
            .get(format!("{}/analytics/branches", self.base_url))
            .query(&params)
            .send()
            .map_err(describe_send_error)?;
        let resp = check_response(resp)?;
        Ok(resp.json()?)
    }

    pub fn branch_detail(
        &self,
        branch: &str,
        since: Option<&str>,
        until: Option<&str>,
    ) -> Result<Option<BranchCost>> {
        let mut params = Vec::new();
        if let Some(s) = since {
            params.push(("since", s));
        }
        if let Some(u) = until {
            params.push(("until", u));
        }
        let resp = self
            .client
            .get(format!("{}/analytics/branches/{}", self.base_url, branch))
            .query(&params)
            .send()
            .map_err(describe_send_error)?;
        // 404 means branch not found — return None instead of error
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let resp = check_response(resp)?;
        let val: Value = resp.json()?;
        if val.is_null() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_value(val)?))
    }

    pub fn models(&self, since: Option<&str>, until: Option<&str>) -> Result<Vec<ModelUsage>> {
        let mut params = Vec::new();
        if let Some(s) = since {
            params.push(("since", s));
        }
        if let Some(u) = until {
            params.push(("until", u));
        }
        let resp = self
            .client
            .get(format!("{}/analytics/models", self.base_url))
            .query(&params)
            .send()
            .map_err(describe_send_error)?;
        let resp = check_response(resp)?;
        Ok(resp.json()?)
    }

    pub fn tags(
        &self,
        key: Option<&str>,
        since: Option<&str>,
        until: Option<&str>,
        limit: usize,
    ) -> Result<Vec<TagCost>> {
        let mut params: Vec<(&str, String)> = Vec::new();
        if let Some(k) = key {
            params.push(("key", k.to_string()));
        }
        if let Some(s) = since {
            params.push(("since", s.to_string()));
        }
        if let Some(u) = until {
            params.push(("until", u.to_string()));
        }
        params.push(("limit", limit.to_string()));
        let resp = self
            .client
            .get(format!("{}/analytics/tags", self.base_url))
            .query(&params)
            .send()
            .map_err(describe_send_error)?;
        let resp = check_response(resp)?;
        Ok(resp.json()?)
    }

    pub fn providers(
        &self,
        since: Option<&str>,
        until: Option<&str>,
    ) -> Result<Vec<ProviderStats>> {
        let mut params = Vec::new();
        if let Some(s) = since {
            params.push(("since", s));
        }
        if let Some(u) = until {
            params.push(("until", u));
        }
        let resp = self
            .client
            .get(format!("{}/analytics/providers", self.base_url))
            .query(&params)
            .send()
            .map_err(describe_send_error)?;
        let resp = check_response(resp)?;
        Ok(resp.json()?)
    }
}
