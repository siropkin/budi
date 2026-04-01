use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use budi_core::config;
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, ValueEnum, Serialize, Deserialize,
)]
#[clap(rename_all = "kebab-case")]
#[serde(rename_all = "kebab-case")]
pub enum IntegrationComponent {
    ClaudeCodeHooks,
    ClaudeCodeMcp,
    ClaudeCodeOtel,
    ClaudeCodeStatusline,
    CursorHooks,
    CursorExtension,
    Starship,
}

impl IntegrationComponent {
    pub fn display_name(self) -> &'static str {
        match self {
            Self::ClaudeCodeHooks => "Claude Code hooks",
            Self::ClaudeCodeMcp => "Claude Code MCP server",
            Self::ClaudeCodeOtel => "Claude Code OTEL",
            Self::ClaudeCodeStatusline => "Claude Code status line",
            Self::CursorHooks => "Cursor hooks",
            Self::CursorExtension => "Cursor extension",
            Self::Starship => "Starship prompt integration",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[clap(rename_all = "kebab-case")]
#[serde(rename_all = "kebab-case")]
pub enum StatuslinePreset {
    Coach,
    Cost,
    Full,
}

impl StatuslinePreset {
    fn as_str(self) -> &'static str {
        match self {
            Self::Coach => "coach",
            Self::Cost => "cost",
            Self::Full => "full",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct IntegrationPreferences {
    pub enabled: BTreeSet<IntegrationComponent>,
    pub statusline_preset: Option<StatuslinePreset>,
}

#[derive(Debug, Default)]
pub struct InstallReport {
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallState {
    Installed,
    NotInstalled,
}

pub fn cmd_integrations(action: crate::IntegrationAction) -> Result<()> {
    let cfg = crate::client::DaemonClient::load_config();
    match action {
        crate::IntegrationAction::List => {
            println!("budi integrations");
            println!();
            for component in all_components() {
                let state = detect_component_state(&cfg, component);
                let mark = if matches!(state, InstallState::Installed) {
                    format!("{}✓{}", super::ansi("\x1b[32m"), super::ansi("\x1b[0m"))
                } else {
                    format!("{}·{}", super::ansi("\x1b[90m"), super::ansi("\x1b[0m"))
                };
                let status = if matches!(state, InstallState::Installed) {
                    "installed"
                } else {
                    "not installed"
                };
                println!("  {mark} {:<28} {status}", component.display_name());
            }
            println!();
            println!("Install later with `budi integrations install --with <name>`");
            Ok(())
        }
        crate::IntegrationAction::Install {
            with,
            all,
            statusline_preset,
            yes,
        } => {
            let mut selected = if all {
                all_components()
            } else if !with.is_empty() {
                with.into_iter().collect()
            } else {
                default_recommended_components()
            };

            if selected.is_empty() {
                selected = default_recommended_components();
            }

            let mut preset = statusline_preset;
            if selected.contains(&IntegrationComponent::ClaudeCodeStatusline)
                && preset.is_none()
                && !yes
                && io::stdin().is_terminal()
            {
                preset = Some(prompt_statusline_preset()?);
            }

            if !yes && io::stdin().is_terminal() {
                println!("Will install:");
                for component in &selected {
                    println!("  - {}", component.display_name());
                }
                eprint!("Continue? [y/N] ");
                io::stdout().flush().ok();
                let mut answer = String::new();
                io::stdin()
                    .read_line(&mut answer)
                    .context("Failed to read stdin")?;
                if !matches!(answer.trim(), "y" | "Y") {
                    println!("Aborted.");
                    return Ok(());
                }
            }

            let report = install_selected(&cfg, &selected, preset);
            let mut prefs = load_preferences();
            for component in &selected {
                prefs.enabled.insert(*component);
            }
            if preset.is_some() {
                prefs.statusline_preset = preset;
            }
            let _ = save_preferences(&prefs);

            if report.warnings.is_empty() {
                println!("Integrations updated.");
            } else {
                println!("Integrations updated with warnings:");
                for warning in report.warnings {
                    println!("  - {warning}");
                }
            }
            Ok(())
        }
    }
}

fn prompt_statusline_preset() -> Result<StatuslinePreset> {
    println!();
    println!("Choose Claude Code status line preset:");
    println!("  1) coach  (session cost + health)");
    println!("  2) cost   (today/week/month)");
    println!("  3) full   (session + health + today)");
    eprint!("Preset [1]: ");
    io::stdout().flush().ok();
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("Failed to read stdin")?;
    let preset = match input.trim() {
        "2" | "cost" => StatuslinePreset::Cost,
        "3" | "full" => StatuslinePreset::Full,
        _ => StatuslinePreset::Coach,
    };
    Ok(preset)
}

pub fn default_recommended_components() -> BTreeSet<IntegrationComponent> {
    [
        IntegrationComponent::ClaudeCodeHooks,
        IntegrationComponent::ClaudeCodeMcp,
        IntegrationComponent::ClaudeCodeOtel,
        IntegrationComponent::ClaudeCodeStatusline,
        IntegrationComponent::CursorHooks,
        IntegrationComponent::CursorExtension,
    ]
    .into_iter()
    .collect()
}

pub fn all_components() -> BTreeSet<IntegrationComponent> {
    [
        IntegrationComponent::ClaudeCodeHooks,
        IntegrationComponent::ClaudeCodeMcp,
        IntegrationComponent::ClaudeCodeOtel,
        IntegrationComponent::ClaudeCodeStatusline,
        IntegrationComponent::CursorHooks,
        IntegrationComponent::CursorExtension,
        IntegrationComponent::Starship,
    ]
    .into_iter()
    .collect()
}

pub fn integrations_config_path() -> Result<PathBuf> {
    Ok(config::budi_config_dir()?.join("integrations.toml"))
}

pub fn load_preferences() -> IntegrationPreferences {
    let path = match integrations_config_path() {
        Ok(p) => p,
        Err(_) => return IntegrationPreferences::default(),
    };
    if !path.exists() {
        return IntegrationPreferences::default();
    }
    let raw = match fs::read_to_string(&path) {
        Ok(r) => r,
        Err(_) => return IntegrationPreferences::default(),
    };
    toml::from_str::<IntegrationPreferences>(&raw).unwrap_or_default()
}

pub fn save_preferences(prefs: &IntegrationPreferences) -> Result<()> {
    let path = integrations_config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    let raw = toml::to_string_pretty(prefs)?;
    fs::write(&path, raw).with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(())
}

pub fn detect_component_state(
    config: &config::BudiConfig,
    component: IntegrationComponent,
) -> InstallState {
    let installed = match component {
        IntegrationComponent::ClaudeCodeHooks => claude_hooks_installed(),
        IntegrationComponent::ClaudeCodeMcp => claude_mcp_installed(),
        IntegrationComponent::ClaudeCodeOtel => claude_otel_installed(config),
        IntegrationComponent::ClaudeCodeStatusline => claude_statusline_installed(),
        IntegrationComponent::CursorHooks => cursor_hooks_installed(),
        IntegrationComponent::CursorExtension => is_cursor_extension_installed(),
        IntegrationComponent::Starship => starship_installed(),
    };
    if installed {
        InstallState::Installed
    } else {
        InstallState::NotInstalled
    }
}

pub fn infer_preferences_from_system(config: &config::BudiConfig) -> IntegrationPreferences {
    let mut prefs = IntegrationPreferences::default();
    for component in all_components() {
        if matches!(
            detect_component_state(config, component),
            InstallState::Installed
        ) {
            prefs.enabled.insert(component);
        }
    }
    prefs
}

pub fn install_selected(
    config: &config::BudiConfig,
    selected: &BTreeSet<IntegrationComponent>,
    statusline_preset: Option<StatuslinePreset>,
) -> InstallReport {
    let mut report = InstallReport::default();

    let uses_claude_settings = selected.contains(&IntegrationComponent::ClaudeCodeHooks)
        || selected.contains(&IntegrationComponent::ClaudeCodeMcp)
        || selected.contains(&IntegrationComponent::ClaudeCodeOtel)
        || selected.contains(&IntegrationComponent::ClaudeCodeStatusline);

    if uses_claude_settings
        && let Err(e) = install_claude_settings(config, selected, statusline_preset)
    {
        let yellow = super::ansi("\x1b[33m");
        let reset = super::ansi("\x1b[0m");
        eprintln!("{yellow}  Warning:{reset} Claude Code setup failed: {e}");
        report.warnings.push(format!("Claude Code settings: {e}"));
    }

    if selected.contains(&IntegrationComponent::CursorHooks)
        && let Err(e) = install_cursor_hooks()
    {
        let yellow = super::ansi("\x1b[33m");
        let reset = super::ansi("\x1b[0m");
        eprintln!("{yellow}  Warning:{reset} Cursor hooks: {e}");
        report.warnings.push(format!("Cursor hooks: {e}"));
    }

    if selected.contains(&IntegrationComponent::CursorExtension) {
        install_cursor_extension(&mut report.warnings);
    }

    if selected.contains(&IntegrationComponent::Starship)
        && let Err(e) = install_starship_integration(config)
    {
        let yellow = super::ansi("\x1b[33m");
        let reset = super::ansi("\x1b[0m");
        eprintln!("{yellow}  Warning:{reset} Starship integration failed: {e}");
        report.warnings.push(format!("Starship: {e}"));
    }

    report
}

pub fn refresh_enabled_integrations(config: &config::BudiConfig) -> InstallReport {
    let mut prefs = load_preferences();
    if prefs.enabled.is_empty() {
        prefs = infer_preferences_from_system(config);
        if !prefs.enabled.is_empty() {
            let _ = save_preferences(&prefs);
        }
    }
    if prefs.enabled.is_empty() {
        return InstallReport::default();
    }
    install_selected(config, &prefs.enabled, prefs.statusline_preset)
}

fn install_claude_settings(
    config: &config::BudiConfig,
    selected: &BTreeSet<IntegrationComponent>,
    statusline_preset: Option<StatuslinePreset>,
) -> Result<()> {
    let home = budi_core::config::home_dir()?;
    let settings_path = home.join(super::statusline::CLAUDE_USER_SETTINGS);
    let mut settings = super::read_json_object_strict(&settings_path)?;
    let mut changed = false;

    if selected.contains(&IntegrationComponent::ClaudeCodeHooks)
        && super::statusline::remove_legacy_budi_hooks_from_value(&mut settings)
    {
        eprintln!(
            "  Cleaned up legacy budi hooks from {}",
            settings_path.display()
        );
        changed = true;
    }

    if selected.contains(&IntegrationComponent::ClaudeCodeStatusline) {
        match apply_statusline(&mut settings)? {
            StatuslineApply::Changed => {
                println!("  Status line: configured in {}", settings_path.display());
                changed = true;
            }
            StatuslineApply::AlreadyConfigured => {
                println!("  Status line: already configured");
            }
            StatuslineApply::ManualMergeRequired => {
                let yellow = super::ansi("\x1b[33m");
                let reset = super::ansi("\x1b[0m");
                eprintln!(
                    "{yellow}  Warning:{reset} existing statusLine command is non-budi. \
                     On Windows, merge is shell-dependent, so budi did not modify it automatically."
                );
            }
        }
        if let Some(preset) = statusline_preset {
            set_statusline_preset(preset)?;
        }
    }

    if selected.contains(&IntegrationComponent::ClaudeCodeHooks) && apply_cc_hooks(&mut settings) {
        println!(
            "  Hooks: installed Claude Code hooks in {}",
            settings_path.display()
        );
        changed = true;
    } else if selected.contains(&IntegrationComponent::ClaudeCodeHooks) {
        println!("  Hooks: Claude Code hooks already installed");
    }

    if selected.contains(&IntegrationComponent::ClaudeCodeMcp) {
        if apply_mcp_server(&mut settings) {
            println!(
                "  MCP: installed budi server in {}",
                settings_path.display()
            );
            changed = true;
        } else {
            println!("  MCP: budi server already configured");
        }
    }

    if selected.contains(&IntegrationComponent::ClaudeCodeOtel)
        && apply_otel_env_vars(&mut settings, config, &settings_path)
    {
        changed = true;
    }

    if changed {
        super::atomic_write_json(&settings_path, &settings)?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatuslineApply {
    Changed,
    AlreadyConfigured,
    ManualMergeRequired,
}

/// Apply statusline configuration. Returns whether settings were changed.
pub(crate) fn apply_statusline(settings: &mut Value) -> Result<StatuslineApply> {
    if let Some(existing) = settings.get("statusLine") {
        if !existing.is_object() {
            anyhow::bail!("statusLine is not an object — fix it manually before installing");
        }
        let existing_cmd = existing
            .get("command")
            .and_then(|c| c.as_str())
            .unwrap_or("");
        if super::statusline::statusline_has_budi(existing_cmd) {
            return Ok(StatuslineApply::AlreadyConfigured);
        }
        // Avoid shell-specific command concatenation on Windows.
        if cfg!(target_os = "windows") {
            return Ok(StatuslineApply::ManualMergeRequired);
        }
        let merged = format!(
            "{existing_cmd}{}",
            super::statusline::budi_statusline_suffix()
        );
        settings["statusLine"]["command"] = Value::String(merged);
        return Ok(StatuslineApply::Changed);
    }

    settings["statusLine"] = json!({
        "type": "command",
        "command": super::statusline::BUDI_STATUSLINE_CMD,
        "padding": 0
    });
    Ok(StatuslineApply::Changed)
}

pub fn set_statusline_preset(preset: StatuslinePreset) -> Result<()> {
    let path = config::statusline_config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    let mut sl_cfg = config::load_statusline_config();
    sl_cfg.preset = Some(preset.as_str().to_string());
    sl_cfg.format = None;
    let raw = toml::to_string_pretty(&sl_cfg)?;
    fs::write(&path, raw).with_context(|| format!("Failed writing {}", path.display()))?;
    println!(
        "  Status line: preset set to `{}` in {}",
        preset.as_str(),
        path.display()
    );
    Ok(())
}

fn budi_hook_cmd() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "budi hook"
    }
    #[cfg(not(target_os = "windows"))]
    {
        "budi hook 2>/dev/null || true"
    }
}

fn apply_cc_hooks(settings: &mut Value) -> bool {
    let obj = match settings.as_object_mut() {
        Some(o) => o,
        None => return false,
    };
    let hooks = obj.entry("hooks").or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }

    let budi_hook_entry = json!({
        "matcher": "",
        "hooks": [{
            "type": "command",
            "command": budi_hook_cmd(),
            "async": true
        }]
    });

    let mut changed = false;
    for event in super::CC_HOOK_EVENTS {
        let Some(hooks_map) = hooks.as_object_mut() else {
            break;
        };
        let event_arr = hooks_map.entry(*event).or_insert_with(|| json!([]));
        if !event_arr.is_array() {
            *event_arr = json!([]);
        }

        let Some(arr_mut) = event_arr.as_array_mut() else {
            continue;
        };
        let had_legacy = arr_mut.iter().any(is_legacy_cc_hook);
        if had_legacy {
            arr_mut.retain(|e| !is_legacy_cc_hook(e));
            changed = true;
        }

        let already_installed = arr_mut.iter().any(super::is_budi_cc_hook_entry);
        if !already_installed {
            if let Some(arr) = event_arr.as_array_mut() {
                arr.push(budi_hook_entry.clone());
            }
            changed = true;
        }
    }

    changed
}

fn apply_mcp_server(settings: &mut Value) -> bool {
    let obj = match settings.as_object_mut() {
        Some(o) => o,
        None => return false,
    };
    let mcp_servers = obj.entry("mcpServers").or_insert_with(|| json!({}));
    if !mcp_servers.is_object() {
        *mcp_servers = json!({});
    }

    let budi_path = which_budi();
    let desired = json!({
        "command": budi_path,
        "args": ["mcp-serve"],
        "type": "stdio"
    });

    let mcp_obj = mcp_servers.as_object_mut().unwrap();
    if mcp_obj.get("budi") == Some(&desired) {
        return false;
    }

    mcp_obj.insert("budi".to_string(), desired);
    true
}

/// Apply OTEL env vars. Returns true when settings changed.
fn apply_otel_env_vars(
    settings: &mut Value,
    config: &config::BudiConfig,
    settings_path: &Path,
) -> bool {
    let obj = match settings.as_object_mut() {
        Some(o) => o,
        None => return false,
    };
    let env = obj.entry("env").or_insert_with(|| json!({}));
    if !env.is_object() {
        *env = json!({});
    }
    let env_obj = env.as_object_mut().unwrap();

    let budi_endpoint = format!("http://127.0.0.1:{}", config.daemon_port);

    if let Some(existing) = env_obj
        .get("OTEL_EXPORTER_OTLP_ENDPOINT")
        .and_then(|v| v.as_str())
    {
        let existing_lower = existing.to_lowercase();
        let is_localhost =
            existing_lower.contains("127.0.0.1") || existing_lower.contains("localhost");
        if !is_localhost {
            println!(
                "  OTEL: already configured (pointing to {existing}). \
                 To also send to budi, use an OTEL Collector with multiple exporters."
            );
            return false;
        }
    }

    let otel_vars = [
        ("CLAUDE_CODE_ENABLE_TELEMETRY", "1"),
        ("OTEL_EXPORTER_OTLP_ENDPOINT", budi_endpoint.as_str()),
        ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json"),
        ("OTEL_METRICS_EXPORTER", "otlp"),
        ("OTEL_LOGS_EXPORTER", "otlp"),
    ];

    let mut changed = false;
    for (key, value) in &otel_vars {
        let current = env_obj.get(*key).and_then(|v| v.as_str());
        if current != Some(*value) {
            env_obj.insert(key.to_string(), json!(value));
            changed = true;
        }
    }

    if changed {
        println!(
            "  OTEL: configured telemetry in {}",
            settings_path.display()
        );
    } else {
        println!("  OTEL: telemetry already configured");
    }
    changed
}

fn install_cursor_hooks() -> Result<()> {
    let home = budi_core::config::home_dir()?;
    let hooks_path = home.join(".cursor/hooks.json");

    if let Some(parent) = hooks_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }

    let mut config = if hooks_path.exists() {
        super::read_json_object_strict(&hooks_path)?
    } else {
        json!({"version": 1, "hooks": {}})
    };

    if config.get("version").is_none() {
        config["version"] = json!(1);
    }
    if config.get("hooks").is_none() || !config["hooks"].is_object() {
        config["hooks"] = json!({});
    }

    let budi_hook_entry = json!({
        "command": budi_hook_cmd(),
        "type": "command"
    });

    let mut changed = false;
    let Some(hooks) = config.get_mut("hooks") else {
        anyhow::bail!("Cursor hooks config missing hooks key");
    };

    for event in super::CURSOR_HOOK_EVENTS {
        let Some(hooks_map) = hooks.as_object_mut() else {
            anyhow::bail!("Cursor hooks is not a JSON object");
        };
        let event_arr = hooks_map.entry(*event).or_insert_with(|| json!([]));
        if !event_arr.is_array() {
            *event_arr = json!([]);
        }

        let Some(arr_mut) = event_arr.as_array_mut() else {
            continue;
        };
        let had_legacy = arr_mut.iter().any(is_legacy_cursor_hook);
        if had_legacy {
            arr_mut.retain(|e| !is_legacy_cursor_hook(e));
            changed = true;
        }

        let already_installed = arr_mut.iter().any(super::is_budi_cursor_hook_entry);
        if !already_installed {
            if let Some(arr) = event_arr.as_array_mut() {
                arr.push(budi_hook_entry.clone());
            }
            changed = true;
        }
    }

    if changed {
        super::atomic_write_json(&hooks_path, &config)?;
        println!(
            "  Hooks: installed Cursor hooks in {}",
            hooks_path.display()
        );
    } else {
        println!("  Hooks: Cursor hooks already installed");
    }
    Ok(())
}

static CURSOR_EXTENSION_VSIX: &[u8] =
    include_bytes!("../../../../extensions/cursor-budi/cursor-budi.vsix");
static CURSOR_EXTENSION_PACKAGE_JSON: &str =
    include_str!("../../../../extensions/cursor-budi/package.json");

fn install_cursor_extension(warnings: &mut Vec<String>) {
    if CURSOR_EXTENSION_VSIX.is_empty() {
        return;
    }

    let cursor_cli = match find_cursor_cli() {
        Some(c) => c,
        None => return,
    };

    let bundled_version = bundled_extension_version();
    let installed_version = installed_extension_version(&cursor_cli);

    if let (Some(installed), Some(bundled)) =
        (installed_version.as_deref(), bundled_version.as_deref())
    {
        if compare_versions(installed, bundled) != Ordering::Less {
            println!("  Extension: Cursor extension already installed (v{installed})");
            return;
        }
    }

    let temp_dir = match create_secure_temp_dir("budi-vsix") {
        Ok(path) => path,
        Err(e) => {
            warnings.push(format!("Cursor extension temp dir: {e}"));
            return;
        }
    };

    let vsix_path = temp_dir.join("cursor-budi.vsix");
    if let Err(e) = fs::write(&vsix_path, CURSOR_EXTENSION_VSIX) {
        warnings.push(format!("Cursor extension write temp file: {e}"));
        let _ = fs::remove_dir_all(&temp_dir);
        return;
    }

    let result = Command::new(&cursor_cli)
        .args([
            "--install-extension",
            &vsix_path.to_string_lossy(),
            "--force",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    let _ = fs::remove_dir_all(&temp_dir);

    match result {
        Ok(status) if status.success() => {
            if let Some(installed) = installed_version {
                println!("  Extension: updated Cursor extension (from v{installed})");
            } else {
                println!("  Extension: installed Cursor extension");
            }
        }
        Ok(_) => {
            warnings.push("Cursor extension install failed".to_string());
        }
        Err(e) => {
            warnings.push(format!("could not run cursor CLI: {e}"));
        }
    }
}

fn create_secure_temp_dir(prefix: &str) -> io::Result<PathBuf> {
    let base = std::env::temp_dir();
    for _ in 0..16 {
        let stamp = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default();
        let candidate = base.join(format!("{prefix}-{stamp}-{}", std::process::id()));
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "Failed to allocate temp directory",
    ))
}

/// Check if the `cursor` CLI is on PATH (or at the well-known macOS location).
pub fn find_cursor_cli() -> Option<String> {
    let candidates = if cfg!(target_os = "macos") {
        vec!["cursor".to_string(), "/usr/local/bin/cursor".to_string()]
    } else {
        vec!["cursor".to_string()]
    };

    candidates.into_iter().find(|candidate| {
        Command::new(candidate)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

fn installed_extension_version(cursor_cli: &str) -> Option<String> {
    let output = Command::new(cursor_cli)
        .args(["--list-extensions", "--show-versions"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let out = String::from_utf8_lossy(&output.stdout);
    out.lines()
        .find_map(parse_extension_line)
        .map(|(_, version)| version)
}

fn parse_extension_line(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim();
    let lower = trimmed.to_ascii_lowercase();
    if !lower.starts_with("siropkin.budi") {
        return None;
    }
    if let Some((name, version)) = trimmed.split_once('@') {
        Some((name.to_string(), version.to_string()))
    } else {
        Some(("siropkin.budi".to_string(), String::new()))
    }
}

fn bundled_extension_version() -> Option<String> {
    let parsed = serde_json::from_str::<Value>(CURSOR_EXTENSION_PACKAGE_JSON).ok()?;
    parsed
        .get("version")
        .and_then(|v| v.as_str())
        .map(|v| v.to_string())
}

fn compare_versions(a: &str, b: &str) -> Ordering {
    let parse = |v: &str| -> Option<Vec<u64>> {
        let core = v
            .split_once('-')
            .map(|(lhs, _)| lhs)
            .unwrap_or(v)
            .trim_start_matches('v');
        if core.is_empty() {
            return None;
        }
        let mut out = Vec::new();
        for part in core.split('.') {
            out.push(part.parse::<u64>().ok()?);
        }
        Some(out)
    };
    match (parse(a), parse(b)) {
        (Some(mut av), Some(mut bv)) => {
            let max_len = av.len().max(bv.len());
            av.resize(max_len, 0);
            bv.resize(max_len, 0);
            av.cmp(&bv)
        }
        _ => a.cmp(b),
    }
}

/// Check if the budi Cursor extension is installed.
pub fn is_cursor_extension_installed() -> bool {
    match find_cursor_cli() {
        Some(cli) => Command::new(cli)
            .arg("--list-extensions")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| {
                let out = String::from_utf8_lossy(&o.stdout);
                out.lines()
                    .any(|l| l.trim().eq_ignore_ascii_case("siropkin.budi"))
            })
            .unwrap_or(false),
        None => false,
    }
}

fn install_starship_integration(config: &config::BudiConfig) -> Result<()> {
    let home = budi_core::config::home_dir()?;
    let starship_path = home.join(".config/starship.toml");
    if let Some(parent) = starship_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }

    let mut content = if starship_path.exists() {
        fs::read_to_string(&starship_path)
            .with_context(|| format!("Failed to read {}", starship_path.display()))?
    } else {
        String::new()
    };

    if content.contains("[custom.budi]") {
        println!("  Starship: integration already configured");
        return Ok(());
    }

    if !content.trim().is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }

    let health_url = format!("{}/health", config.daemon_base_url());
    let block = format!(
        r#"
# Added by budi
[custom.budi]
command = "budi statusline --format=starship"
when = "curl -sf {health_url} >/dev/null 2>&1"
format = "[$output]($style) "
style = "cyan"
shell = ["sh"]
"#
    );

    content.push_str(&block);
    fs::write(&starship_path, content)
        .with_context(|| format!("Failed writing {}", starship_path.display()))?;
    println!(
        "  Starship: installed integration in {}",
        starship_path.display()
    );
    Ok(())
}

pub fn claude_statusline_installed() -> bool {
    let Some(settings) = read_claude_settings() else {
        return false;
    };
    settings
        .get("statusLine")
        .and_then(|sl| sl.get("command"))
        .and_then(|c| c.as_str())
        .is_some_and(super::statusline::statusline_has_budi)
}

pub fn claude_hooks_installed() -> bool {
    let Some(settings) = read_claude_settings() else {
        return false;
    };
    let Some(hooks) = settings.get("hooks").and_then(|v| v.as_object()) else {
        return false;
    };
    for event in super::CC_HOOK_EVENTS {
        let ok = hooks
            .get(*event)
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().any(super::is_budi_cc_hook_entry))
            .unwrap_or(false);
        if !ok {
            return false;
        }
    }
    true
}

pub fn claude_mcp_installed() -> bool {
    let Some(settings) = read_claude_settings() else {
        return false;
    };
    let Some(budi) = settings.get("mcpServers").and_then(|m| m.get("budi")) else {
        return false;
    };
    budi.get("command")
        .and_then(|c| c.as_str())
        .is_some_and(|c| c.contains("budi"))
        && budi
            .get("args")
            .and_then(|a| a.as_array())
            .is_some_and(|args| args.iter().any(|a| a.as_str() == Some("mcp-serve")))
}

pub fn claude_otel_installed(config: &config::BudiConfig) -> bool {
    let Some(settings) = read_claude_settings() else {
        return false;
    };
    let Some(env) = settings.get("env").and_then(|e| e.as_object()) else {
        return false;
    };
    let expected_endpoint = format!("http://127.0.0.1:{}", config.daemon_port);
    let checks = [
        ("CLAUDE_CODE_ENABLE_TELEMETRY", Some("1")),
        (
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            Some(expected_endpoint.as_str()),
        ),
        ("OTEL_EXPORTER_OTLP_PROTOCOL", Some("http/json")),
        ("OTEL_METRICS_EXPORTER", Some("otlp")),
        ("OTEL_LOGS_EXPORTER", Some("otlp")),
    ];
    checks.iter().all(|(key, expected_val)| {
        env.get(*key)
            .and_then(|v| v.as_str())
            .is_some_and(|v| expected_val.is_none_or(|exp| v == exp))
    })
}

pub fn cursor_hooks_installed() -> bool {
    let home = match budi_core::config::home_dir() {
        Ok(h) => h,
        Err(_) => return false,
    };
    let path = home.join(".cursor/hooks.json");
    let raw = match fs::read_to_string(&path) {
        Ok(r) => r,
        Err(_) => return false,
    };
    let cfg = match serde_json::from_str::<Value>(&raw) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let Some(hooks) = cfg.get("hooks").and_then(|v| v.as_object()) else {
        return false;
    };
    for event in super::CURSOR_HOOK_EVENTS {
        let ok = hooks
            .get(*event)
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().any(super::is_budi_cursor_hook_entry))
            .unwrap_or(false);
        if !ok {
            return false;
        }
    }
    true
}

pub fn starship_installed() -> bool {
    let home = match budi_core::config::home_dir() {
        Ok(h) => h,
        Err(_) => return false,
    };
    let path = home.join(".config/starship.toml");
    let raw = match fs::read_to_string(path) {
        Ok(r) => r,
        Err(_) => return false,
    };
    raw.contains("[custom.budi]")
}

fn read_claude_settings() -> Option<Value> {
    let home = budi_core::config::home_dir().ok()?;
    let settings_path = home.join(super::statusline::CLAUDE_USER_SETTINGS);
    let raw = fs::read_to_string(settings_path).ok()?;
    serde_json::from_str::<Value>(&raw).ok()
}

/// Find the budi binary path, preferring the same directory as the running binary.
fn which_budi() -> String {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join("budi");
        if candidate.exists() {
            return candidate.display().to_string();
        }
    }
    "budi".to_string()
}

fn is_legacy_cc_hook(entry: &Value) -> bool {
    entry
        .get("hooks")
        .and_then(|h| h.as_array())
        .map(|hooks| {
            hooks.iter().any(|h| {
                h.get("command")
                    .and_then(|c| c.as_str())
                    .is_some_and(|c| c.trim() == "budi hook")
            })
        })
        .unwrap_or(false)
}

fn is_legacy_cursor_hook(entry: &Value) -> bool {
    entry
        .get("command")
        .and_then(|c| c.as_str())
        .is_some_and(|c| c.trim() == "budi hook")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compare_versions_orders_semver_like_values() {
        assert_eq!(compare_versions("1.2.3", "1.2.3"), Ordering::Equal);
        assert_eq!(compare_versions("1.2.4", "1.2.3"), Ordering::Greater);
        assert_eq!(compare_versions("1.2.3", "1.3.0"), Ordering::Less);
        assert_eq!(compare_versions("v1.10.0", "1.9.9"), Ordering::Greater);
    }

    #[test]
    fn parse_extension_line_extracts_name_and_version() {
        let parsed = parse_extension_line("siropkin.budi@0.5.1").expect("parsed");
        assert_eq!(parsed.0, "siropkin.budi");
        assert_eq!(parsed.1, "0.5.1");
        assert!(parse_extension_line("other.ext@1.0.0").is_none());
    }

    #[test]
    fn apply_statusline_adds_new_statusline_when_missing() {
        let mut settings = json!({});
        let state = apply_statusline(&mut settings).expect("apply");
        assert_eq!(state, StatuslineApply::Changed);
        assert_eq!(
            settings["statusLine"]["command"],
            super::super::statusline::BUDI_STATUSLINE_CMD
        );
    }

    #[test]
    fn apply_statusline_detects_existing_budi_statusline() {
        let mut settings = json!({
            "statusLine": {
                "type": "command",
                "command": "budi statusline"
            }
        });
        let state = apply_statusline(&mut settings).expect("apply");
        assert_eq!(state, StatuslineApply::AlreadyConfigured);
    }
}
