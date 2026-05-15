//! HTTP client for the budi daemon API.
//!
//! All analytics queries go through the daemon so it is the single owner of
//! the SQLite database.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use budi_core::analytics::{
    ActivityCost, ActivityCostDetail, BranchCost, BreakdownPage, FileCost, FileCostDetail,
    ModelUsage, PaginatedSessions, ProviderStats, ProviderSyncStats, RepoUsage, SessionHealth,
    SessionListEntry, SessionTag, StatusSnapshot, SyncProgress, TagCost, TicketCost,
    TicketCostDetail, UsageSummary,
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

/// Build `GET .../analytics/files/{path}` with per-segment encoding.
///
/// File paths are repo-relative and forward-slashed (see
/// `file_attribution::attribute_files`). We split on `/` and push each
/// segment individually so slashes stay structural (axum's `{*file_path}`
/// wildcard receives them as a single joined path) and every other
/// character gets percent-encoded correctly.
fn analytics_file_detail_url(base_url: &str, file_path: &str) -> Result<String> {
    let normalized = format!("{}/", base_url.trim_end_matches('/'));
    let mut url = reqwest::Url::parse(&normalized)
        .with_context(|| format!("invalid daemon base URL: {base_url}"))?;
    {
        let mut segs = url
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("invalid daemon base URL: {base_url}"))?;
        segs.push("analytics").push("files");
        for segment in file_path.split('/').filter(|s| !s.is_empty()) {
            segs.push(segment);
        }
    }
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

/// Check the response status and return a descriptive error for non-success codes.
///
/// `503 Service Unavailable` with a `needs_migration: true` body (#366) is
/// formatted as an actionable error message instead of being echoed as raw
/// JSON. Other non-success codes preserve the body in the error so operators
/// can still see whatever the daemon said.
fn check_response(resp: Response) -> Result<Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().unwrap_or_default();
    if status == reqwest::StatusCode::SERVICE_UNAVAILABLE
        && let Some(msg) = parse_needs_migration_error(&body)
    {
        anyhow::bail!("{msg}");
    }
    if body.is_empty() {
        anyhow::bail!("Daemon returned {status}");
    } else {
        anyhow::bail!("Daemon returned {status}: {body}");
    }
}

/// Return the daemon-provided `error` string from a stale-schema 503 body,
/// or `None` if the body doesn't look like the #366 contract.
///
/// We key off `needs_migration: true` rather than the status code alone so
/// an unrelated future 503 (e.g. "cloud backend unreachable") keeps its
/// existing raw-body formatting.
fn parse_needs_migration_error(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    if v.get("needs_migration")?.as_bool()? {
        let msg = v.get("error")?.as_str()?;
        Some(msg.to_string())
    } else {
        None
    }
}

/// Typed mirror of the daemon's `SyncResponse` (see
/// `budi_daemon::routes::hooks::SyncResponse`). Returned by `POST /sync/all`
/// and `POST /sync/reset`. Used by `budi db import` to render the final
/// per-agent breakdown (#440).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SyncResponse {
    pub files_synced: usize,
    pub messages_ingested: usize,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub per_provider: Vec<ProviderSyncStats>,
}

/// Typed mirror of the daemon's `SyncStatusResponse`. `progress` is populated
/// only while a sync is in flight and cleared once `syncing: false`, so a
/// `Some(..)` value on a `syncing: false` snapshot should be ignored.
///
/// The freshness and ingest-queue fields are carried for wire-format
/// parity with consumers that already key off `/sync/status` (Cursor
/// extension, statusline). `budi db import` only reads `syncing` and
/// `progress`; `#[allow(dead_code)]` keeps the struct whole without
/// tripping the dead-code lint for the unused ones.
#[derive(Debug, Clone, serde::Deserialize)]
#[allow(dead_code)]
pub struct SyncStatusResponse {
    pub syncing: bool,
    #[serde(default)]
    pub last_sync_completed_at: Option<String>,
    #[serde(default)]
    pub newest_data_at: Option<String>,
    #[serde(default)]
    pub ingest_backlog: u64,
    #[serde(default)]
    pub ingest_ready: u64,
    #[serde(default)]
    pub ingest_failed: u64,
    #[serde(default)]
    pub last_synced: Option<String>,
    #[serde(default)]
    pub progress: Option<SyncProgress>,
}

/// Typed mirror of the daemon's `/analytics/sessions/resolve` envelope
/// (#603). Returned by `DaemonClient::resolve_session_token` so the
/// CLI can render the optional `fallback_reason` on stderr without
/// digging through raw JSON.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ResolvedSession {
    pub session_id: String,
    /// `"current"` if the cwd-encoded transcript dir resolved
    /// directly; `"latest"` if we fell back to the newest DB session.
    #[serde(default)]
    #[allow(dead_code)]
    pub source: String,
    /// Human-readable note about a fallback path (`current` → `latest`,
    /// or `current` with no cwd). The CLI surfaces this verbatim on
    /// stderr per the #603 acceptance criteria.
    #[serde(default)]
    pub fallback_reason: Option<String>,
}

/// Thin HTTP client that talks to budi-daemon.
#[derive(Clone)]
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

    /// Build a client pinned to a specific base URL. Test-only — production
    /// callers use [`DaemonClient::connect`] which auto-starts the daemon.
    #[cfg(test)]
    pub(crate) fn for_tests(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            client: Client::builder()
                .timeout(Duration::from_secs(5))
                .connect_timeout(Duration::from_secs(2))
                .build()
                .expect("build test client"),
        }
    }

    pub(crate) fn load_config() -> BudiConfig {
        std::env::current_dir()
            .ok()
            .and_then(|cwd| config::find_repo_root(&cwd).ok())
            .and_then(|root| config::load_or_default(&root).ok())
            .unwrap_or_default()
    }

    // ─── Sync & Migration ────────────────────────────────────────────

    fn send_sync_post(&self, path: &str) -> std::result::Result<Response, reqwest::Error> {
        self.client
            .post(format!(
                "{}/{}",
                self.base_url,
                path.trim_start_matches('/')
            ))
            .timeout(Duration::from_secs(600))
            .send()
    }

    fn sync_request(
        &self,
        send: impl Fn() -> std::result::Result<Response, reqwest::Error>,
    ) -> Result<SyncResponse> {
        let resp = send().map_err(describe_send_error)?;
        if resp.status() == reqwest::StatusCode::CONFLICT {
            // Another sync is already running; wait for it to finish (the
            // import command polls /sync/status itself, so this branch only
            // matters when a caller skipped the CLI progress loop).
            self.wait_for_sync_completion()?;
            let resp = send().map_err(describe_send_error)?;
            let resp = check_response(resp)?;
            return Ok(resp.json()?);
        }
        let resp = check_response(resp)?;
        Ok(resp.json()?)
    }

    /// Block until a 409-indicated in-flight sync reports `syncing: false`.
    /// Separate from the `budi db import` progress loop: this is the fallback
    /// when a caller just wanted to trigger a sync and discovered one was
    /// already running.
    fn wait_for_sync_completion(&self) -> Result<()> {
        let start = std::time::Instant::now();
        let max_wait = Duration::from_secs(300);
        loop {
            std::thread::sleep(Duration::from_secs(2));
            if start.elapsed() > max_wait {
                anyhow::bail!(
                    "timed out waiting for running sync to finish — run `budi doctor` to check status"
                );
            }
            let still_running = self.sync_status().map(|s| s.syncing).unwrap_or(false);
            if !still_running {
                return Ok(());
            }
        }
    }

    /// `POST /sync/all` — run a quick 30-day sync and return the typed
    /// per-agent report. Used by `budi db import` (no `--force`).
    pub fn history(&self) -> Result<SyncResponse> {
        self.sync_request(|| self.send_sync_post("sync/all"))
    }

    /// `POST /sync/reset` — clear sync state and re-ingest all history from
    /// scratch. Used by `budi db import --force`.
    pub fn sync_reset(&self) -> Result<SyncResponse> {
        self.sync_request(|| self.send_sync_post("sync/reset"))
    }

    /// `GET /sync/status` — typed snapshot. The `progress` field is populated
    /// only while a sync is in flight (see `/sync/status` handler).
    /// `budi db import` polls this every ~2 s to render live per-agent progress.
    pub fn sync_status(&self) -> Result<SyncStatusResponse> {
        let resp = self
            .client
            .get(format!("{}/sync/status", self.base_url))
            .send()
            .map_err(describe_send_error)?;
        let resp = check_response(resp)?;
        Ok(resp.json()?)
    }

    pub fn check(&self) -> Result<Value> {
        let resp = self
            .client
            .get(format!("{}/admin/check", self.base_url))
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

    /// `POST /cloud/sync` — trigger an immediate cloud flush.
    ///
    /// The daemon runs the same code path as the background worker
    /// (`cloud_sync::sync_tick_report`) and returns a structured JSON body
    /// that the CLI renders via `commands::cloud`. Non-2xx responses are
    /// surfaced as `anyhow` errors via [`check_response`]; a successful HTTP
    /// status still encodes per-result outcomes (`auth_failure`,
    /// `transient_error`, …) in `result`.
    pub fn cloud_sync(&self) -> Result<Value> {
        let resp = self
            .client
            .post(format!("{}/cloud/sync", self.base_url))
            .timeout(std::time::Duration::from_secs(120))
            .send()
            .map_err(describe_send_error)?;
        let resp = check_response(resp)?;
        Ok(resp.json()?)
    }

    /// `POST /cloud/reset` — drop the cloud-sync watermarks so the next
    /// sync re-uploads every local rollup + session summary (#564).
    ///
    /// Returns the same `{ok, result, removed, message}` shape on success
    /// and a 409 `busy` error when the worker / a manual sync are mid-flight.
    pub fn cloud_reset(&self) -> Result<Value> {
        let resp = self
            .client
            .post(format!("{}/cloud/reset", self.base_url))
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .map_err(describe_send_error)?;
        let resp = check_response(resp)?;
        Ok(resp.json()?)
    }

    /// `GET /cloud/status` — read cloud sync readiness and watermarks.
    /// Never blocks on the network; the daemon only reads local state.
    pub fn cloud_status(&self) -> Result<Value> {
        let resp = self
            .client
            .get(format!("{}/cloud/status", self.base_url))
            .send()
            .map_err(describe_send_error)?;
        let resp = check_response(resp)?;
        Ok(resp.json()?)
    }

    /// `GET /pricing/status` — snapshot of the in-memory pricing manifest
    /// (layer, version, known model count, unknown models seen). Backs
    /// `budi pricing status` (ADR-0091 §8).
    pub fn pricing_status(&self) -> Result<Value> {
        let resp = self
            .client
            .get(format!("{}/pricing/status", self.base_url))
            .send()
            .map_err(describe_send_error)?;
        let resp = check_response(resp)?;
        Ok(resp.json()?)
    }

    /// `POST /pricing/refresh` — trigger an immediate LiteLLM manifest
    /// refresh, bypassing the worker's 24 h cadence. Longer timeout than
    /// `pricing_status` because this actually hits the network and runs
    /// validation + atomic write + backfill.
    ///
    /// #493 (RC-3): the daemon returns 502 on a validation failure with
    /// a structured `{"ok": false, "error": "..."}` body. Those
    /// validation bodies are a first-class signal for the CLI renderer,
    /// not a generic "Bad Gateway". This method short-circuits
    /// `check_response` whenever the 502 payload parses as that shape
    /// so the caller sees the structured body; the CLI in
    /// `cmd_pricing_status` already distinguishes `body.ok == true`
    /// from `body.ok == false` on the rendering side.
    pub fn pricing_refresh(&self) -> Result<Value> {
        let resp = self
            .client
            .post(format!("{}/pricing/refresh", self.base_url))
            .timeout(std::time::Duration::from_secs(60))
            .send()
            .map_err(describe_send_error)?;
        let status = resp.status();
        if status.is_success() {
            return Ok(resp.json()?);
        }
        if status == reqwest::StatusCode::BAD_GATEWAY {
            let body = resp.text().unwrap_or_default();
            if let Ok(parsed) = serde_json::from_str::<Value>(&body)
                && parsed.get("ok").and_then(Value::as_bool) == Some(false)
                && parsed.get("error").is_some()
            {
                return Ok(parsed);
            }
            anyhow::bail!(
                "Daemon returned 502 on pricing refresh (no JSON body). \
                 Run `budi doctor` to diagnose. Body: {body}"
            );
        }
        let body = resp.text().unwrap_or_default();
        if body.is_empty() {
            anyhow::bail!("Daemon returned {status} on pricing refresh");
        } else {
            anyhow::bail!("Daemon returned {status} on pricing refresh: {body}");
        }
    }

    /// `POST /pricing/recompute` — re-poll the cloud price list and
    /// recompute `messages.cost_cents_effective`. Used by
    /// `budi pricing recompute`. `force=true` runs the recompute even
    /// when `list_version` is unchanged (the worker would otherwise
    /// short-circuit). #732.
    pub fn pricing_recompute(&self, force: bool) -> Result<Value> {
        // #745: the daemon route uses serde's strict bool deserializer, which
        // only accepts the literal strings "true" / "false". A numeric
        // 0 / 1 (the older convention) trips 400 Bad Request.
        let resp = self
            .client
            .post(format!(
                "{}/pricing/recompute?force={}",
                self.base_url,
                if force { "true" } else { "false" }
            ))
            .timeout(std::time::Duration::from_secs(120))
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
        surfaces: &[String],
    ) -> Result<UsageSummary> {
        let mut params: Vec<(&str, String)> = Vec::new();
        if let Some(s) = since {
            params.push(("since", s.to_string()));
        }
        if let Some(u) = until {
            params.push(("until", u.to_string()));
        }
        if let Some(p) = provider {
            params.push(("provider", p.to_string()));
        }
        if !surfaces.is_empty() {
            params.push(("surfaces", surfaces.join(",")));
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
        surfaces: &[String],
    ) -> Result<CostEstimate> {
        let mut params: Vec<(&str, String)> = Vec::new();
        if let Some(s) = since {
            params.push(("since", s.to_string()));
        }
        if let Some(u) = until {
            params.push(("until", u.to_string()));
        }
        if let Some(p) = provider {
            params.push(("provider", p.to_string()));
        }
        if !surfaces.is_empty() {
            params.push(("surfaces", surfaces.join(",")));
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

    /// Single-call snapshot of summary + cost + providers (#619).
    pub fn status_snapshot(
        &self,
        since: Option<&str>,
        until: Option<&str>,
        provider: Option<&str>,
        surfaces: &[String],
    ) -> Result<StatusSnapshot> {
        let mut params: Vec<(&str, String)> = Vec::new();
        if let Some(s) = since {
            params.push(("since", s.to_string()));
        }
        if let Some(u) = until {
            params.push(("until", u.to_string()));
        }
        if let Some(p) = provider {
            params.push(("provider", p.to_string()));
        }
        if !surfaces.is_empty() {
            params.push(("surfaces", surfaces.join(",")));
        }
        let resp = self
            .client
            .get(format!("{}/analytics/status_snapshot", self.base_url))
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
        providers: Option<&str>,
        surfaces: &[String],
        limit: usize,
    ) -> Result<BreakdownPage<RepoUsage>> {
        let mut params: Vec<(&str, String)> = Vec::new();
        if let Some(s) = since {
            params.push(("since", s.to_string()));
        }
        if let Some(u) = until {
            params.push(("until", u.to_string()));
        }
        if let Some(p) = providers {
            params.push(("providers", p.to_string()));
        }
        if !surfaces.is_empty() {
            params.push(("surfaces", surfaces.join(",")));
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

    /// Fetch per-cwd-basename breakdown of non-repo work for the
    /// `--include-non-repo` view on `budi stats --projects` (#442).
    pub fn non_repo(
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
            .get(format!("{}/analytics/non_repo", self.base_url))
            .query(&params)
            .send()
            .map_err(describe_send_error)?;
        let resp = check_response(resp)?;
        Ok(resp.json()?)
    }

    pub fn branches(
        &self,
        since: Option<&str>,
        until: Option<&str>,
        providers: Option<&str>,
        surfaces: &[String],
        limit: usize,
    ) -> Result<BreakdownPage<BranchCost>> {
        let mut params: Vec<(&str, String)> = Vec::new();
        if let Some(s) = since {
            params.push(("since", s.to_string()));
        }
        if let Some(u) = until {
            params.push(("until", u.to_string()));
        }
        if let Some(p) = providers {
            params.push(("providers", p.to_string()));
        }
        if !surfaces.is_empty() {
            params.push(("surfaces", surfaces.join(",")));
        }
        params.push(("limit", limit.to_string()));
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
        providers: Option<&str>,
        surfaces: &[String],
        limit: usize,
    ) -> Result<BreakdownPage<TicketCost>> {
        let mut params: Vec<(&str, String)> = Vec::new();
        if let Some(s) = since {
            params.push(("since", s.to_string()));
        }
        if let Some(u) = until {
            params.push(("until", u.to_string()));
        }
        if let Some(p) = providers {
            params.push(("providers", p.to_string()));
        }
        if !surfaces.is_empty() {
            params.push(("surfaces", surfaces.join(",")));
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
        providers: Option<&str>,
        surfaces: &[String],
        limit: usize,
    ) -> Result<BreakdownPage<ActivityCost>> {
        let mut params: Vec<(&str, String)> = Vec::new();
        if let Some(s) = since {
            params.push(("since", s.to_string()));
        }
        if let Some(u) = until {
            params.push(("until", u.to_string()));
        }
        if let Some(p) = providers {
            params.push(("providers", p.to_string()));
        }
        if !surfaces.is_empty() {
            params.push(("surfaces", surfaces.join(",")));
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

    /// `GET /analytics/files` — per-file cost roll-up. Used by
    /// `budi stats --files`. Mirrors `tickets` / `activities`.
    pub fn files(
        &self,
        since: Option<&str>,
        until: Option<&str>,
        providers: Option<&str>,
        surfaces: &[String],
        limit: usize,
    ) -> Result<BreakdownPage<FileCost>> {
        let mut params: Vec<(&str, String)> = Vec::new();
        if let Some(s) = since {
            params.push(("since", s.to_string()));
        }
        if let Some(u) = until {
            params.push(("until", u.to_string()));
        }
        if let Some(p) = providers {
            params.push(("providers", p.to_string()));
        }
        if !surfaces.is_empty() {
            params.push(("surfaces", surfaces.join(",")));
        }
        params.push(("limit", limit.to_string()));
        let resp = self
            .client
            .get(format!("{}/analytics/files", self.base_url))
            .query(&params)
            .send()
            .map_err(describe_send_error)?;
        let resp = check_response(resp)?;
        Ok(resp.json()?)
    }

    /// `GET /analytics/files/{path}` — single-file detail with per-branch
    /// and per-ticket breakdowns. Returns `Ok(None)` for an unknown file.
    pub fn file_detail(
        &self,
        file_path: &str,
        repo_id: Option<&str>,
        since: Option<&str>,
        until: Option<&str>,
    ) -> Result<Option<FileCostDetail>> {
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
        let url = analytics_file_detail_url(&self.base_url, file_path)
            .with_context(|| format!("invalid daemon base URL or file: {file_path:?}"))?;
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

    pub fn models(
        &self,
        since: Option<&str>,
        until: Option<&str>,
        providers: Option<&str>,
        surfaces: &[String],
        limit: usize,
    ) -> Result<BreakdownPage<ModelUsage>> {
        let mut params: Vec<(&str, String)> = Vec::new();
        if let Some(s) = since {
            params.push(("since", s.to_string()));
        }
        if let Some(u) = until {
            params.push(("until", u.to_string()));
        }
        if let Some(p) = providers {
            params.push(("providers", p.to_string()));
        }
        if !surfaces.is_empty() {
            params.push(("surfaces", surfaces.join(",")));
        }
        params.push(("limit", limit.to_string()));
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
    ) -> Result<BreakdownPage<TagCost>> {
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
        surfaces: &[String],
    ) -> Result<Vec<ProviderStats>> {
        let mut params: Vec<(&str, String)> = Vec::new();
        if let Some(s) = since {
            params.push(("since", s.to_string()));
        }
        if let Some(u) = until {
            params.push(("until", u.to_string()));
        }
        if !surfaces.is_empty() {
            params.push(("surfaces", surfaces.join(",")));
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

    /// `GET /analytics/surfaces` — per-host-environment breakdown (#702).
    /// Mirror of [`Self::providers`] keyed on the surface axis. Returns one
    /// row per surface present in the window; empty surfaces are excluded.
    pub fn surfaces(
        &self,
        since: Option<&str>,
        until: Option<&str>,
        surfaces_filter: &[String],
    ) -> Result<Vec<budi_core::analytics::SurfaceStats>> {
        let mut params: Vec<(&str, String)> = Vec::new();
        if let Some(s) = since {
            params.push(("since", s.to_string()));
        }
        if let Some(u) = until {
            params.push(("until", u.to_string()));
        }
        if !surfaces_filter.is_empty() {
            params.push(("surfaces", surfaces_filter.join(",")));
        }
        let resp = self
            .client
            .get(format!("{}/analytics/surfaces", self.base_url))
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
        provider: Option<&str>,
        surfaces: &[String],
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
        if let Some(p) = provider {
            // `/analytics/sessions` filters via `DimensionParams` (flattened
            // into `SessionsQueryParams`), whose `agents` field aliases
            // `providers`. Send the CLI's already-normalized provider name
            // through that key so the same SQL predicate breakdown routes
            // already use kicks in.
            params.push(("providers", p.to_string()));
        }
        if !surfaces.is_empty() {
            // Same pattern as `providers`: the daemon's `DimensionParams`
            // accepts `surface=` (singular) and `surfaces=` (plural CSV)
            // — pass CSV so multiple `--surface` flags collapse to one
            // query-string entry instead of repeating.
            params.push(("surfaces", surfaces.join(",")));
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

    /// `GET /analytics/sessions/resolve?token=<token>&cwd=<path>` —
    /// server-side resolution for the `current` and `latest` literal
    /// session tokens (#603). Returns the resolved session id plus
    /// an optional `fallback_reason` line the CLI prints on stderr.
    pub fn resolve_session_token(&self, token: &str, cwd: Option<&str>) -> Result<ResolvedSession> {
        let mut params: Vec<(&str, &str)> = vec![("token", token)];
        if let Some(c) = cwd {
            params.push(("cwd", c));
        }
        let resp = self
            .client
            .get(format!("{}/analytics/sessions/resolve", self.base_url))
            .query(&params)
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
mod tests;
