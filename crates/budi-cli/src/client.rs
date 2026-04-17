//! HTTP client for the budi daemon API.
//!
//! All analytics queries go through the daemon so it is the single owner of
//! the SQLite database.

use std::io::Write;
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use budi_core::analytics::{
    ActivityCost, ActivityCostDetail, BranchCost, ModelUsage, PaginatedSessions, ProviderStats,
    RepoUsage, SessionHealth, SessionListEntry, SessionTag, TagCost, TicketCost, TicketCostDetail,
    UsageSummary,
};
use budi_core::config::{self, BudiConfig};
use budi_core::cost::CostEstimate;
use reqwest::blocking::{Client, Response};
use serde_json::Value;

use crate::daemon::{daemon_health, ensure_daemon_running};

/// Build `GET .../analytics/branches/{branch}` with correct path encoding (slashes, spaces, etc.).
fn analytics_branch_detail_url(base_url: &str, branch: &str) -> Result<String> {
    let normalized = format!("{}/", base_url.trim_end_matches('/'));
    let mut url = reqwest::Url::parse(&normalized)
        .with_context(|| format!("invalid daemon base URL: {base_url}"))?;
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("invalid daemon base URL: {base_url}"))?
        .push("analytics")
        .push("branches")
        .push(branch);
    Ok(url.to_string())
}

/// Build `GET .../analytics/tickets/{ticket_id}` with correct path encoding.
/// Mirrors `analytics_branch_detail_url` so ticket IDs containing `-` or `/`
/// (rare, but possible if a future provider chooses richer IDs) round-trip
/// through the daemon untouched.
fn analytics_ticket_detail_url(base_url: &str, ticket_id: &str) -> Result<String> {
    let normalized = format!("{}/", base_url.trim_end_matches('/'));
    let mut url = reqwest::Url::parse(&normalized)
        .with_context(|| format!("invalid daemon base URL: {base_url}"))?;
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("invalid daemon base URL: {base_url}"))?
        .push("analytics")
        .push("tickets")
        .push(ticket_id);
    Ok(url.to_string())
}

/// Build `GET .../analytics/activities/{name}` with correct path encoding.
/// Mirrors `analytics_ticket_detail_url` so activity values that include
/// unusual characters (future classifier output) round-trip untouched.
fn analytics_activity_detail_url(base_url: &str, activity: &str) -> Result<String> {
    let normalized = format!("{}/", base_url.trim_end_matches('/'));
    let mut url = reqwest::Url::parse(&normalized)
        .with_context(|| format!("invalid daemon base URL: {base_url}"))?;
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("invalid daemon base URL: {base_url}"))?
        .push("analytics")
        .push("activities")
        .push(activity);
    Ok(url.to_string())
}

/// Produce a user-friendly error message based on the kind of reqwest error.
fn describe_send_error(e: reqwest::Error) -> anyhow::Error {
    let log_hint = daemon_log_hint();
    if e.is_connect() {
        anyhow::anyhow!(
            "daemon is not running — start it with `budi init`, or run `budi doctor` to diagnose. \
             If this repeats, rerun `budi init`.{log_hint}"
        )
    } else if e.is_timeout() {
        anyhow::anyhow!(
            "daemon timed out — first sync or full history can take several minutes. Run `budi doctor` to check status"
        )
    } else {
        anyhow::anyhow!(
            "cannot reach daemon: {e} — run `budi doctor` to diagnose. \
             If this repeats, rerun `budi init`.{log_hint}"
        )
    }
}

fn daemon_log_hint() -> String {
    budi_core::config::budi_home_dir()
        .map(|p| p.join("logs").join("daemon.log"))
        .map(|p| format!(" Check daemon log: {}.", p.display()))
        .unwrap_or_default()
}

fn run_with_sync_heartbeat<T, E>(
    op: &impl Fn() -> std::result::Result<T, E>,
) -> std::result::Result<T, E> {
    let running = Arc::new(AtomicBool::new(true));
    let running_flag = running.clone();

    let heartbeat = thread::spawn(move || {
        let mut elapsed_secs = 0u64;
        while running_flag.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_secs(15));
            if !running_flag.load(Ordering::Relaxed) {
                break;
            }
            elapsed_secs += 15;
            print!(" {elapsed_secs}s...");
            let _ = std::io::stdout().flush();
        }
    });

    let result = op();
    running.store(false, Ordering::Relaxed);
    let _ = heartbeat.join();
    result
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

fn ensure_daemon_ready<H, E>(
    repo_root: Option<&Path>,
    config: &BudiConfig,
    daemon_is_healthy: H,
    ensure_running: E,
) -> Result<()>
where
    H: Fn(&BudiConfig) -> bool,
    E: Fn(Option<&Path>, &BudiConfig) -> Result<()>,
{
    let was_healthy = daemon_is_healthy(config);
    ensure_running(repo_root, config).with_context(|| {
        if was_healthy {
            "Failed to validate or restart budi daemon. Run `budi doctor` to diagnose."
        } else {
            "Failed to start budi daemon. Run `budi doctor` to diagnose."
        }
    })
}

impl DaemonClient {
    /// Create a new client, auto-starting the daemon if needed.
    pub fn connect() -> Result<Self> {
        let config = Self::load_config();
        let base_url = config.daemon_base_url();
        let repo_root = std::env::current_dir()
            .ok()
            .and_then(|cwd| config::find_repo_root(&cwd).ok());

        // Always run readiness checks, even when health is green.
        // `ensure_daemon_running` also handles version mismatches by restarting stale daemons.
        ensure_daemon_ready(
            repo_root.as_deref(),
            &config,
            daemon_health,
            ensure_daemon_running,
        )?;

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

    fn wait_for_sync(&self) -> Result<()> {
        print!(" sync in progress, waiting");
        let _ = std::io::stdout().flush();
        let start = std::time::Instant::now();
        let max_wait = Duration::from_secs(300);
        loop {
            std::thread::sleep(Duration::from_secs(2));
            if start.elapsed() > max_wait {
                println!();
                anyhow::bail!(
                    "timed out waiting for running sync to finish — run `budi doctor` to check status"
                );
            }
            let ok = self
                .client
                .get(format!("{}/sync/status", self.base_url))
                .send()
                .ok()
                .and_then(|r| r.json::<Value>().ok())
                .and_then(|v| v.get("syncing")?.as_bool())
                .unwrap_or(false);
            if !ok {
                print!(" ");
                let _ = std::io::stdout().flush();
                return Ok(());
            }
            print!(".");
            let _ = std::io::stdout().flush();
        }
    }

    fn sync_request(
        &self,
        send: impl Fn() -> std::result::Result<Response, reqwest::Error>,
    ) -> Result<Value> {
        let resp = run_with_sync_heartbeat(&send).map_err(describe_send_error)?;
        if resp.status() == reqwest::StatusCode::CONFLICT {
            self.wait_for_sync()?;
            let resp = run_with_sync_heartbeat(&send).map_err(describe_send_error)?;
            let resp = check_response(resp)?;
            return Ok(resp.json()?);
        }
        let resp = check_response(resp)?;
        Ok(resp.json()?)
    }

    pub fn history(&self) -> Result<Value> {
        self.sync_request(|| {
            self.client
                .post(format!("{}/sync/all", self.base_url))
                .timeout(Duration::from_secs(600))
                .send()
        })
    }

    pub fn sync_reset(&self) -> Result<Value> {
        self.sync_request(|| {
            self.client
                .post(format!("{}/sync/reset", self.base_url))
                .timeout(Duration::from_secs(600))
                .send()
        })
    }

    pub fn migrate(&self) -> Result<Value> {
        let resp = self
            .client
            .post(format!("{}/admin/migrate", self.base_url))
            .timeout(std::time::Duration::from_secs(600))
            .send()
            .map_err(describe_send_error)?;
        let resp = check_response(resp)?;
        Ok(resp.json()?)
    }

    pub fn repair(&self) -> Result<Value> {
        let resp = self
            .client
            .post(format!("{}/admin/repair", self.base_url))
            .timeout(std::time::Duration::from_secs(600))
            .send()
            .map_err(describe_send_error)?;
        let resp = check_response(resp)?;
        Ok(resp.json()?)
    }

    pub fn schema_version(&self) -> Result<Value> {
        let resp = self
            .client
            .get(format!("{}/admin/schema", self.base_url))
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
        repo_id: Option<&str>,
        since: Option<&str>,
        until: Option<&str>,
    ) -> Result<Option<BranchCost>> {
        let mut params = Vec::new();
        if let Some(repo) = repo_id {
            params.push(("repo_id", repo));
        }
        if let Some(s) = since {
            params.push(("since", s));
        }
        if let Some(u) = until {
            params.push(("until", u));
        }
        let url = analytics_branch_detail_url(&self.base_url, branch)
            .with_context(|| format!("invalid daemon base URL or branch: {branch:?}"))?;
        let resp = self
            .client
            .get(url)
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

    /// `GET /analytics/tickets` — per-ticket cost roll-up. Matches the
    /// daemon's `TicketListParams` shape (date window + dimension filters +
    /// limit). Used by `budi stats --tickets`.
    pub fn tickets(
        &self,
        since: Option<&str>,
        until: Option<&str>,
        limit: usize,
    ) -> Result<Vec<TicketCost>> {
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
            .get(format!("{}/analytics/tickets", self.base_url))
            .query(&params)
            .send()
            .map_err(describe_send_error)?;
        let resp = check_response(resp)?;
        Ok(resp.json()?)
    }

    /// `GET /analytics/tickets/{id}` — single-ticket detail with per-branch
    /// breakdown. Returns `Ok(None)` for an unknown ticket id (mirrors
    /// `branch_detail`).
    pub fn ticket_detail(
        &self,
        ticket_id: &str,
        repo_id: Option<&str>,
        since: Option<&str>,
        until: Option<&str>,
    ) -> Result<Option<TicketCostDetail>> {
        let mut params: Vec<(&str, &str)> = Vec::new();
        if let Some(repo) = repo_id {
            params.push(("repo_id", repo));
        }
        if let Some(s) = since {
            params.push(("since", s));
        }
        if let Some(u) = until {
            params.push(("until", u));
        }
        let url = analytics_ticket_detail_url(&self.base_url, ticket_id)
            .with_context(|| format!("invalid daemon base URL or ticket: {ticket_id:?}"))?;
        let resp = self
            .client
            .get(url)
            .query(&params)
            .send()
            .map_err(describe_send_error)?;
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

    /// `GET /analytics/activities` — per-activity cost roll-up. Used by
    /// `budi stats --activities`. Mirrors `tickets` so operators can swap
    /// `--tickets` / `--activities` without learning a new query shape.
    pub fn activities(
        &self,
        since: Option<&str>,
        until: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ActivityCost>> {
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
            .get(format!("{}/analytics/activities", self.base_url))
            .query(&params)
            .send()
            .map_err(describe_send_error)?;
        let resp = check_response(resp)?;
        Ok(resp.json()?)
    }

    /// `GET /analytics/activities/{name}` — single-activity detail with
    /// per-branch breakdown. Returns `Ok(None)` for an unknown activity
    /// (mirrors `ticket_detail`).
    pub fn activity_detail(
        &self,
        activity: &str,
        repo_id: Option<&str>,
        since: Option<&str>,
        until: Option<&str>,
    ) -> Result<Option<ActivityCostDetail>> {
        let mut params: Vec<(&str, &str)> = Vec::new();
        if let Some(repo) = repo_id {
            params.push(("repo_id", repo));
        }
        if let Some(s) = since {
            params.push(("since", s));
        }
        if let Some(u) = until {
            params.push(("until", u));
        }
        let url = analytics_activity_detail_url(&self.base_url, activity)
            .with_context(|| format!("invalid daemon base URL or activity: {activity:?}"))?;
        let resp = self
            .client
            .get(url)
            .query(&params)
            .send()
            .map_err(describe_send_error)?;
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

    #[allow(clippy::too_many_arguments)]
    pub fn sessions(
        &self,
        since: Option<&str>,
        until: Option<&str>,
        search: Option<&str>,
        ticket: Option<&str>,
        activity: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<PaginatedSessions> {
        let mut params: Vec<(&str, String)> = Vec::new();
        if let Some(s) = since {
            params.push(("since", s.to_string()));
        }
        if let Some(u) = until {
            params.push(("until", u.to_string()));
        }
        if let Some(q) = search {
            params.push(("search", q.to_string()));
        }
        if let Some(t) = ticket {
            params.push(("ticket", t.to_string()));
        }
        if let Some(a) = activity {
            params.push(("activity", a.to_string()));
        }
        params.push(("sort_by", "started_at".to_string()));
        params.push(("limit", limit.to_string()));
        params.push(("offset", offset.to_string()));
        let resp = self
            .client
            .get(format!("{}/analytics/sessions", self.base_url))
            .query(&params)
            .send()
            .map_err(describe_send_error)?;
        let resp = check_response(resp)?;
        Ok(resp.json()?)
    }

    pub fn session_detail(&self, session_id: &str) -> Result<Option<SessionListEntry>> {
        let resp = self
            .client
            .get(format!(
                "{}/analytics/sessions/{}",
                self.base_url, session_id
            ))
            .send()
            .map_err(describe_send_error)?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let resp = check_response(resp)?;
        Ok(Some(resp.json()?))
    }

    pub fn session_tags(&self, session_id: &str) -> Result<Vec<SessionTag>> {
        let resp = self
            .client
            .get(format!(
                "{}/analytics/sessions/{}/tags",
                self.base_url, session_id
            ))
            .send()
            .map_err(describe_send_error)?;
        let resp = check_response(resp)?;
        Ok(resp.json()?)
    }

    pub fn session_health(&self, session_id: Option<&str>) -> Result<SessionHealth> {
        let mut params: Vec<(&str, &str)> = Vec::new();
        if let Some(s) = session_id {
            params.push(("session_id", s));
        }
        let resp = self
            .client
            .get(format!("{}/analytics/session-health", self.base_url))
            .query(&params)
            .send()
            .map_err(describe_send_error)?;
        let resp = check_response(resp)?;
        Ok(resp.json()?)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[test]
    fn ensure_daemon_ready_checks_running_daemon_too() {
        let config = BudiConfig::default();
        let ensure_calls = Cell::new(0usize);

        let result = ensure_daemon_ready(
            None,
            &config,
            |_| true,
            |_, _| {
                ensure_calls.set(ensure_calls.get() + 1);
                Ok(())
            },
        );

        assert!(result.is_ok());
        assert_eq!(ensure_calls.get(), 1);
    }

    #[test]
    fn ensure_daemon_ready_still_checks_when_daemon_is_down() {
        let config = BudiConfig::default();
        let ensure_calls = Cell::new(0usize);

        let result = ensure_daemon_ready(
            None,
            &config,
            |_| false,
            |_, _| {
                ensure_calls.set(ensure_calls.get() + 1);
                Ok(())
            },
        );

        assert!(result.is_ok());
        assert_eq!(ensure_calls.get(), 1);
    }

    #[test]
    fn ensure_daemon_ready_uses_startup_error_context_when_unhealthy() {
        let config = BudiConfig::default();
        let err = ensure_daemon_ready(None, &config, |_| false, |_, _| anyhow::bail!("boom"))
            .expect_err("should fail");

        assert!(
            err.to_string()
                .contains("Failed to start budi daemon. Run `budi doctor` to diagnose."),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn ensure_daemon_ready_uses_mismatch_error_context_when_healthy() {
        let config = BudiConfig::default();
        let err = ensure_daemon_ready(None, &config, |_| true, |_, _| anyhow::bail!("boom"))
            .expect_err("should fail");

        assert!(
            err.to_string().contains(
                "Failed to validate or restart budi daemon. Run `budi doctor` to diagnose."
            ),
            "unexpected error: {err}"
        );
    }
}
