//! HTTP client for the budi daemon API.
//!
//! All analytics queries go through the daemon so it is the single owner of
//! the SQLite database.

use std::time::Duration;

use anyhow::{Context, Result};
use budi_core::analytics::{
    BranchCost, ModelUsage, ProviderStats, RepoUsage,
    TagCost, UsageSummary,
};
use budi_core::config::{self, BudiConfig};
use budi_core::cost::CostEstimate;
use reqwest::blocking::Client;
use serde_json::Value;

use crate::daemon::{daemon_health, ensure_daemon_running};

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
            if let Some(ref root) = repo_root {
                ensure_daemon_running(root, &config)
                    .context("Failed to start budi daemon")?;
            } else {
                anyhow::bail!(
                    "budi daemon is not running at {}. Start it with `budi init` in a repo.",
                    base_url
                );
            }
        }

        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;

        Ok(Self { base_url, client })
    }

    fn load_config() -> BudiConfig {
        std::env::current_dir()
            .ok()
            .and_then(|cwd| config::find_repo_root(&cwd).ok())
            .and_then(|root| config::load_or_default(&root).ok())
            .unwrap_or_default()
    }

    // ─── Sync & Migration ────────────────────────────────────────────

    pub fn sync(&self, migrate: bool) -> Result<Value> {
        let resp = self
            .client
            .post(format!("{}/sync", self.base_url))
            .json(&serde_json::json!({
                "migrate": migrate,
            }))
            .send()
            .context("Failed to connect to budi daemon")?
            .error_for_status()
            .context("Sync request failed")?;
        Ok(resp.json()?)
    }

    pub fn history(&self) -> Result<Value> {
        let resp = self
            .client
            .post(format!("{}/history", self.base_url))
            .timeout(std::time::Duration::from_secs(600)) // History can take minutes
            .send()
            .context("Failed to connect to budi daemon")?
            .error_for_status()
            .context("History sync request failed")?;
        Ok(resp.json()?)
    }

    pub fn migrate(&self) -> Result<Value> {
        let resp = self
            .client
            .post(format!("{}/migrate", self.base_url))
            .send()
            .context("Failed to connect to budi daemon")?
            .error_for_status()
            .context("Migration request failed")?;
        Ok(resp.json()?)
    }

    pub fn schema_version(&self) -> Result<Value> {
        let resp = self
            .client
            .get(format!("{}/analytics/schema-version", self.base_url))
            .send()
            .context("Failed to connect to budi daemon")?;
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
            .context("Failed to connect to budi daemon")?
            .error_for_status()
            .context("Summary request failed")?;
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
            .context("Failed to connect to budi daemon")?
            .error_for_status()
            .context("Cost request failed")?;
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
            .context("Failed to connect to budi daemon")?
            .error_for_status()
            .context("Projects request failed")?;
        Ok(resp.json()?)
    }

    pub fn branches(
        &self,
        since: Option<&str>,
        until: Option<&str>,
    ) -> Result<Vec<BranchCost>> {
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
            .context("Failed to connect to budi daemon")?
            .error_for_status()
            .context("Branches request failed")?;
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
            .context("Failed to connect to budi daemon")?
            .error_for_status()
            .context("Branch detail request failed")?;
        let val: Value = resp.json()?;
        if val.is_null() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_value(val)?))
    }

    pub fn models(
        &self,
        since: Option<&str>,
        until: Option<&str>,
    ) -> Result<Vec<ModelUsage>> {
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
            .context("Failed to connect to budi daemon")?
            .error_for_status()
            .context("Models request failed")?;
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
            .context("Failed to connect to budi daemon")?
            .error_for_status()
            .context("Tags request failed")?;
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
            .context("Failed to connect to budi daemon")?
            .error_for_status()
            .context("Providers request failed")?;
        Ok(resp.json()?)
    }

    pub fn provider_count(&self) -> Result<usize> {
        let resp = self
            .client
            .get(format!("{}/analytics/provider-count", self.base_url))
            .send()
            .context("Failed to connect to budi daemon")?
            .error_for_status()
            .context("Provider count request failed")?;
        let val: Value = resp.json()?;
        Ok(val
            .get("count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize)
    }

}
