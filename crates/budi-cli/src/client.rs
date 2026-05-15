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
    fn parse_needs_migration_error_extracts_message() {
        // Body text was renamed `budi db migrate` → `budi db check --fix`
        // in 8.3.14 (#586). The wire contract (`needs_migration: true`)
        // is unchanged; only the human-readable verb in `error` moved.
        let body = r#"{"ok":false,"error":"analytics schema is v0, daemon expects v1; run `budi db check --fix` (or `budi init`) to upgrade","needs_migration":true,"current":0,"target":1}"#;
        let msg = parse_needs_migration_error(body).expect("body matches #366 contract");
        assert!(
            msg.contains("analytics schema is v0, daemon expects v1"),
            "unexpected message: {msg}"
        );
        assert!(
            msg.contains("budi db check --fix"),
            "should mention budi db check --fix"
        );
    }

    #[test]
    fn parse_needs_migration_error_skips_unrelated_503() {
        let body = r#"{"ok":false,"error":"cloud backend unreachable"}"#;
        assert!(parse_needs_migration_error(body).is_none());
    }

    #[test]
    fn parse_needs_migration_error_skips_non_json() {
        assert!(parse_needs_migration_error("").is_none());
        assert!(parse_needs_migration_error("not json").is_none());
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

    // ─── #682: breakdown methods forward `--provider` as `?providers=` ───
    //
    // Each breakdown HTTP method must thread the CLI `--provider` flag into
    // a `providers=` query parameter so the daemon's `DimensionParams` (which
    // aliases `providers` → `agents`) can scope the SQL filter. Pre-#682 the
    // CLI accepted `--provider X` for the summary view only and silently
    // dropped it on every breakdown — the bug this ticket fixes.

    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    /// Spin up a one-shot HTTP server on 127.0.0.1, capture the first
    /// request's path+query, respond with `body`, and return the captured
    /// request line. The empty JSON body matches `BreakdownPage<T>` for any
    /// `T` that has no required fields beyond the ones below.
    fn one_shot_server(body: &'static str) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            // First line is `GET /path?query HTTP/1.1`.
            let request_line = req.lines().next().unwrap_or("").to_string();
            let _ = tx.send(request_line);
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes());
        });
        (format!("http://127.0.0.1:{port}"), rx)
    }

    fn assert_providers_forwarded(request_line: &str, expected: &str) {
        assert!(
            request_line.contains(&format!("providers={expected}")),
            "expected `providers={expected}` in request line, got: {request_line}"
        );
    }

    /// Empty `BreakdownPage` JSON. Works for every `T` because both
    /// `rows` and `other` are absent / empty. Produced as a `&'static str`
    /// so the spawned thread has no lifetime issues.
    const EMPTY_PAGE_BODY: &str =
        r#"{"rows":[],"total_cost_cents":0.0,"total_rows":0,"shown_rows":0,"limit":5}"#;

    #[test]
    fn projects_forwards_provider_filter() {
        let (base, rx) = one_shot_server(EMPTY_PAGE_BODY);
        let client = DaemonClient::for_tests(base);
        let _ = client
            .projects(None, None, Some("copilot_chat"), &[], 5)
            .expect("projects call");
        let req = rx.recv_timeout(Duration::from_secs(5)).expect("captured");
        assert_providers_forwarded(&req, "copilot_chat");
    }

    #[test]
    fn branches_forwards_provider_filter() {
        let (base, rx) = one_shot_server(EMPTY_PAGE_BODY);
        let client = DaemonClient::for_tests(base);
        let _ = client
            .branches(None, None, Some("copilot_chat"), &[], 5)
            .expect("branches call");
        let req = rx.recv_timeout(Duration::from_secs(5)).expect("captured");
        assert_providers_forwarded(&req, "copilot_chat");
    }

    #[test]
    fn tickets_forwards_provider_filter() {
        let (base, rx) = one_shot_server(EMPTY_PAGE_BODY);
        let client = DaemonClient::for_tests(base);
        let _ = client
            .tickets(None, None, Some("copilot_chat"), &[], 5)
            .expect("tickets call");
        let req = rx.recv_timeout(Duration::from_secs(5)).expect("captured");
        assert_providers_forwarded(&req, "copilot_chat");
    }

    #[test]
    fn activities_forwards_provider_filter() {
        let (base, rx) = one_shot_server(EMPTY_PAGE_BODY);
        let client = DaemonClient::for_tests(base);
        let _ = client
            .activities(None, None, Some("copilot_chat"), &[], 5)
            .expect("activities call");
        let req = rx.recv_timeout(Duration::from_secs(5)).expect("captured");
        assert_providers_forwarded(&req, "copilot_chat");
    }

    #[test]
    fn files_forwards_provider_filter() {
        let (base, rx) = one_shot_server(EMPTY_PAGE_BODY);
        let client = DaemonClient::for_tests(base);
        let _ = client
            .files(None, None, Some("copilot_chat"), &[], 5)
            .expect("files call");
        let req = rx.recv_timeout(Duration::from_secs(5)).expect("captured");
        assert_providers_forwarded(&req, "copilot_chat");
    }

    #[test]
    fn models_forwards_provider_filter() {
        let (base, rx) = one_shot_server(EMPTY_PAGE_BODY);
        let client = DaemonClient::for_tests(base);
        let _ = client
            .models(None, None, Some("copilot_chat"), &[], 5)
            .expect("models call");
        let req = rx.recv_timeout(Duration::from_secs(5)).expect("captured");
        assert_providers_forwarded(&req, "copilot_chat");
    }

    #[test]
    fn breakdown_omits_providers_when_filter_is_none() {
        // `--provider` unset must not synthesize a stray `providers=` —
        // the daemon would treat empty-string as "filter to nothing".
        let (base, rx) = one_shot_server(EMPTY_PAGE_BODY);
        let client = DaemonClient::for_tests(base);
        let _ = client
            .models(None, None, None, &[], 5)
            .expect("models call");
        let req = rx.recv_timeout(Duration::from_secs(5)).expect("captured");
        assert!(
            !req.contains("providers="),
            "no provider filter must omit the param entirely, got: {req}"
        );
    }

    // ─── #822: mock-server coverage for every public client method ───
    //
    // The block above (added in #682) exercised provider-forwarding for the
    // six breakdown methods. Everything below was added in #822 to drive
    // `cli/src/client.rs` line coverage above the 65% threshold required
    // by the 8.5.2 quality bar. Each test stands up the existing one-shot
    // TCP listener with a configurable status + body, calls one method, and
    // asserts either:
    //   - happy path: the daemon returns a representative body and the call
    //     yields `Ok(...)` with the expected request path/query, OR
    //   - error path: the daemon returns a non-2xx (or special-cased body)
    //     and the call yields an `Err` (or the documented Ok-with-error
    //     shape for `pricing_refresh`'s 502 branch).

    /// One-shot server with configurable status + body. Mirrors
    /// `one_shot_server` but lets the caller drive non-2xx paths through
    /// `check_response`. Returns `(base_url, captured_request_line)`.
    fn mock_response(status: u16, body: &'static str) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 8192];
            let n = stream.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let request_line = req.lines().next().unwrap_or("").to_string();
            let _ = tx.send(request_line);
            let reason = match status {
                200 => "OK",
                204 => "No Content",
                400 => "Bad Request",
                404 => "Not Found",
                409 => "Conflict",
                500 => "Internal Server Error",
                502 => "Bad Gateway",
                503 => "Service Unavailable",
                _ => "X",
            };
            let resp = format!(
                "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                status,
                reason,
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes());
        });
        (format!("http://127.0.0.1:{port}"), rx)
    }

    const USAGE_SUMMARY_BODY: &str = r#"{"total_messages":3,"total_user_messages":1,"total_assistant_messages":2,"total_input_tokens":100,"total_output_tokens":50,"total_cache_creation_tokens":0,"total_cache_read_tokens":0,"total_cost_cents":1.5}"#;
    const COST_BODY: &str = r#"{"total_cost":1.0,"input_cost":0.5,"output_cost":0.3,"cache_write_cost":0.1,"cache_read_cost":0.1,"cache_savings":0.0}"#;
    const STATUS_SNAPSHOT_BODY: &str = r#"{"summary":{"total_messages":0,"total_user_messages":0,"total_assistant_messages":0,"total_input_tokens":0,"total_output_tokens":0,"total_cache_creation_tokens":0,"total_cache_read_tokens":0,"total_cost_cents":0.0},"cost":{"total_cost":0.0,"input_cost":0.0,"output_cost":0.0,"cache_write_cost":0.0,"cache_read_cost":0.0,"cache_savings":0.0},"providers":[]}"#;
    const SYNC_RESPONSE_BODY: &str =
        r#"{"files_synced":1,"messages_ingested":2,"warnings":[],"per_provider":[]}"#;
    const SYNC_STATUS_BODY: &str =
        r#"{"syncing":false,"ingest_backlog":0,"ingest_ready":0,"ingest_failed":0}"#;
    const SESSION_HEALTH_BODY: &str = r#"{"state":"ok","message_count":1,"total_cost_cents":0.0,"vitals":{},"tip":"keep going","details":[]}"#;
    const SESSION_ENTRY_BODY: &str = r#"{"id":"s1","started_at":null,"ended_at":null,"duration_ms":null,"message_count":0,"cost_cents":0.0,"models":[],"provider":"claude_code","repo_ids":[],"git_branches":[],"input_tokens":0,"output_tokens":0,"cost_confidence":"high"}"#;
    const PAGINATED_SESSIONS_BODY: &str = r#"{"sessions":[],"total_count":0}"#;
    const RESOLVED_SESSION_BODY: &str = r#"{"session_id":"abc","source":"latest","fallback_reason":"no cwd-encoded match — falling back to newest session"}"#;
    const BRANCH_DETAIL_BODY: &str = r#"{"git_branch":"main","repo_id":"r","session_count":1,"message_count":1,"input_tokens":0,"output_tokens":0,"cache_read_tokens":0,"cache_creation_tokens":0,"cost_cents":1.0}"#;
    const TICKET_DETAIL_BODY: &str = r#"{"ticket_id":"T-1","ticket_prefix":"T","session_count":1,"message_count":1,"input_tokens":0,"output_tokens":0,"cache_read_tokens":0,"cache_creation_tokens":0,"cost_cents":1.0,"repo_id":"r","branches":[]}"#;
    const ACTIVITY_DETAIL_BODY: &str = r#"{"activity":"bugfix","session_count":1,"message_count":1,"input_tokens":0,"output_tokens":0,"cache_read_tokens":0,"cache_creation_tokens":0,"cost_cents":1.0,"repo_id":"r","branches":[]}"#;
    const FILE_DETAIL_BODY: &str = r#"{"file_path":"src/main.rs","session_count":1,"message_count":1,"input_tokens":0,"output_tokens":0,"cache_read_tokens":0,"cache_creation_tokens":0,"cost_cents":1.0,"repo_id":"r","branches":[],"tickets":[]}"#;

    fn run_with<F, T>(status: u16, body: &'static str, call: F) -> (Result<T>, String)
    where
        F: FnOnce(&DaemonClient) -> Result<T>,
    {
        let (base, rx) = mock_response(status, body);
        let client = DaemonClient::for_tests(base);
        let result = call(&client);
        let req = rx.recv_timeout(Duration::from_secs(5)).unwrap_or_default();
        (result, req)
    }

    // ─── check_response branches ────────────────────────────────────────

    #[test]
    fn check_response_500_includes_body_in_error() {
        let (res, _) =
            run_with::<_, UsageSummary>(500, "boom-details", |c| c.summary(None, None, None, &[]));
        let err = res.expect_err("500 must error");
        let s = err.to_string();
        assert!(s.contains("500"), "missing status: {s}");
        assert!(s.contains("boom-details"), "missing body: {s}");
    }

    #[test]
    fn check_response_500_empty_body_yields_status_only_error() {
        let (res, _) = run_with::<_, UsageSummary>(500, "", |c| c.summary(None, None, None, &[]));
        let err = res.expect_err("500 must error");
        let s = err.to_string();
        assert!(
            s.contains("Daemon returned") && s.contains("500"),
            "unexpected: {s}"
        );
        assert!(!s.contains(":"), "no body suffix expected: {s}");
    }

    #[test]
    fn check_response_503_with_needs_migration_uses_friendly_message() {
        let body = r#"{"ok":false,"error":"schema v0, daemon expects v1; run `budi db check --fix`","needs_migration":true,"current":0,"target":1}"#;
        let (res, _) = run_with::<_, UsageSummary>(503, body, |c| c.summary(None, None, None, &[]));
        let err = res.expect_err("503 needs-migration must error");
        let s = err.to_string();
        assert!(
            s.contains("schema v0, daemon expects v1"),
            "should surface needs_migration error: {s}"
        );
        assert!(
            s.contains("budi db check --fix"),
            "should retain CLI hint: {s}"
        );
    }

    #[test]
    fn check_response_503_unrelated_falls_back_to_raw_body() {
        let body = r#"{"ok":false,"error":"cloud backend unreachable"}"#;
        let (res, _) = run_with::<_, UsageSummary>(503, body, |c| c.summary(None, None, None, &[]));
        let err = res.expect_err("503 must error");
        let s = err.to_string();
        assert!(s.contains("503"), "missing status: {s}");
        assert!(s.contains("cloud backend unreachable"), "raw body: {s}");
    }

    // ─── describe_send_error ────────────────────────────────────────────

    #[test]
    fn unreachable_daemon_yields_friendly_connect_error() {
        // 127.0.0.1:1 is reserved; no service listens there.
        let client = DaemonClient::for_tests("http://127.0.0.1:1");
        let err = client
            .summary(None, None, None, &[])
            .expect_err("must fail to connect");
        let s = err.to_string();
        assert!(
            s.contains("daemon is not running") || s.contains("cannot reach daemon"),
            "unexpected error: {s}"
        );
    }

    // ─── Sync & migration ───────────────────────────────────────────────

    #[test]
    fn history_happy_path_posts_sync_all() {
        let (res, req) = run_with(200, SYNC_RESPONSE_BODY, |c| c.history());
        let sync = res.expect("history Ok");
        assert_eq!(sync.files_synced, 1);
        assert_eq!(sync.messages_ingested, 2);
        assert!(req.contains("POST /sync/all"), "wrong route: {req}");
    }

    #[test]
    fn history_propagates_error() {
        let (res, _) = run_with::<_, SyncResponse>(500, "", |c| c.history());
        assert!(res.is_err(), "non-200 must surface as Err");
    }

    #[test]
    fn sync_reset_happy_path_posts_sync_reset() {
        let (res, req) = run_with(200, SYNC_RESPONSE_BODY, |c| c.sync_reset());
        let _sync = res.expect("sync_reset Ok");
        assert!(req.contains("POST /sync/reset"), "wrong route: {req}");
    }

    #[test]
    fn sync_status_happy_path() {
        let (res, req) = run_with(200, SYNC_STATUS_BODY, |c| c.sync_status());
        let status = res.expect("sync_status Ok");
        assert!(!status.syncing);
        assert!(req.contains("GET /sync/status"), "wrong route: {req}");
    }

    // ─── Admin ──────────────────────────────────────────────────────────

    #[test]
    fn check_happy_path() {
        let (res, req) = run_with(200, r#"{"ok":true}"#, |c| c.check());
        let v = res.expect("check Ok");
        assert_eq!(v.get("ok").and_then(Value::as_bool), Some(true));
        assert!(req.contains("GET /admin/check"), "wrong route: {req}");
    }

    #[test]
    fn repair_happy_path() {
        let (res, req) = run_with(200, r#"{"repaired":3}"#, |c| c.repair());
        let _v = res.expect("repair Ok");
        assert!(req.contains("POST /admin/repair"), "wrong route: {req}");
    }

    // ─── Cloud ──────────────────────────────────────────────────────────

    #[test]
    fn cloud_sync_happy_path() {
        let (res, req) = run_with(200, r#"{"ok":true,"result":"ok"}"#, |c| c.cloud_sync());
        let v = res.expect("cloud_sync Ok");
        assert_eq!(v.get("ok").and_then(Value::as_bool), Some(true));
        assert!(req.contains("POST /cloud/sync"), "wrong route: {req}");
    }

    #[test]
    fn cloud_sync_propagates_error() {
        let (res, _) = run_with::<_, Value>(500, "internal", |c| c.cloud_sync());
        assert!(res.is_err(), "5xx must surface as Err");
    }

    #[test]
    fn cloud_reset_happy_path() {
        let (res, req) = run_with(200, r#"{"ok":true,"removed":5}"#, |c| c.cloud_reset());
        let _v = res.expect("cloud_reset Ok");
        assert!(req.contains("POST /cloud/reset"), "wrong route: {req}");
    }

    #[test]
    fn cloud_status_happy_path() {
        let (res, req) = run_with(200, r#"{"enabled":false}"#, |c| c.cloud_status());
        let _v = res.expect("cloud_status Ok");
        assert!(req.contains("GET /cloud/status"), "wrong route: {req}");
    }

    // ─── Pricing ────────────────────────────────────────────────────────

    #[test]
    fn pricing_status_happy_path() {
        let (res, req) = run_with(200, r#"{"layer":"shipped"}"#, |c| c.pricing_status());
        let v = res.expect("pricing_status Ok");
        assert_eq!(v.get("layer").and_then(Value::as_str), Some("shipped"));
        assert!(req.contains("GET /pricing/status"), "wrong route: {req}");
    }

    #[test]
    fn pricing_refresh_happy_path() {
        let (res, req) = run_with(200, r#"{"ok":true,"version":"42"}"#, |c| {
            c.pricing_refresh()
        });
        let v = res.expect("pricing_refresh Ok");
        assert_eq!(v.get("ok").and_then(Value::as_bool), Some(true));
        assert!(req.contains("POST /pricing/refresh"), "wrong route: {req}");
    }

    #[test]
    fn pricing_refresh_502_validation_body_returns_ok_with_structured_error() {
        // #493: a 502 with `{"ok":false,"error":...}` must be surfaced as
        // an Ok value (the CLI renderer distinguishes ok=false on its own
        // side) rather than swallowed by `check_response`.
        let body = r#"{"ok":false,"error":"manifest validation failed: unknown model 'foo'"}"#;
        let (res, _) = run_with(502, body, |c| c.pricing_refresh());
        let v = res.expect("structured 502 must round-trip as Ok");
        assert_eq!(v.get("ok").and_then(Value::as_bool), Some(false));
        assert!(
            v.get("error")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .contains("manifest validation failed"),
            "error message preserved: {v:?}"
        );
    }

    #[test]
    fn pricing_refresh_502_unstructured_body_errors() {
        // Plain 502 (e.g. proxy in front of the daemon) — must error with
        // a hint pointing at `budi doctor`.
        let (res, _) = run_with::<_, Value>(502, "Bad Gateway", |c| c.pricing_refresh());
        let err = res.expect_err("unstructured 502 must error");
        let s = err.to_string();
        assert!(s.contains("502"), "should mention status: {s}");
        assert!(s.contains("Bad Gateway"), "should include body: {s}");
    }

    #[test]
    fn pricing_refresh_other_status_errors_with_body() {
        let (res, _) = run_with::<_, Value>(500, "kaboom", |c| c.pricing_refresh());
        let err = res.expect_err("500 must error");
        let s = err.to_string();
        assert!(s.contains("500"), "{s}");
        assert!(s.contains("kaboom"), "should include body: {s}");
    }

    #[test]
    fn pricing_refresh_other_status_empty_body_errors() {
        let (res, _) = run_with::<_, Value>(500, "", |c| c.pricing_refresh());
        let err = res.expect_err("500 must error");
        assert!(err.to_string().contains("500"));
    }

    #[test]
    fn pricing_recompute_force_true_sends_true_query() {
        let (res, req) = run_with(200, r#"{"ok":true}"#, |c| c.pricing_recompute(true));
        let _ = res.expect("pricing_recompute Ok");
        assert!(req.contains("force=true"), "force=true expected: {req}");
        assert!(
            req.contains("POST /pricing/recompute"),
            "wrong route: {req}"
        );
    }

    #[test]
    fn pricing_recompute_force_false_sends_false_query() {
        let (res, req) = run_with(200, r#"{"ok":true}"#, |c| c.pricing_recompute(false));
        let _ = res.expect("pricing_recompute Ok");
        assert!(req.contains("force=false"), "force=false expected: {req}");
    }

    // ─── Analytics: summary / cost / status_snapshot ────────────────────

    #[test]
    fn summary_forwards_all_query_params() {
        let (res, req) = run_with(200, USAGE_SUMMARY_BODY, |c| {
            c.summary(
                Some("2026-01-01"),
                Some("2026-02-01"),
                Some("claude_code"),
                &["vscode".to_string(), "cursor".to_string()],
            )
        });
        let summary = res.expect("summary Ok");
        assert_eq!(summary.total_messages, 3);
        assert!(req.contains("GET /analytics/summary"), "wrong route: {req}");
        assert!(req.contains("since=2026-01-01"), "since: {req}");
        assert!(req.contains("until=2026-02-01"), "until: {req}");
        assert!(req.contains("provider=claude_code"), "provider: {req}");
        // Surfaces are joined on ',' before reqwest's query encoder turns
        // it into `vscode%2Ccursor`.
        assert!(
            req.contains("surfaces=vscode%2Ccursor"),
            "surfaces csv: {req}"
        );
    }

    #[test]
    fn summary_omits_optional_params_when_none() {
        let (res, req) = run_with(200, USAGE_SUMMARY_BODY, |c| {
            c.summary(None, None, None, &[])
        });
        let _ = res.expect("summary Ok");
        assert!(!req.contains("since="), "no since expected: {req}");
        assert!(!req.contains("until="), "no until expected: {req}");
        assert!(!req.contains("provider="), "no provider expected: {req}");
        assert!(!req.contains("surfaces="), "no surfaces expected: {req}");
    }

    #[test]
    fn cost_happy_path_forwards_params() {
        let (res, req) = run_with(200, COST_BODY, |c| {
            c.cost(
                Some("2026-01-01"),
                Some("2026-02-01"),
                Some("copilot_chat"),
                &["jetbrains".to_string()],
            )
        });
        let cost = res.expect("cost Ok");
        assert!((cost.total_cost - 1.0).abs() < f64::EPSILON);
        assert!(req.contains("GET /analytics/cost"), "wrong route: {req}");
        assert!(req.contains("provider=copilot_chat"), "provider: {req}");
        assert!(req.contains("surfaces=jetbrains"), "surfaces: {req}");
    }

    #[test]
    fn status_snapshot_happy_path() {
        let (res, req) = run_with(200, STATUS_SNAPSHOT_BODY, |c| {
            c.status_snapshot(None, None, None, &[])
        });
        let _snap = res.expect("status_snapshot Ok");
        assert!(
            req.contains("GET /analytics/status_snapshot"),
            "wrong route: {req}"
        );
    }

    // ─── Analytics: list breakdowns ─────────────────────────────────────

    #[test]
    fn projects_happy_path_forwards_window_and_limit() {
        let (res, req) = run_with(200, EMPTY_PAGE_BODY, |c| {
            c.projects(
                Some("2026-01-01"),
                Some("2026-02-01"),
                None,
                &["vscode".to_string()],
                7,
            )
        });
        let _ = res.expect("projects Ok");
        assert!(
            req.contains("GET /analytics/projects"),
            "wrong route: {req}"
        );
        assert!(req.contains("limit=7"), "limit: {req}");
        assert!(req.contains("surfaces=vscode"), "surfaces: {req}");
    }

    #[test]
    fn non_repo_happy_path() {
        let (res, req) = run_with(200, "[]", |c| c.non_repo(Some("2026-01-01"), None, 3));
        let rows = res.expect("non_repo Ok");
        assert!(rows.is_empty());
        assert!(
            req.contains("GET /analytics/non_repo"),
            "wrong route: {req}"
        );
        assert!(req.contains("limit=3"), "limit: {req}");
    }

    #[test]
    fn tags_happy_path_forwards_key_and_window() {
        let (res, req) = run_with(200, EMPTY_PAGE_BODY, |c| {
            c.tags(Some("env"), Some("2026-01-01"), None, 9)
        });
        let _ = res.expect("tags Ok");
        assert!(req.contains("GET /analytics/tags"), "wrong route: {req}");
        assert!(req.contains("key=env"), "key: {req}");
        assert!(req.contains("limit=9"), "limit: {req}");
    }

    #[test]
    fn providers_happy_path_emits_empty_list() {
        let (res, req) = run_with(200, "[]", |c| {
            c.providers(None, None, &["vscode".to_string()])
        });
        let stats = res.expect("providers Ok");
        assert!(stats.is_empty());
        assert!(
            req.contains("GET /analytics/providers"),
            "wrong route: {req}"
        );
        assert!(req.contains("surfaces=vscode"), "surfaces: {req}");
    }

    #[test]
    fn surfaces_happy_path() {
        let (res, req) = run_with(200, "[]", |c| {
            c.surfaces(
                Some("2026-01-01"),
                Some("2026-02-01"),
                &["jetbrains".to_string()],
            )
        });
        let _ = res.expect("surfaces Ok");
        assert!(
            req.contains("GET /analytics/surfaces"),
            "wrong route: {req}"
        );
        assert!(req.contains("surfaces=jetbrains"), "surfaces: {req}");
    }

    // ─── Analytics: detail endpoints with 404 → None ───────────────────

    #[test]
    fn branch_detail_present_returns_some() {
        let (res, req) = run_with(200, BRANCH_DETAIL_BODY, |c| {
            c.branch_detail("main", Some("r1"), Some("2026-01-01"), None)
        });
        let detail = res.expect("branch_detail Ok").expect("Some");
        assert_eq!(detail.git_branch, "main");
        assert!(
            req.contains("GET /analytics/branches/main"),
            "wrong route: {req}"
        );
        assert!(req.contains("repo_id=r1"), "repo_id: {req}");
    }

    #[test]
    fn branch_detail_404_returns_none() {
        let (res, _) = run_with(404, "", |c| c.branch_detail("missing", None, None, None));
        assert!(res.expect("Ok(None) on 404").is_none());
    }

    #[test]
    fn branch_detail_null_body_returns_none() {
        let (res, _) = run_with(200, "null", |c| {
            c.branch_detail("missing", None, None, None)
        });
        assert!(res.expect("Ok(None) on null body").is_none());
    }

    #[test]
    fn branch_detail_encodes_slash_in_branch_name() {
        let (_, req) = run_with::<_, Option<BranchCost>>(404, "", |c| {
            c.branch_detail("feat/x y", None, None, None)
        });
        // path_segments_mut percent-encodes `/` to `%2F` and space to `%20`
        assert!(
            req.contains("GET /analytics/branches/feat%2Fx%20y"),
            "encoded branch: {req}"
        );
    }

    #[test]
    fn ticket_detail_present_returns_some() {
        let (res, req) = run_with(200, TICKET_DETAIL_BODY, |c| {
            c.ticket_detail("T-1", None, None, None)
        });
        let detail = res.expect("ticket_detail Ok").expect("Some");
        assert_eq!(detail.ticket_id, "T-1");
        assert!(
            req.contains("GET /analytics/tickets/T-1"),
            "wrong route: {req}"
        );
    }

    #[test]
    fn ticket_detail_404_returns_none() {
        let (res, _) = run_with(404, "", |c| c.ticket_detail("nope", None, None, None));
        assert!(res.expect("Ok(None)").is_none());
    }

    #[test]
    fn ticket_detail_null_body_returns_none() {
        let (res, _) = run_with(200, "null", |c| c.ticket_detail("nope", None, None, None));
        assert!(res.expect("Ok(None)").is_none());
    }

    #[test]
    fn activity_detail_present_returns_some() {
        let (res, req) = run_with(200, ACTIVITY_DETAIL_BODY, |c| {
            c.activity_detail("bugfix", None, None, None)
        });
        let detail = res.expect("activity_detail Ok").expect("Some");
        assert_eq!(detail.activity, "bugfix");
        assert!(
            req.contains("GET /analytics/activities/bugfix"),
            "wrong route: {req}"
        );
    }

    #[test]
    fn activity_detail_404_returns_none() {
        let (res, _) = run_with(404, "", |c| c.activity_detail("nope", None, None, None));
        assert!(res.expect("Ok(None)").is_none());
    }

    #[test]
    fn activity_detail_null_body_returns_none() {
        let (res, _) = run_with(200, "null", |c| c.activity_detail("nope", None, None, None));
        assert!(res.expect("Ok(None)").is_none());
    }

    #[test]
    fn file_detail_present_with_subpath_keeps_slashes_structural() {
        let (res, req) = run_with(200, FILE_DETAIL_BODY, |c| {
            c.file_detail("src/main.rs", Some("r1"), None, None)
        });
        let detail = res.expect("file_detail Ok").expect("Some");
        assert_eq!(detail.file_path, "src/main.rs");
        // Each path segment is pushed individually so `/` stays structural.
        assert!(
            req.contains("GET /analytics/files/src/main.rs"),
            "wrong route: {req}"
        );
        assert!(req.contains("repo_id=r1"), "repo_id: {req}");
    }

    #[test]
    fn file_detail_404_returns_none() {
        let (res, _) = run_with(404, "", |c| {
            c.file_detail("src/missing.rs", None, None, None)
        });
        assert!(res.expect("Ok(None)").is_none());
    }

    #[test]
    fn file_detail_null_body_returns_none() {
        let (res, _) = run_with(200, "null", |c| {
            c.file_detail("src/missing.rs", None, None, None)
        });
        assert!(res.expect("Ok(None)").is_none());
    }

    #[test]
    fn file_detail_skips_empty_path_segments() {
        // `analytics_file_detail_url` filters out empty segments so a
        // leading or doubled `/` doesn't produce a `//` in the URL.
        let (_, req) = run_with::<_, Option<FileCostDetail>>(404, "", |c| {
            c.file_detail("/a//b", None, None, None)
        });
        assert!(
            req.contains("GET /analytics/files/a/b"),
            "collapsed segments: {req}"
        );
    }

    // ─── Analytics: sessions ────────────────────────────────────────────

    #[test]
    fn sessions_forwards_every_filter() {
        let (res, req) = run_with(200, PAGINATED_SESSIONS_BODY, |c| {
            c.sessions(
                Some("2026-01-01"),
                Some("2026-02-01"),
                Some("foo bar"),
                Some("claude_code"),
                &["vscode".to_string()],
                Some("T-1"),
                Some("refactor"),
                10,
                20,
            )
        });
        let page = res.expect("sessions Ok");
        assert_eq!(page.total_count, 0);
        assert!(
            req.contains("GET /analytics/sessions"),
            "wrong route: {req}"
        );
        // Per the comment in `sessions`, --provider rides as `providers=`.
        assert!(req.contains("providers=claude_code"), "providers: {req}");
        assert!(req.contains("ticket=T-1"), "ticket: {req}");
        assert!(req.contains("activity=refactor"), "activity: {req}");
        assert!(req.contains("sort_by=started_at"), "sort_by: {req}");
        assert!(req.contains("limit=10"), "limit: {req}");
        assert!(req.contains("offset=20"), "offset: {req}");
        // `search` is percent-encoded (space → `+` or `%20`).
        assert!(
            req.contains("search=foo+bar") || req.contains("search=foo%20bar"),
            "search: {req}"
        );
    }

    #[test]
    fn session_detail_present_returns_some() {
        let (res, req) = run_with(200, SESSION_ENTRY_BODY, |c| c.session_detail("s1"));
        let entry = res.expect("session_detail Ok").expect("Some");
        assert_eq!(entry.id, "s1");
        assert!(
            req.contains("GET /analytics/sessions/s1"),
            "wrong route: {req}"
        );
    }

    #[test]
    fn session_detail_404_returns_none() {
        let (res, _) = run_with(404, "", |c| c.session_detail("missing"));
        assert!(res.expect("Ok(None)").is_none());
    }

    #[test]
    fn session_tags_happy_path() {
        let (res, req) = run_with(200, "[]", |c| c.session_tags("s1"));
        let tags = res.expect("session_tags Ok");
        assert!(tags.is_empty());
        assert!(
            req.contains("GET /analytics/sessions/s1/tags"),
            "wrong route: {req}"
        );
    }

    #[test]
    fn resolve_session_token_with_cwd_emits_both_params() {
        let (res, req) = run_with(200, RESOLVED_SESSION_BODY, |c| {
            c.resolve_session_token("current", Some("/repo"))
        });
        let resolved = res.expect("resolve Ok");
        assert_eq!(resolved.session_id, "abc");
        assert!(resolved.fallback_reason.is_some(), "fallback_reason set");
        assert!(
            req.contains("GET /analytics/sessions/resolve"),
            "wrong route: {req}"
        );
        assert!(req.contains("token=current"), "token: {req}");
        assert!(req.contains("cwd="), "cwd: {req}");
    }

    #[test]
    fn resolve_session_token_without_cwd_omits_cwd_param() {
        let (res, req) = run_with(200, RESOLVED_SESSION_BODY, |c| {
            c.resolve_session_token("latest", None)
        });
        let _ = res.expect("resolve Ok");
        assert!(req.contains("token=latest"), "token: {req}");
        assert!(!req.contains("cwd="), "no cwd expected: {req}");
    }

    #[test]
    fn session_health_with_id_forwards_param() {
        let (res, req) = run_with(200, SESSION_HEALTH_BODY, |c| c.session_health(Some("s1")));
        let h = res.expect("session_health Ok");
        assert_eq!(h.state, "ok");
        assert!(
            req.contains("GET /analytics/session-health"),
            "wrong route: {req}"
        );
        assert!(req.contains("session_id=s1"), "session_id: {req}");
    }

    #[test]
    fn session_health_without_id_omits_param() {
        let (res, req) = run_with(200, SESSION_HEALTH_BODY, |c| c.session_health(None));
        let _ = res.expect("session_health Ok");
        assert!(!req.contains("session_id="), "no id expected: {req}");
    }
}
