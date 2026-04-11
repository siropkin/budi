use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::internal_error;
use crate::AppState;

#[derive(serde::Serialize)]
pub struct HealthResponse {
    pub ok: bool,
    pub version: &'static str,
    /// Daemon management API version.  The Cursor extension checks this field
    /// on startup and warns if its expected API version is unsupported.
    /// Bump when a breaking change is made to any management API endpoint.
    pub api_version: u32,
}

#[derive(serde::Serialize)]
pub struct SyncResponse {
    pub files_synced: usize,
    pub messages_ingested: usize,
    pub warnings: Vec<String>,
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

#[derive(Debug, Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IntegrationStatuslinePreset {
    Coach,
    Cost,
    Full,
}

impl IntegrationStatuslinePreset {
    fn as_cli_arg(self) -> &'static str {
        match self {
            Self::Coach => "coach",
            Self::Cost => "cost",
            Self::Full => "full",
        }
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct InstallIntegrationsRequest {
    #[serde(default)]
    pub components: Vec<IntegrationInstallComponent>,
    #[serde(default)]
    pub statusline_preset: Option<IntegrationStatuslinePreset>,
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
}

impl BusyFlagGuard {
    fn new(flag: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
        Self { flag }
    }
}

impl Drop for BusyFlagGuard {
    fn drop(&mut self) {
        self.flag.store(false, std::sync::atomic::Ordering::SeqCst);
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

/// Current daemon management API version.  Bump when a breaking change is
/// made to any management API endpoint consumed by budi-cursor or the CLI.
pub const API_VERSION: u32 = 1;

pub async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        ok: true,
        version: env!("CARGO_PKG_VERSION"),
        api_version: API_VERSION,
    })
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
    if let Some(preset) = req.statusline_preset {
        args.push("--statusline-preset".to_string());
        args.push(preset.as_cli_arg().to_string());
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
    })
}

#[cfg(test)]
mod tests {
    use super::{manifest_has_cursor_extension, output_has_cursor_extension};

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
}

#[derive(serde::Deserialize, Default)]
pub struct SyncParams {
    #[serde(default)]
    pub migrate: bool,
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
    let result = tokio::task::spawn_blocking(move || {
        let _busy = BusyFlagGuard::new(flag);
        (|| -> anyhow::Result<_> {
            let db_path = budi_core::analytics::db_path()?;
            let mut conn = if params.migrate {
                budi_core::analytics::open_db_with_migration(&db_path)?
            } else {
                let c = budi_core::analytics::open_db(&db_path)?;
                if budi_core::migration::needs_migration(&c) {
                    anyhow::bail!(
                        "Database needs migration. Use migrate=true or run `budi migrate`."
                    );
                }
                c
            };
            let (files_synced, messages_ingested, warnings) =
                budi_core::analytics::sync_all(&mut conn)?;
            Ok(SyncResponse {
                files_synced,
                messages_ingested,
                warnings,
            })
        })()
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    Ok(Json(result))
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
    let result = tokio::task::spawn_blocking(move || {
        let _busy = BusyFlagGuard::new(flag);
        (|| -> anyhow::Result<_> {
            let db_path = budi_core::analytics::db_path()?;
            let mut conn = budi_core::analytics::open_db_with_migration(&db_path)?;
            budi_core::analytics::reset_sync_state(&conn)?;
            let (files_synced, messages_ingested, warnings) =
                budi_core::analytics::sync_history(&mut conn)?;
            Ok(SyncResponse {
                files_synced,
                messages_ingested,
                warnings,
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
    let result = tokio::task::spawn_blocking(move || {
        let _busy = BusyFlagGuard::new(flag);
        (|| -> anyhow::Result<_> {
            let db_path = budi_core::analytics::db_path()?;
            let mut conn = budi_core::analytics::open_db_with_migration(&db_path)?;
            let (files_synced, messages_ingested, warnings) =
                budi_core::analytics::sync_history(&mut conn)?;
            Ok(SyncResponse {
                files_synced,
                messages_ingested,
                warnings,
            })
        })()
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    Ok(Json(result))
}
