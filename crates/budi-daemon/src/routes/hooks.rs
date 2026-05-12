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
