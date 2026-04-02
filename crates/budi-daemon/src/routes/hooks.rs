use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;

use super::internal_error;
use crate::AppState;

#[derive(serde::Serialize)]
pub struct HealthResponse {
    pub ok: bool,
    pub version: &'static str,
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

pub async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        ok: true,
        version: env!("CARGO_PKG_VERSION"),
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
        const CC_HOOK_EVENTS: &[&str] = &[
            "SessionStart",
            "SessionEnd",
            "PostToolUse",
            "SubagentStop",
            "PreCompact",
            "Stop",
            "UserPromptSubmit",
        ];
        const CURSOR_HOOK_EVENTS: &[&str] = &[
            "sessionStart",
            "sessionEnd",
            "postToolUse",
            "subagentStop",
            "preCompact",
            "stop",
            "afterFileEdit",
            "beforeSubmitPrompt",
        ];

        let is_budi_cc_hook_entry = |entry: &serde_json::Value| -> bool {
            entry
                .get("hooks")
                .and_then(|h| h.as_array())
                .map(|hooks| {
                    hooks.iter().any(|h| {
                        h.get("command")
                            .and_then(|c| c.as_str())
                            .is_some_and(|cmd| {
                                let trimmed = cmd.trim();
                                trimmed == "budi hook" || trimmed.starts_with("budi hook ")
                            })
                    })
                })
                .unwrap_or(false)
        };
        let is_budi_cursor_hook_entry = |entry: &serde_json::Value| -> bool {
            entry
                .get("command")
                .and_then(|c| c.as_str())
                .is_some_and(|cmd| {
                    let trimmed = cmd.trim();
                    trimmed == "budi hook" || trimmed.starts_with("budi hook ")
                })
        };

        let home = budi_core::config::home_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();

        // Check Claude Code settings
        let claude_path = format!("{home}/.claude/settings.json");
        let claude_settings: Option<serde_json::Value> = std::fs::read_to_string(&claude_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok());

        let hooks_installed = claude_settings
            .as_ref()
            .and_then(|s| s.get("hooks").and_then(|h| h.as_object()))
            .is_some_and(|hooks| {
                CC_HOOK_EVENTS.iter().all(|event| {
                    hooks
                        .get(*event)
                        .and_then(|v| v.as_array())
                        .map(|arr| arr.iter().any(is_budi_cc_hook_entry))
                        .unwrap_or(false)
                })
            });

        let mcp_installed = claude_settings
            .as_ref()
            .and_then(|s| s.get("mcpServers"))
            .and_then(|m| m.get("budi"))
            .is_some_and(|budi| {
                budi.get("command")
                    .and_then(|c| c.as_str())
                    .is_some_and(|cmd| cmd.contains("budi"))
                    && budi
                        .get("args")
                        .and_then(|a| a.as_array())
                        .is_some_and(|args| args.iter().any(|a| a.as_str() == Some("mcp-serve")))
            });

        let otel_installed = claude_settings
            .as_ref()
            .and_then(|s| s.get("env").and_then(|e| e.as_object()))
            .is_some_and(|env| {
                let endpoint_ok = env
                    .get("OTEL_EXPORTER_OTLP_ENDPOINT")
                    .and_then(|v| v.as_str())
                    .is_some_and(|url| {
                        let lower = url.to_lowercase();
                        lower.contains("127.0.0.1") || lower.contains("localhost")
                    });
                endpoint_ok
                    && env
                        .get("CLAUDE_CODE_ENABLE_TELEMETRY")
                        .and_then(|v| v.as_str())
                        == Some("1")
                    && env
                        .get("OTEL_EXPORTER_OTLP_PROTOCOL")
                        .and_then(|v| v.as_str())
                        == Some("http/json")
                    && env.get("OTEL_METRICS_EXPORTER").and_then(|v| v.as_str()) == Some("otlp")
                    && env.get("OTEL_LOGS_EXPORTER").and_then(|v| v.as_str()) == Some("otlp")
            });

        let statusline_installed = claude_settings
            .as_ref()
            .and_then(|s| s.get("statusLine"))
            .and_then(|sl| sl.get("command"))
            .and_then(|c| c.as_str())
            .map(|c| c.contains("budi statusline") || c.contains("budi_out=$(budi"))
            .unwrap_or(false);

        // Cursor hooks
        let cursor_path = format!("{home}/.cursor/hooks.json");
        let cursor_hooks = std::fs::read_to_string(&cursor_path)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| v.get("hooks").and_then(|h| h.as_object()).cloned())
            .is_some_and(|hooks| {
                CURSOR_HOOK_EVENTS.iter().all(|event| {
                    hooks
                        .get(*event)
                        .and_then(|v| v.as_array())
                        .map(|arr| arr.iter().any(is_budi_cursor_hook_entry))
                        .unwrap_or(false)
                })
            });

        // Cursor extension
        let cursor_extension = std::process::Command::new("cursor")
            .arg("--list-extensions")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| {
                let out = String::from_utf8_lossy(&o.stdout);
                out.lines()
                    .any(|l| l.trim().eq_ignore_ascii_case("siropkin.budi"))
            })
            .unwrap_or(false);

        // Starship integration (optional shell prompt integration)
        let starship_path = format!("{home}/.config/starship.toml");
        let starship = std::fs::read_to_string(&starship_path)
            .ok()
            .is_some_and(|raw| {
                let has_section = raw.contains("[custom.budi]");
                let has_command = raw.contains("budi statusline --format=starship")
                    || raw.contains("budi statusline --format starship");
                has_section && has_command
            });

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
            claude_code_hooks: hooks_installed,
            cursor_hooks,
            cursor_extension,
            mcp_server: mcp_installed,
            otel: otel_installed,
            statusline: statusline_installed,
            starship,
            database: db_stats,
            paths: IntegrationPaths {
                database: db_path_str,
                config: config_dir,
                claude_settings: claude_path,
                cursor_hooks: cursor_path,
            },
        }
    })
    .await
    .map_err(|e| super::internal_error(anyhow::anyhow!("{e}")))?;

    Ok(Json(result))
}

pub async fn sync_status(State(state): State<AppState>) -> Json<SyncStatusResponse> {
    let syncing = state.syncing.load(std::sync::atomic::Ordering::Acquire);
    let last_synced = tokio::task::spawn_blocking(|| {
        let db_path = budi_core::analytics::db_path().ok()?;
        let conn = budi_core::analytics::open_db(&db_path).ok()?;
        conn.query_row("SELECT MAX(last_synced) FROM sync_state", [], |r| {
            r.get::<_, Option<String>>(0)
        })
        .ok()
        .flatten()
    })
    .await
    .ok()
    .flatten();
    Json(SyncStatusResponse {
        syncing,
        last_synced,
    })
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

// ---------------------------------------------------------------------------
// Hook event ingestion
//
// This endpoint opens its own SQLite connection via `open_db`, which is safe
// to run concurrently with the background sync.  SQLite in WAL mode allows
// concurrent readers, and write serialization is handled by SQLite's internal
// locking (SQLITE_BUSY with a timeout configured via `busy_timeout`).  The
// `syncing` AtomicBool only guards against *duplicate* long-running syncs;
// hook ingestion writes are small and fast, so the SQLite-level lock is
// sufficient to prevent data corruption.
// ---------------------------------------------------------------------------

pub async fn hooks_ingest(
    Json(payload): Json<Value>,
) -> Result<Json<Value>, (StatusCode, Json<serde_json::Value>)> {
    tokio::task::spawn_blocking(move || {
        let event = budi_core::hooks::parse_hook_event(&payload)?;

        let db_path = budi_core::analytics::db_path()?;
        let mut conn = budi_core::analytics::open_db(&db_path)?;

        let tx = conn.transaction()?;

        // If prompt submission, classify and update session
        if matches!(event.event.as_str(), "user_prompt_submit")
            && let Some(prompt) = payload
                .get("user_prompt")
                .or_else(|| payload.get("prompt"))
                .and_then(|v| v.as_str())
            && let Some(category) = budi_core::hooks::classify_prompt(prompt)
        {
            let _ = budi_core::hooks::update_session_category(&tx, &event, &category);
        }

        budi_core::hooks::upsert_session(&tx, &event)?;
        budi_core::hooks::ingest_hook_event(&tx, &event)?;

        tx.commit()?;
        Ok::<_, anyhow::Error>(())
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("{e}")))?
    .map_err(internal_error)?;

    Ok(Json(json!({"ok": true})))
}
