use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::header;
use axum::response::IntoResponse;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{internal_error, schema_unavailable};
use crate::AppState;

#[derive(serde::Serialize)]
pub struct HealthResponse {
    pub ok: bool,
    pub version: &'static str,
    /// Daemon management API version.  The Cursor extension checks this field
    /// on startup and warns if its expected API version is unsupported.
    /// Bump when a breaking change is made to any management API endpoint.
    pub api_version: u32,
    /// Canonical surface values this daemon's data layer can emit on the
    /// `surface` dimension. Lets host extensions introspect the value
    /// space (instead of hardcoding it against an old daemon) before
    /// rendering a host filter UI. Added in 8.4.2 (#701) alongside the
    /// `api_version` bump to 3.
    pub surfaces: &'static [&'static str],
}

#[derive(serde::Serialize)]
pub struct SyncResponse {
    pub files_synced: usize,
    pub messages_ingested: usize,
    pub warnings: Vec<String>,
    /// Per-provider breakdown of the sync. Omitted from the wire format when
    /// empty so legacy consumers that don't know about the field ignore it
    /// safely; the `budi db import` CLI uses it to render the
    /// post-import per-agent summary table (#440).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub per_provider: Vec<budi_core::analytics::ProviderSyncStats>,
}

#[derive(serde::Serialize)]
pub struct SyncStatusResponse {
    pub syncing: bool,
    pub last_sync_completed_at: Option<String>,
    pub newest_data_at: Option<String>,
    pub ingest_backlog: u64,
    pub ingest_ready: u64,
    pub ingest_failed: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_synced: Option<String>,
    /// Live per-agent progress while `syncing == true`. Set to `None` between
    /// syncs so `/sync/status` returns a minimal payload on the hot path
    /// (statusline + doctor polls hit this route too). Backs `budi db import`
    /// per-agent progress output (#440).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<budi_core::analytics::SyncProgress>,
}

#[derive(Debug, Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IntegrationInstallComponent {
    ClaudeCodeHooks,
    ClaudeCodeMcp,
    ClaudeCodeOtel,
    ClaudeCodeStatusline,
    CursorHooks,
    CursorExtension,
    Starship,
}

impl IntegrationInstallComponent {
    fn as_cli_arg(self) -> &'static str {
        match self {
            Self::ClaudeCodeHooks => "claude-code-hooks",
            Self::ClaudeCodeMcp => "claude-code-mcp",
            Self::ClaudeCodeOtel => "claude-code-otel",
            Self::ClaudeCodeStatusline => "claude-code-statusline",
            Self::CursorHooks => "cursor-hooks",
            Self::CursorExtension => "cursor-extension",
            Self::Starship => "starship",
        }
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct InstallIntegrationsRequest {
    #[serde(default)]
    pub components: Vec<IntegrationInstallComponent>,
}

#[derive(Debug, serde::Serialize)]
pub struct InstallIntegrationsResponse {
    pub ok: bool,
    pub command: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub stdout: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub stderr: String,
}

struct BusyFlagGuard {
    flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    progress: Option<std::sync::Arc<std::sync::Mutex<Option<budi_core::analytics::SyncProgress>>>>,
}

impl BusyFlagGuard {
    fn new(flag: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
        Self {
            flag,
            progress: None,
        }
    }

    fn with_progress(
        flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
        progress: std::sync::Arc<std::sync::Mutex<Option<budi_core::analytics::SyncProgress>>>,
    ) -> Self {
        Self {
            flag,
            progress: Some(progress),
        }
    }
}

impl Drop for BusyFlagGuard {
    fn drop(&mut self) {
        self.flag.store(false, std::sync::atomic::Ordering::SeqCst);
        if let Some(slot) = self.progress.as_ref()
            && let Ok(mut guard) = slot.lock()
        {
            *guard = None;
        }
    }
}

fn resolve_budi_binary() -> PathBuf {
    let exe_name = if cfg!(windows) { "budi.exe" } else { "budi" };
    if let Ok(current) = std::env::current_exe()
        && let Some(dir) = current.parent()
    {
        let candidate = dir.join(exe_name);
        if candidate.is_file() {
            return candidate;
        }
    }
    PathBuf::from(exe_name)
}

fn shell_quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/' | b':' | b'=')
        })
    {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', "'\\''"))
}

const CURSOR_EXTENSION_ID: &str = "siropkin.budi";

fn cursor_cli_candidates() -> &'static [&'static str] {
    #[cfg(target_os = "macos")]
    {
        &[
            "cursor",
            "/usr/local/bin/cursor",
            "/opt/homebrew/bin/cursor",
        ]
    }
    #[cfg(not(target_os = "macos"))]
    {
        &["cursor"]
    }
}

fn output_has_cursor_extension(output: &str) -> bool {
    output.lines().any(|line| {
        let normalized = line.trim().split('@').next().unwrap_or("").trim();
        normalized.eq_ignore_ascii_case(CURSOR_EXTENSION_ID)
    })
}

fn manifest_has_cursor_extension(raw: &str) -> bool {
    let manifest: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return false,
    };
    manifest.as_array().is_some_and(|entries| {
        entries.iter().any(|entry| {
            entry
                .get("identifier")
                .and_then(|id| id.get("id"))
                .and_then(|id| id.as_str())
                .is_some_and(|id| id.eq_ignore_ascii_case(CURSOR_EXTENSION_ID))
        })
    })
}

fn cursor_extension_installed_via_cli() -> Option<bool> {
    for candidate in cursor_cli_candidates() {
        let Ok(output) = std::process::Command::new(candidate)
            .arg("--list-extensions")
            .output()
        else {
            continue;
        };
        if output.status.success() {
            let out = String::from_utf8_lossy(&output.stdout);
            return Some(output_has_cursor_extension(&out));
        }
    }
    None
}

fn cursor_extension_installed_via_filesystem(home: &str) -> bool {
    let extensions_dir = Path::new(home).join(".cursor").join("extensions");
    if let Ok(entries) = std::fs::read_dir(&extensions_dir) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                let lower = name.to_ascii_lowercase();
                if lower == CURSOR_EXTENSION_ID
                    || lower.starts_with(&format!("{CURSOR_EXTENSION_ID}-"))
                {
                    return true;
                }
            }
        }
    }

    let manifest_path = extensions_dir.join("extensions.json");
    std::fs::read_to_string(manifest_path)
        .ok()
        .is_some_and(|raw| manifest_has_cursor_extension(&raw))
}

fn is_cursor_extension_installed(home: &str) -> bool {
    cursor_extension_installed_via_cli().unwrap_or(false)
        || cursor_extension_installed_via_filesystem(home)
}

pub async fn favicon() -> impl IntoResponse {
    let svg = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 100'><text y='.9em' font-size='90'>&#x1f4ca;</text></svg>";
    (
        [
            (header::CONTENT_TYPE, "image/svg+xml; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        svg,
    )
}

/// Current daemon management API version.  Bump when a breaking change is
/// made to any management API endpoint consumed by budi-cursor or the CLI.
///
/// 8.4.2 (#692) — `session_msg_cost` in `/analytics/statusline` is now in
/// dollars (was cents). Host extensions that compiled against the cents
/// contract render 100× too small until they bump their `MIN_API_VERSION`,
/// so the `/health` advertisement bumps in the same PR.
///
/// 8.4.2 (#701) — every message/session row now carries a `surface`
/// dimension (`vscode`, `cursor`, `jetbrains`, `terminal`, `unknown`).
/// Sibling host extensions (budi-cursor, the future budi-jetbrains) gate
/// their host-scoped filter UI on this `api_version` advertisement so
/// they can fall back to the all-rows view when talking to an older
/// daemon. Sibling ticket adds the HTTP + CLI filter; this PR ships the
/// data layer and the `surface` field shows up in returned rows.
pub const API_VERSION: u32 = 3;

/// Canonical surface values advertised on `/health` (#701). Mirrors
/// `budi_core::surface`'s `VSCODE` / `CURSOR` / `JETBRAINS` / `TERMINAL`
/// / `UNKNOWN` constants; we materialize the slice at this seam rather
/// than re-exporting an array from `budi_core` so the daemon's wire
/// format stays a deliberate snapshot rather than a transitive view.
const SURFACES: &[&str] = &["vscode", "cursor", "jetbrains", "terminal", "unknown"];

pub async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        ok: true,
        version: env!("CARGO_PKG_VERSION"),
        api_version: API_VERSION,
        surfaces: SURFACES,
    })
}

/// Query parameters for `GET /health/sources` (#735).
///
/// `?surface=<id>` narrows the response to a single surface — the
/// filtered shape the JetBrains plugin's "Detected sources" row
/// consumes today (siropkin/budi-jetbrains#36). Omitting the param
/// returns every surface in the grouped shape; the plugin doesn't
/// consume the unfiltered form yet, but the contract is documented
/// here so future hosts can rely on it.
#[derive(Debug, serde::Deserialize)]
pub struct HealthSourcesParams {
    pub surface: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct HealthSourcesByGroup {
    pub surface: String,
    pub paths: Vec<String>,
}

#[derive(Debug, serde::Serialize)]
#[serde(untagged)]
pub enum HealthSourcesResponse {
    Filtered { surface: String, paths: Vec<String> },
    Grouped { surfaces: Vec<HealthSourcesByGroup> },
}

/// `GET /health/sources?surface=<id>` (#735).
///
/// Returns the on-disk paths the daemon's tailer is currently watching,
/// grouped by surface (`vscode` / `cursor` / `jetbrains` / `terminal` /
/// `unknown`). Source of truth is each enabled provider's
/// [`budi_core::provider::Provider::watch_roots`] — the same call the
/// tailer's reconcile loop runs every backstop tick. We deliberately
/// re-query on each request: `watch_roots` is filesystem-sensitive
/// (cheap `is_dir`/`exists` checks, no JSONL parse), and reflecting
/// directories that have materialized since the last tailer tick is
/// what makes this endpoint "live" from a host extension's point of
/// view.
///
/// Surface inference mirrors the ingest path: providers that bind to a
/// single host get [`budi_core::surface::default_for_provider`], and
/// `copilot_chat` (whose watch roots span VS Code, Cursor, and
/// JetBrains storage) gets [`budi_core::surface::infer_copilot_chat_surface`]
/// per path so each root is bucketed correctly.
pub async fn health_sources(
    axum::extract::Query(params): axum::extract::Query<HealthSourcesParams>,
) -> Result<Json<HealthSourcesResponse>, (StatusCode, Json<serde_json::Value>)> {
    let result = tokio::task::spawn_blocking(move || collect_health_sources(params.surface))
        .await
        .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?;
    Ok(Json(result))
}

fn surface_for_path(provider_name: &str, path: &Path) -> &'static str {
    if provider_name == "copilot_chat" {
        budi_core::surface::infer_copilot_chat_surface(path)
    } else {
        budi_core::surface::default_for_provider(provider_name)
    }
}

/// Collect tailed paths from every enabled provider, mirroring the
/// snapshot logic the tailer uses at boot
/// (`workers::tailer::run`). Broken out as a pure function (no
/// `axum::Json`, no env coupling) so the response shape can be
/// unit-tested directly.
fn collect_health_sources(surface_filter: Option<String>) -> HealthSourcesResponse {
    let agents_config = budi_core::config::load_agents_config();
    let providers: Vec<Box<dyn budi_core::provider::Provider>> = match &agents_config {
        Some(cfg) => budi_core::provider::all_providers()
            .into_iter()
            .filter(|p| cfg.is_agent_enabled(p.name()))
            .collect(),
        None => budi_core::provider::all_providers(),
    };
    let routes: Vec<(String, std::path::PathBuf)> = providers
        .iter()
        .flat_map(|p| {
            let name = p.name().to_string();
            p.watch_roots()
                .into_iter()
                .map(move |root| (name.clone(), root))
        })
        .collect();

    let normalized_filter = surface_filter
        .as_deref()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty());

    if let Some(filter) = normalized_filter {
        let mut paths: Vec<String> = routes
            .into_iter()
            .filter(|(name, path)| surface_for_path(name, path) == filter)
            .map(|(_, path)| path.to_string_lossy().to_string())
            .collect();
        paths.sort();
        paths.dedup();
        return HealthSourcesResponse::Filtered {
            surface: filter,
            paths,
        };
    }

    let mut by_surface: std::collections::BTreeMap<&'static str, Vec<String>> =
        std::collections::BTreeMap::new();
    for (name, path) in routes {
        let surface = surface_for_path(&name, &path);
        by_surface
            .entry(surface)
            .or_default()
            .push(path.to_string_lossy().to_string());
    }
    let surfaces = by_surface
        .into_iter()
        .map(|(surface, mut paths)| {
            paths.sort();
            paths.dedup();
            HealthSourcesByGroup {
                surface: surface.to_string(),
                paths,
            }
        })
        .collect();
    HealthSourcesResponse::Grouped { surfaces }
}

pub async fn health_check_update()
-> Result<Json<super::analytics::CheckUpdateResponse>, (StatusCode, Json<serde_json::Value>)> {
    use super::analytics::CheckUpdateResponse;

    let result = tokio::task::spawn_blocking(|| -> CheckUpdateResponse {
        let current = env!("CARGO_PKG_VERSION").to_string();
        let client = match reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
        {
            Ok(client) => client,
            Err(e) => {
                return CheckUpdateResponse {
                    current,
                    latest: None,
                    up_to_date: None,
                    error: Some(format!("Could not create HTTP client: {e}")),
                };
            }
        };

        let resp = match client
            .get("https://api.github.com/repos/siropkin/budi/releases/latest")
            .header("User-Agent", format!("budi/{current}"))
            .send()
        {
            Ok(resp) => resp,
            Err(e) => {
                return CheckUpdateResponse {
                    current,
                    latest: None,
                    up_to_date: None,
                    error: Some(format!("Could not reach GitHub API: {e}")),
                };
            }
        };

        if !resp.status().is_success() {
            return CheckUpdateResponse {
                current,
                latest: None,
                up_to_date: None,
                error: Some(format!("GitHub API returned {}", resp.status())),
            };
        }

        let release: serde_json::Value = match resp.json() {
            Ok(release) => release,
            Err(e) => {
                return CheckUpdateResponse {
                    current,
                    latest: None,
                    up_to_date: None,
                    error: Some(format!("Could not parse GitHub response: {e}")),
                };
            }
        };

        let latest_tag = match budi_core::update::parse_and_normalize_release_tag(&release) {
            Ok(tag) => tag,
            Err(e) => {
                return CheckUpdateResponse {
                    current,
                    latest: None,
                    up_to_date: None,
                    error: Some(e.to_string()),
                };
            }
        };
        let latest = budi_core::update::version_from_tag(&latest_tag);
        let up_to_date = latest == current;
        CheckUpdateResponse {
            current,
            latest: Some(latest),
            up_to_date: Some(up_to_date),
            error: None,
        }
    })
    .await
    .map_err(|e| super::internal_error(anyhow::anyhow!("{e}")))?;
    Ok(Json(result))
}

pub async fn admin_install_integrations(
    State(state): State<AppState>,
    Json(req): Json<InstallIntegrationsRequest>,
) -> Result<Json<InstallIntegrationsResponse>, (StatusCode, Json<serde_json::Value>)> {
    if req.components.is_empty() {
        return Err(super::bad_request("components must not be empty"));
    }

    if state
        .integrations_installing
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .is_err()
    {
        return Err((
            StatusCode::CONFLICT,
            Json(
                json!({ "ok": false, "error": "another integration update is already in progress" }),
            ),
        ));
    }
    let _busy = BusyFlagGuard::new(state.integrations_installing.clone());

    let budi_bin = resolve_budi_binary();
    let mut args: Vec<String> = vec![
        "integrations".to_string(),
        "install".to_string(),
        "--yes".to_string(),
    ];
    for component in req.components {
        args.push("--with".to_string());
        args.push(component.as_cli_arg().to_string());
    }
    let budi_bin_for_run = budi_bin.clone();
    let args_for_run = args.clone();
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new(&budi_bin_for_run)
            .args(&args_for_run)
            .output()
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("failed to run integrations task: {e}")))?
    .map_err(|e| {
        internal_error(anyhow::anyhow!(
            "failed to run budi integrations install: {e}"
        ))
    })?;

    let command = std::iter::once(budi_bin.to_string_lossy().to_string())
        .chain(args.iter().cloned())
        .map(|part| shell_quote(&part))
        .collect::<Vec<_>>()
        .join(" ");
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    if !output.status.success() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "ok": false,
                "error": format!("Integration install command failed ({})", output.status),
                "command": command,
                "stdout": stdout,
                "stderr": stderr
            })),
        ));
    }

    Ok(Json(InstallIntegrationsResponse {
        ok: true,
        command,
        stdout,
        stderr,
    }))
}

pub async fn health_integrations()
-> Result<Json<super::analytics::IntegrationsResponse>, (StatusCode, Json<serde_json::Value>)> {
    use super::analytics::{DatabaseStats, IntegrationPaths, IntegrationsResponse};

    let result = tokio::task::spawn_blocking(|| -> IntegrationsResponse {
        let home = budi_core::config::home_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();

        // Check Claude Code settings
        let claude_path = format!("{home}/.claude/settings.json");
        let claude_settings: Option<serde_json::Value> = std::fs::read_to_string(&claude_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok());

        let statusline_installed = claude_settings
            .as_ref()
            .and_then(|s| s.get("statusLine"))
            .and_then(|sl| sl.get("command"))
            .and_then(|c| c.as_str())
            .map(|c| c.contains("budi statusline") || c.contains("budi_out=$(budi"))
            .unwrap_or(false);

        // Cursor extension
        let cursor_extension = is_cursor_extension_installed(&home);

        // DB stats + paths
        let db_path = budi_core::analytics::db_path().ok();
        let db_path_str = db_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let db_stats = db_path
            .and_then(|p| {
                let size_mb = std::fs::metadata(&p)
                    .ok()
                    .map(|m| m.len() as f64 / 1_048_576.0);
                let conn = budi_core::analytics::open_db(&p).ok()?;
                let msg_count: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM messages WHERE role = 'assistant'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                let first_record: Option<String> = conn
                    .query_row(
                        "SELECT MIN(timestamp) FROM messages WHERE role = 'assistant'",
                        [],
                        |r| r.get(0),
                    )
                    .ok()
                    .flatten();
                Some(DatabaseStats {
                    size_mb: (size_mb.unwrap_or(0.0) * 10.0).round() / 10.0,
                    records: msg_count,
                    first_record,
                })
            })
            .unwrap_or(DatabaseStats {
                size_mb: 0.0,
                records: 0,
                first_record: None,
            });

        let config_dir = budi_core::config::budi_home_dir()
            .ok()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();

        IntegrationsResponse {
            cursor_extension,
            statusline: statusline_installed,
            database: db_stats,
            paths: IntegrationPaths {
                database: db_path_str,
                config: config_dir,
                claude_settings: claude_path,
            },
        }
    })
    .await
    .map_err(|e| super::internal_error(anyhow::anyhow!("{e}")))?;

    Ok(Json(result))
}

pub async fn sync_status(State(state): State<AppState>) -> Json<SyncStatusResponse> {
    let syncing = state.syncing.load(std::sync::atomic::Ordering::Acquire);
    // Only return a progress snapshot when we actually hold the busy flag;
    // a leftover `Some(..)` from a previous run (cleared by `BusyFlagGuard`
    // on Drop, but timing matters for the `syncing = false` → `None`
    // transition) should not leak into the next poll.
    let progress = if syncing {
        state.sync_progress.lock().ok().and_then(|g| g.clone())
    } else {
        None
    };
    let status = tokio::task::spawn_blocking(|| {
        let db_path = budi_core::analytics::db_path().ok()?;
        let conn = budi_core::analytics::open_db(&db_path).ok()?;
        Some((
            budi_core::analytics::last_sync_completed_at(&conn)
                .ok()
                .flatten(),
            budi_core::analytics::newest_ingested_data_at(&conn)
                .ok()
                .flatten(),
        ))
    })
    .await
    .ok()
    .flatten();
    let (last_sync_completed_at, newest_data_at) = status.unwrap_or((None, None));
    Json(SyncStatusResponse {
        syncing,
        last_sync_completed_at: last_sync_completed_at.clone(),
        newest_data_at,
        ingest_backlog: 0,
        ingest_ready: 0,
        ingest_failed: 0,
        last_synced: last_sync_completed_at,
        progress,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        HealthSourcesResponse, manifest_has_cursor_extension, output_has_cursor_extension,
        surface_for_path,
    };
    use std::path::Path;

    #[test]
    fn cursor_extension_cli_output_detection_handles_versioned_lines() {
        let output = "ms-python.python\nsiropkin.budi@0.1.0\n";
        assert!(output_has_cursor_extension(output));
        assert!(!output_has_cursor_extension("ms-python.python\n"));
    }

    #[test]
    fn cursor_extension_manifest_detection_finds_identifier() {
        let raw = r#"[
          {"identifier":{"id":"ms-python.python"},"version":"1.0.0"},
          {"identifier":{"id":"siropkin.budi"},"version":"0.1.0"}
        ]"#;
        assert!(manifest_has_cursor_extension(raw));
        assert!(!manifest_has_cursor_extension(
            r#"[{"identifier":{"id":"ms-python.python"}}]"#
        ));
    }

    #[test]
    fn surface_for_path_maps_single_host_providers_via_default() {
        // Path is irrelevant for non-copilot_chat providers — they bind to
        // a single host so `default_for_provider` decides.
        let p = Path::new("/anywhere/whatever.jsonl");
        assert_eq!(surface_for_path("claude_code", p), "terminal");
        assert_eq!(surface_for_path("cursor", p), "cursor");
        assert_eq!(surface_for_path("codex", p), "terminal");
        assert_eq!(surface_for_path("copilot_cli", p), "terminal");
        assert_eq!(surface_for_path("jetbrains_ai_assistant", p), "jetbrains");
    }

    #[test]
    fn surface_for_path_classifies_copilot_chat_per_path() {
        // copilot_chat's watch roots span three hosts; surface comes from
        // the path itself per ADR-0092 §2.1.
        let cursor = Path::new("/Users/u/Library/Application Support/Cursor/User/workspaceStorage");
        let vscode = Path::new("/Users/u/Library/Application Support/Code/User/workspaceStorage");
        let jetbrains = Path::new("/Users/u/Library/Application Support/JetBrains/IdeaIC2026.1");
        assert_eq!(surface_for_path("copilot_chat", cursor), "cursor");
        assert_eq!(surface_for_path("copilot_chat", vscode), "vscode");
        assert_eq!(surface_for_path("copilot_chat", jetbrains), "jetbrains");
    }

    /// #758: the JetBrains-side Copilot watch roots live under
    /// `~/.config/github-copilot/<ide-slug>/<session-type>/`. Before the
    /// fix the aggregator routed them to `surface=unknown`, leaving the
    /// JetBrains widget's `?surface=jetbrains` query empty even when the
    /// parser would have emitted rows.
    #[test]
    fn surface_for_path_routes_github_copilot_to_jetbrains() {
        for slug in ["iu", "ic", "ws"] {
            for session_type in [
                "chat-sessions",
                "chat-agent-sessions",
                "chat-edit-sessions",
                "bg-agent-sessions",
            ] {
                let p = std::path::PathBuf::from(format!(
                    "/Users/u/.config/github-copilot/{slug}/{session_type}"
                ));
                assert_eq!(
                    surface_for_path("copilot_chat", &p),
                    "jetbrains",
                    "expected jetbrains surface for {}/{}",
                    slug,
                    session_type
                );
            }
        }
    }

    #[test]
    fn collect_health_sources_filtered_returns_only_matching_surface() {
        let filtered = super::collect_health_sources(Some("jetbrains".to_string()));
        match filtered {
            HealthSourcesResponse::Filtered { surface, paths } => {
                assert_eq!(surface, "jetbrains");
                // Every returned path must classify as jetbrains.
                for path in &paths {
                    let p = Path::new(path);
                    // The path's owning provider can be either
                    // jetbrains_ai_assistant or copilot_chat; both produce
                    // the jetbrains surface.
                    let jb_ai = surface_for_path("jetbrains_ai_assistant", p) == "jetbrains";
                    let copilot = surface_for_path("copilot_chat", p) == "jetbrains";
                    assert!(
                        jb_ai || copilot,
                        "path {path} does not map to jetbrains surface"
                    );
                }
            }
            HealthSourcesResponse::Grouped { .. } => panic!("expected filtered shape"),
        }
    }

    #[test]
    fn collect_health_sources_blank_surface_is_treated_as_unfiltered() {
        // `?surface=` (empty value) should fall through to the grouped
        // shape, not 404 / return nothing — mirrors the issue contract.
        let resp = super::collect_health_sources(Some("   ".to_string()));
        match resp {
            HealthSourcesResponse::Grouped { .. } => {}
            HealthSourcesResponse::Filtered { .. } => {
                panic!("blank surface filter should fall through to grouped shape")
            }
        }
    }

    #[test]
    fn collect_health_sources_grouped_omits_unknown_surfaces() {
        // Grouped shape is allowed to include any of the canonical
        // surfaces; the only invariant we can assert without mocking the
        // filesystem is that each entry is a valid canonical surface and
        // its `paths` is sorted+deduped.
        let resp = super::collect_health_sources(None);
        match resp {
            HealthSourcesResponse::Grouped { surfaces } => {
                let canonical = ["vscode", "cursor", "jetbrains", "terminal", "unknown"];
                for group in &surfaces {
                    assert!(
                        canonical.contains(&group.surface.as_str()),
                        "non-canonical surface returned: {}",
                        group.surface
                    );
                    let mut sorted = group.paths.clone();
                    sorted.sort();
                    sorted.dedup();
                    assert_eq!(
                        sorted, group.paths,
                        "paths must be sorted + deduped for surface {}",
                        group.surface
                    );
                }
            }
            HealthSourcesResponse::Filtered { .. } => panic!("expected grouped shape"),
        }
    }

    // ─── #817 handler coverage tests ────────────────────────────────────
    //
    // Baseline coverage on `routes/hooks.rs` was 25.5% on the 8.5.2
    // baseline (#804) — `surface_for_path` + `collect_health_sources`
    // were well covered, but every handler body (the proxy hot path) was
    // 0%. These tests exercise each handler directly under a tempdir
    // HOME, plus a small set of full-router cases for the
    // `require_local_host` middleware and the axum extractor 400 paths.

    use super::{
        API_VERSION, HealthSourcesParams, InstallIntegrationsRequest, SyncParams, SyncResponse,
        admin_install_integrations, analytics_history, analytics_sync, analytics_sync_reset,
        favicon, health, health_integrations, health_sources, sync_status,
    };
    use crate::AppState;
    use crate::routes::HostAllowlist;
    use axum::Json;
    use axum::Router;
    use axum::body::Body;
    use axum::extract::{ConnectInfo, State};
    use axum::http::{Method, Request, StatusCode, header};
    use axum::middleware::from_fn_with_state;
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use http_body_util::BodyExt;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tower::ServiceExt;

    /// Process-global `HOME` / `BUDI_HOME` are mutated to point at a
    /// throw-away tempdir below. `cargo test` runs tests in parallel by
    /// default, so without this mutex two tests would observe each
    /// other's env writes between `set_var` and `remove_var`. Mirrors
    /// the pattern used in `routes::pricing` / `routes::analytics` tests.
    static HOME_MUTEX: Mutex<()> = Mutex::new(());

    struct HomeGuard {
        prev_home: Option<String>,
        prev_budi_home: Option<String>,
        _tmp: tempfile::TempDir,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl HomeGuard {
        fn new() -> Self {
            let lock = HOME_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            let tmp = tempfile::tempdir().expect("tempdir for HomeGuard");
            let prev_home = std::env::var("HOME").ok();
            let prev_budi_home = std::env::var("BUDI_HOME").ok();
            // SAFETY: serialized by HOME_MUTEX above; no other thread
            // reads HOME / BUDI_HOME for the duration of the guard.
            unsafe { std::env::set_var("HOME", tmp.path()) };
            unsafe { std::env::remove_var("BUDI_HOME") };
            Self {
                prev_home,
                prev_budi_home,
                _tmp: tmp,
                _lock: lock,
            }
        }

        /// Materialize the analytics DB at the redirected home so handler
        /// success paths don't fall over on `open_db` for a missing schema.
        fn init_db(&self) {
            let db_path = budi_core::analytics::db_path().expect("db_path");
            budi_core::analytics::open_db_with_migration(&db_path).expect("migrate empty db");
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match &self.prev_home {
                Some(h) => unsafe { std::env::set_var("HOME", h) },
                None => unsafe { std::env::remove_var("HOME") },
            }
            match &self.prev_budi_home {
                Some(h) => unsafe { std::env::set_var("BUDI_HOME", h) },
                None => unsafe { std::env::remove_var("BUDI_HOME") },
            }
        }
    }

    fn fresh_app_state() -> AppState {
        AppState {
            syncing: Arc::new(AtomicBool::new(false)),
            integrations_installing: Arc::new(AtomicBool::new(false)),
            cloud_syncing: Arc::new(AtomicBool::new(false)),
            sync_progress: Arc::new(Mutex::new(None)),
        }
    }

    // ─── Direct-handler tests ───────────────────────────────────────────

    #[tokio::test]
    async fn favicon_returns_svg_with_cache_headers() {
        let resp = favicon().await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            ct.starts_with("image/svg+xml"),
            "expected svg content-type, got {ct}"
        );
        // Cache-Control is set to make doctor / browser probes cheap.
        assert!(resp.headers().get(header::CACHE_CONTROL).is_some());
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(text.starts_with("<svg"), "expected svg body, got {text}");
    }

    #[tokio::test]
    async fn health_returns_advertised_api_version_and_surfaces() {
        let Json(body) = health().await;
        assert!(body.ok);
        assert_eq!(body.api_version, API_VERSION);
        // The wire contract pins exactly these five canonical surfaces;
        // host extensions key their host-filter UI off this slice.
        assert_eq!(
            body.surfaces,
            ["vscode", "cursor", "jetbrains", "terminal", "unknown"]
        );
        // `version` is whatever Cargo baked in; just check it's non-empty.
        assert!(!body.version.is_empty());
    }

    #[tokio::test]
    async fn health_sources_handler_returns_filtered_shape_for_unknown_surface() {
        // The handler doesn't reject unknown surface filters at the
        // extractor — it normalizes the input and returns the empty
        // Filtered shape, which is what host extensions expect when
        // probing for a surface their daemon's providers don't expose.
        let params = HealthSourcesParams {
            surface: Some("definitely-not-a-real-surface".to_string()),
        };
        let Json(resp) = health_sources(axum::extract::Query(params))
            .await
            .expect("handler must not error for an unknown surface filter");
        match resp {
            HealthSourcesResponse::Filtered { surface, paths } => {
                assert_eq!(surface, "definitely-not-a-real-surface");
                assert!(
                    paths.is_empty(),
                    "no provider should publish a path for an unknown surface"
                );
            }
            HealthSourcesResponse::Grouped { .. } => panic!("expected filtered shape"),
        }
    }

    #[tokio::test]
    async fn health_sources_handler_returns_grouped_shape_when_param_absent() {
        let Json(resp) =
            health_sources(axum::extract::Query(HealthSourcesParams { surface: None }))
                .await
                .expect("handler must succeed without surface filter");
        match resp {
            HealthSourcesResponse::Grouped { .. } => {}
            HealthSourcesResponse::Filtered { .. } => panic!("expected grouped shape"),
        }
    }

    #[tokio::test]
    async fn health_integrations_returns_paths_shape_against_empty_home() {
        let _guard = HomeGuard::new();
        let Json(body) = health_integrations()
            .await
            .expect("integrations handler must not error on empty HOME");
        // Cursor extension cannot be installed (no fixture under HOME),
        // statusline cannot be installed (no claude settings file).
        assert!(!body.cursor_extension);
        assert!(!body.statusline);
        // Database stats fall through to the zeroed default when the
        // DB hasn't been materialized yet.
        assert_eq!(body.database.records, 0);
        // Paths must always be populated — the dashboard renders the
        // settings.json / database / config paths regardless of state.
        assert!(body.paths.database.ends_with(".sqlite") || !body.paths.database.is_empty());
        assert!(!body.paths.config.is_empty());
        assert!(
            body.paths
                .claude_settings
                .ends_with(".claude/settings.json")
        );
    }

    #[tokio::test]
    async fn sync_status_returns_idle_shape_when_no_sync_in_flight() {
        let _guard = HomeGuard::new();
        let state = fresh_app_state();
        let Json(resp) = sync_status(State(state)).await;
        assert!(!resp.syncing);
        // Progress slot is only populated while `syncing=true`. With a
        // freshly-built state on a non-syncing daemon, it must be None
        // even though the underlying Mutex holds an Option.
        assert!(resp.progress.is_none());
        // The ingest-queue fields stay at zero on the read path; this
        // pin matches the pre-#603 wire shape budi-cursor still relies
        // on for its statusline render.
        assert_eq!(resp.ingest_backlog, 0);
        assert_eq!(resp.ingest_ready, 0);
        assert_eq!(resp.ingest_failed, 0);
    }

    #[tokio::test]
    async fn sync_status_reports_syncing_true_when_busy_flag_is_set() {
        let _guard = HomeGuard::new();
        let state = fresh_app_state();
        state.syncing.store(true, Ordering::SeqCst);
        let Json(resp) = sync_status(State(state)).await;
        assert!(resp.syncing);
    }

    #[tokio::test]
    async fn analytics_sync_returns_409_when_a_sync_is_already_running() {
        // Pre-set the busy flag to simulate a concurrent in-flight sync.
        // The handler must short-circuit with 409 before touching the
        // DB, so we don't even need to init it.
        let _guard = HomeGuard::new();
        let state = fresh_app_state();
        state.syncing.store(true, Ordering::SeqCst);
        let err = match analytics_sync(State(state), None).await {
            Err(e) => e,
            Ok(_) => panic!("a second concurrent sync must return 409"),
        };
        assert_eq!(err.0, StatusCode::CONFLICT);
        assert_eq!(err.1.0["ok"], false);
    }

    #[tokio::test]
    async fn analytics_sync_succeeds_with_migrate_true_on_fresh_tempdir() {
        // `migrate=true` lets the handler bootstrap the schema instead
        // of returning 503 needs-migration. Against a tempdir HOME with
        // no JSONLs, the sync completes cleanly with zero rows.
        let _guard = HomeGuard::new();
        let state = fresh_app_state();
        let Json(SyncResponse {
            files_synced,
            messages_ingested,
            warnings: _warnings,
            per_provider: _per_provider,
        }) = analytics_sync(
            State(state.clone()),
            Some(Json(SyncParams { migrate: true })),
        )
        .await
        .expect("sync handler must succeed against a fresh tempdir HOME");
        assert_eq!(files_synced, 0);
        assert_eq!(messages_ingested, 0);
        // BusyFlagGuard must clear the busy flag on the happy path so a
        // follow-up sync can run.
        assert!(!state.syncing.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn analytics_sync_returns_503_on_stale_schema_without_migrate() {
        // Pre-create a DB with the schema bootstrap, then bump the
        // binary's expected version above what's on disk. The
        // open-with-migration path is gated by `migrate=false`, so the
        // handler should hit the `SchemaStatus::Stale` branch and
        // return the structured 503 from `schema_unavailable`.
        let guard = HomeGuard::new();
        guard.init_db();
        // Force `user_version` to 0 so the binary's compiled-in target
        // looks newer. Tests share the binary's version, so this is the
        // cleanest way to reach the stale branch without a parallel
        // binary build.
        let db_path = budi_core::analytics::db_path().expect("db_path");
        let conn = rusqlite::Connection::open(&db_path).expect("open db");
        conn.pragma_update(None, "user_version", 0_u32)
            .expect("rewind user_version");
        drop(conn);

        let state = fresh_app_state();
        let err =
            match analytics_sync(State(state), Some(Json(SyncParams { migrate: false }))).await {
                Err(e) => e,
                Ok(_) => panic!("stale schema without migrate must return 503"),
            };
        assert_eq!(err.0, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(err.1.0["needs_migration"], true);
    }

    #[tokio::test]
    async fn analytics_sync_reset_succeeds_on_fresh_tempdir() {
        let _guard = HomeGuard::new();
        let state = fresh_app_state();
        let Json(resp) = analytics_sync_reset(State(state.clone()))
            .await
            .expect("reset handler must succeed against a fresh HOME");
        assert_eq!(resp.files_synced, 0);
        assert_eq!(resp.messages_ingested, 0);
        assert!(!state.syncing.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn analytics_sync_reset_returns_409_when_a_sync_is_already_running() {
        let _guard = HomeGuard::new();
        let state = fresh_app_state();
        state.syncing.store(true, Ordering::SeqCst);
        let err = match analytics_sync_reset(State(state)).await {
            Err(e) => e,
            Ok(_) => panic!("a second concurrent reset must return 409"),
        };
        assert_eq!(err.0, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn analytics_history_succeeds_on_fresh_tempdir() {
        let _guard = HomeGuard::new();
        let state = fresh_app_state();
        let Json(resp) = analytics_history(State(state.clone()))
            .await
            .expect("history handler must succeed against a fresh HOME");
        assert_eq!(resp.files_synced, 0);
        assert_eq!(resp.messages_ingested, 0);
        assert!(!state.syncing.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn analytics_history_returns_409_when_a_sync_is_already_running() {
        let _guard = HomeGuard::new();
        let state = fresh_app_state();
        state.syncing.store(true, Ordering::SeqCst);
        let err = match analytics_history(State(state)).await {
            Err(e) => e,
            Ok(_) => panic!("a second concurrent history pull must return 409"),
        };
        assert_eq!(err.0, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn admin_install_integrations_rejects_empty_components_with_400() {
        let _guard = HomeGuard::new();
        let state = fresh_app_state();
        let err = admin_install_integrations(
            State(state.clone()),
            Json(InstallIntegrationsRequest { components: vec![] }),
        )
        .await
        .expect_err("empty components must 400");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        let msg = err.1.0["error"].as_str().unwrap_or_default();
        assert!(
            msg.contains("components"),
            "error must mention components, got {msg}"
        );
        // The busy flag must remain unset since we 400'd before
        // entering the BusyFlagGuard scope.
        assert!(!state.integrations_installing.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn admin_install_integrations_returns_409_when_another_install_is_in_flight() {
        let _guard = HomeGuard::new();
        let state = fresh_app_state();
        state.integrations_installing.store(true, Ordering::SeqCst);
        let err = admin_install_integrations(
            State(state),
            Json(InstallIntegrationsRequest {
                components: vec![super::IntegrationInstallComponent::Starship],
            }),
        )
        .await
        .expect_err("concurrent install must 409");
        assert_eq!(err.0, StatusCode::CONFLICT);
        let msg = err.1.0["error"].as_str().unwrap_or_default();
        assert!(msg.contains("in progress"), "got {msg}");
    }

    // ─── Full-router middleware tests ───────────────────────────────────
    //
    // Wires the public hooks routes through `require_local_host` so the
    // DNS-rebinding defense (#695) is exercised against this surface
    // specifically, plus malformed-body / malformed-query 400 paths
    // through axum's extractors.

    fn hooks_test_router() -> Router {
        Router::new()
            .route("/health", get(health))
            .route("/health/sources", get(health_sources))
            .route("/sync/status", get(sync_status))
            .route(
                "/admin/integrations/install",
                post(admin_install_integrations),
            )
            .with_state(fresh_app_state())
            .layer(from_fn_with_state(
                HostAllowlist::for_tests(),
                crate::routes::require_local_host,
            ))
    }

    fn loopback_request(
        method: Method,
        uri: &str,
        host: Option<&'static str>,
        body: Body,
    ) -> Request<Body> {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .body(body)
            .unwrap();
        if let Some(h) = host {
            req.headers_mut()
                .insert(header::HOST, axum::http::HeaderValue::from_static(h));
        }
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 54545))));
        req
    }

    #[tokio::test]
    async fn hooks_router_accepts_loopback_host_on_health() {
        let _guard = HomeGuard::new();
        let app = hooks_test_router();
        let req = loopback_request(Method::GET, "/health", Some("127.0.0.1"), Body::empty());
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["api_version"], API_VERSION);
    }

    #[tokio::test]
    async fn hooks_router_rejects_non_local_host_on_health_with_403() {
        // DNS-rebinding scenario: peer IP is loopback (browser dialed
        // 127.0.0.1) but the Host header is an attacker-controlled name.
        let _guard = HomeGuard::new();
        let app = hooks_test_router();
        let req = loopback_request(
            Method::GET,
            "/health",
            Some("attacker.example"),
            Body::empty(),
        );
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "invalid Host header");
    }

    #[tokio::test]
    async fn hooks_router_rejects_missing_host_header_with_403() {
        let _guard = HomeGuard::new();
        let app = hooks_test_router();
        let req = loopback_request(Method::GET, "/sync/status", None, Body::empty());
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn hooks_router_rejects_malformed_install_body_with_4xx() {
        // axum's `Json<T>` extractor returns 400 for syntactically
        // malformed JSON and 422 for valid JSON that fails to
        // deserialize against the target type. Both close the gap
        // before the handler runs — the test covers both branches.
        let _guard = HomeGuard::new();
        let app = hooks_test_router();

        // Branch 1: junk bytes — 400 from the json parser.
        let mut req = loopback_request(
            Method::POST,
            "/admin/integrations/install",
            Some("127.0.0.1"),
            Body::from("not-json-at-all{"),
        );
        req.headers_mut().insert(
            header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/json"),
        );
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // Branch 2: valid JSON but an unknown enum variant — 422 from
        // the typed deserializer (`IntegrationInstallComponent` is a
        // closed enum). Either way, the handler body must not run.
        let mut req = loopback_request(
            Method::POST,
            "/admin/integrations/install",
            Some("127.0.0.1"),
            Body::from(r#"{"components":["not-a-real-component"]}"#),
        );
        req.headers_mut().insert(
            header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/json"),
        );
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn hooks_router_health_sources_filtered_path_via_oneshot() {
        // Drives the `?surface=<id>` path through the router so the
        // axum::Query extractor's parsing is exercised end-to-end.
        let _guard = HomeGuard::new();
        let app = hooks_test_router();
        let req = loopback_request(
            Method::GET,
            "/health/sources?surface=jetbrains",
            Some("127.0.0.1"),
            Body::empty(),
        );
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        // Filtered shape has `surface` + `paths` at the top level
        // (HealthSourcesResponse is `#[serde(untagged)]`).
        assert_eq!(json["surface"], "jetbrains");
        assert!(json["paths"].is_array());
    }
}

#[derive(serde::Deserialize, Default)]
pub struct SyncParams {
    #[serde(default)]
    pub migrate: bool,
}

/// Build a callback that publishes `SyncProgress` snapshots into the
/// shared `AppState.sync_progress` slot so `/sync/status` can surface live
/// per-agent progress during an in-flight `POST /sync/*` call (#440).
///
/// `Mutex::lock` failures are swallowed: a poisoned mutex just means a
/// previous progress publish panicked, and losing a progress tick is
/// strictly preferable to killing the sync itself.
fn progress_publisher(
    slot: std::sync::Arc<std::sync::Mutex<Option<budi_core::analytics::SyncProgress>>>,
) -> impl FnMut(&budi_core::analytics::SyncProgress) + Send + 'static {
    move |progress: &budi_core::analytics::SyncProgress| {
        if let Ok(mut guard) = slot.lock() {
            *guard = Some(progress.clone());
        }
    }
}

pub async fn analytics_sync(
    State(state): State<AppState>,
    body: Option<Json<SyncParams>>,
) -> Result<Json<SyncResponse>, (StatusCode, Json<serde_json::Value>)> {
    let params = body.map(|Json(p)| p).unwrap_or_default();
    if state
        .syncing
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .is_err()
    {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({ "ok": false, "error": "sync already running" })),
        ));
    }
    let flag = state.syncing.clone();
    let progress_slot = state.sync_progress.clone();

    // Result variant carries an explicit "stale schema, migrate=false"
    // signal so we can map it to the structured 503 that #366 mandates
    // instead of the opaque 500 this used to bail with.
    enum SyncOutcome {
        Ok(SyncResponse),
        StaleSchema { current: u32, target: u32 },
    }

    let outcome = tokio::task::spawn_blocking(move || -> anyhow::Result<SyncOutcome> {
        let _busy = BusyFlagGuard::with_progress(flag, progress_slot.clone());
        let db_path = budi_core::analytics::db_path()?;
        let mut conn = if params.migrate {
            budi_core::analytics::open_db_with_migration(&db_path)?
        } else {
            let c = budi_core::analytics::open_db(&db_path)?;
            let current = budi_core::migration::current_version(&c);
            let target = budi_core::migration::SCHEMA_VERSION;
            if current < target {
                return Ok(SyncOutcome::StaleSchema { current, target });
            }
            c
        };
        let report = budi_core::analytics::sync_all_with_progress(
            &mut conn,
            progress_publisher(progress_slot),
        )?;
        Ok(SyncOutcome::Ok(SyncResponse {
            files_synced: report.files_synced,
            messages_ingested: report.messages_ingested,
            warnings: report.warnings,
            per_provider: report.per_provider,
        }))
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    match outcome {
        SyncOutcome::Ok(resp) => Ok(Json(resp)),
        SyncOutcome::StaleSchema { current, target } => Err(schema_unavailable(current, target)),
    }
}

pub async fn analytics_sync_reset(
    State(state): State<AppState>,
) -> Result<Json<SyncResponse>, (StatusCode, Json<serde_json::Value>)> {
    if state
        .syncing
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .is_err()
    {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({ "ok": false, "error": "sync already running" })),
        ));
    }
    let flag = state.syncing.clone();
    let progress_slot = state.sync_progress.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _busy = BusyFlagGuard::with_progress(flag, progress_slot.clone());
        (|| -> anyhow::Result<_> {
            let db_path = budi_core::analytics::db_path()?;
            let mut conn = budi_core::analytics::open_db_with_migration(&db_path)?;
            budi_core::analytics::reset_sync_state(&conn)?;
            let report = budi_core::analytics::sync_history_with_progress(
                &mut conn,
                progress_publisher(progress_slot),
            )?;
            Ok(SyncResponse {
                files_synced: report.files_synced,
                messages_ingested: report.messages_ingested,
                warnings: report.warnings,
                per_provider: report.per_provider,
            })
        })()
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    Ok(Json(result))
}

pub async fn analytics_history(
    State(state): State<AppState>,
) -> Result<Json<SyncResponse>, (StatusCode, Json<serde_json::Value>)> {
    if state
        .syncing
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .is_err()
    {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({ "ok": false, "error": "sync already running" })),
        ));
    }
    let flag = state.syncing.clone();
    let progress_slot = state.sync_progress.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _busy = BusyFlagGuard::with_progress(flag, progress_slot.clone());
        (|| -> anyhow::Result<_> {
            let db_path = budi_core::analytics::db_path()?;
            let mut conn = budi_core::analytics::open_db_with_migration(&db_path)?;
            let report = budi_core::analytics::sync_history_with_progress(
                &mut conn,
                progress_publisher(progress_slot),
            )?;
            Ok(SyncResponse {
                files_synced: report.files_synced,
                messages_ingested: report.messages_ingested,
                warnings: report.warnings,
                per_provider: report.per_provider,
            })
        })()
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    Ok(Json(result))
}
