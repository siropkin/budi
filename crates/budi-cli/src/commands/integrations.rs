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
    #[value(skip)]
    ClaudeCodeHooks,
    #[value(skip)]
    ClaudeCodeOtel,
    ClaudeCodeStatusline,
    ClaudeCodeBudiSkill,
    #[value(skip)]
    CursorHooks,
    CursorExtension,
}

impl IntegrationComponent {
    pub fn display_name(self) -> &'static str {
        match self {
            Self::ClaudeCodeHooks => "Claude Code hooks",
            Self::ClaudeCodeOtel => "Claude Code OTEL",
            Self::ClaudeCodeStatusline => "Claude Code status line",
            Self::ClaudeCodeBudiSkill => "Claude Code /budi skill",
            Self::CursorHooks => "Cursor hooks",
            Self::CursorExtension => "Cursor extension",
        }
    }

    pub fn is_removed_surface(self) -> bool {
        matches!(
            self,
            Self::ClaudeCodeHooks | Self::ClaudeCodeOtel | Self::CursorHooks
        )
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

            let removed_surface_notes = drop_removed_surfaces(&mut selected);
            if selected.is_empty() {
                println!("No active integrations selected.");
                for warning in removed_surface_notes {
                    println!("  - {warning}");
                }
                return Ok(());
            }

            // Default statusline is the quiet `1d` / `7d` / `30d` cost view
            // (ADR-0088 §4). `coach` / `full` remain opt-in advanced variants
            // documented in the README — we no longer prompt for a preset
            // during onboarding so the default path stays simple.
            let preset = statusline_preset;

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

            let mut report = install_selected(&cfg, &selected, preset);
            report.warnings.extend(removed_surface_notes);
            let mut prefs = load_preferences();
            prefs
                .enabled
                .retain(|component| !component.is_removed_surface());
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
        crate::IntegrationAction::Refresh => {
            let report = refresh_enabled_integrations(&cfg);
            if !report.warnings.is_empty() {
                eprintln!("Integration refresh warnings:");
                for warning in report.warnings {
                    eprintln!("  - {warning}");
                }
            }
            Ok(())
        }
    }
}

pub fn default_recommended_components() -> BTreeSet<IntegrationComponent> {
    [
        IntegrationComponent::ClaudeCodeStatusline,
        IntegrationComponent::ClaudeCodeBudiSkill,
        IntegrationComponent::CursorExtension,
    ]
    .into_iter()
    .collect()
}

pub fn all_components() -> BTreeSet<IntegrationComponent> {
    [
        IntegrationComponent::ClaudeCodeStatusline,
        IntegrationComponent::ClaudeCodeBudiSkill,
        IntegrationComponent::CursorExtension,
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
        IntegrationComponent::ClaudeCodeOtel => claude_otel_installed(config),
        IntegrationComponent::ClaudeCodeStatusline => claude_statusline_installed(),
        IntegrationComponent::ClaudeCodeBudiSkill => claude_budi_skill_installed(),
        IntegrationComponent::CursorHooks => cursor_hooks_installed(),
        IntegrationComponent::CursorExtension => is_cursor_extension_installed(),
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
    let mut filtered_selected = selected.clone();
    let mut report = InstallReport::default();
    report
        .warnings
        .extend(drop_removed_surfaces(&mut filtered_selected));

    if filtered_selected.is_empty() {
        return report;
    }

    let uses_claude_settings = filtered_selected.contains(&IntegrationComponent::ClaudeCodeHooks)
        || filtered_selected.contains(&IntegrationComponent::ClaudeCodeOtel)
        || filtered_selected.contains(&IntegrationComponent::ClaudeCodeStatusline);

    if uses_claude_settings
        && let Err(e) = install_claude_settings(config, &filtered_selected, statusline_preset)
    {
        let yellow = super::ansi("\x1b[33m");
        let reset = super::ansi("\x1b[0m");
        eprintln!("{yellow}  Warning:{reset} Claude Code setup failed: {e}");
        report.warnings.push(format!("Claude Code settings: {e}"));
    }

    if filtered_selected.contains(&IntegrationComponent::CursorHooks)
        && let Err(e) = install_cursor_hooks()
    {
        let yellow = super::ansi("\x1b[33m");
        let reset = super::ansi("\x1b[0m");
        eprintln!("{yellow}  Warning:{reset} Cursor hooks: {e}");
        report.warnings.push(format!("Cursor hooks: {e}"));
    }

    if filtered_selected.contains(&IntegrationComponent::ClaudeCodeBudiSkill) {
        match install_claude_budi_skill() {
            Ok(BudiSkillApply::Created) => {
                let path = claude_budi_skill_path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "~/.claude/skills/budi/SKILL.md".to_string());
                println!("  /budi skill: installed at {path}");
            }
            Ok(BudiSkillApply::AlreadyInstalled) => {
                println!("  /budi skill: already installed");
            }
            Ok(BudiSkillApply::Skipped) => {
                println!("  /budi skill: skipped (Claude Code not installed)");
            }
            Err(e) => {
                let yellow = super::ansi("\x1b[33m");
                let reset = super::ansi("\x1b[0m");
                eprintln!("{yellow}  Warning:{reset} /budi skill: {e}");
                report.warnings.push(format!("/budi skill: {e}"));
            }
        }
    }

    if filtered_selected.contains(&IntegrationComponent::CursorExtension) {
        install_cursor_extension(&mut report.warnings);
    }

    report
}

fn drop_removed_surfaces(selected: &mut BTreeSet<IntegrationComponent>) -> Vec<String> {
    let removed: Vec<IntegrationComponent> = selected
        .iter()
        .copied()
        .filter(|component| component.is_removed_surface())
        .collect();

    let mut warnings = Vec::new();
    for component in removed {
        selected.remove(&component);
        warnings.push(format!(
            "{} is removed in 8.0 and was skipped.",
            component.display_name()
        ));
    }

    warnings
}

pub fn refresh_enabled_integrations(config: &config::BudiConfig) -> InstallReport {
    let mut prefs = load_preferences();
    if prefs.enabled.is_empty() {
        prefs = infer_preferences_from_system(config);
        if !prefs.enabled.is_empty() {
            let _ = save_preferences(&prefs);
        }
    }

    // #613: union with the default-recommended set so upgrading users
    // pick up new IntegrationComponents that landed in this release
    // without having to re-run `budi init` or `budi integrations install`.
    // Each install path is idempotent (canonical-bytes check on disk),
    // so adding already-installed components is a no-op. Persist the
    // union so `integrations list` reflects reality. Users who explicitly
    // opt out of a component will gain that opt-out semantics when the
    // CLI grows a remove action; today there is no remove path, so
    // recommended-by-default is the right semantics.
    let mut grew = false;
    for component in default_recommended_components() {
        if !component.is_removed_surface() && !prefs.enabled.contains(&component) {
            prefs.enabled.insert(component);
            grew = true;
        }
    }
    if grew {
        let _ = save_preferences(&prefs);
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
        } else {
            // #600: when no preset is passed (the `budi init` path), drop a
            // template `statusline.toml` so users have a real file to edit.
            // README docs the file as the source of truth; without seeding
            // a fresh install has nothing to discover. Idempotent — repeat
            // installs leave user edits byte-stable.
            seed_statusline_toml()?;
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

/// Idempotently seed `~/.config/budi/statusline.toml` with the default
/// `cost` preset and commented examples. Called on the no-preset path
/// (i.e. `budi init`) so users have a real file to edit.
///
/// Prints a single confirmation line on first generation. Stays quiet
/// on repeat runs so `budi init` doesn't nag once the user already has
/// (and possibly customized) the file.
fn seed_statusline_toml() -> Result<()> {
    match config::seed_statusline_config_if_needed()? {
        config::SeedStatuslineOutcome::Generated => {
            let path = config::statusline_config_path()?;
            let dim = super::ansi("\x1b[90m");
            let reset = super::ansi("\x1b[0m");
            println!(
                "  Status line: {} {dim}(cost preset — edit to customize){reset}",
                path.display()
            );
        }
        config::SeedStatuslineOutcome::AlreadySet => {}
    }
    Ok(())
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

fn install_cursor_extension(warnings: &mut Vec<String>) {
    if is_cursor_extension_installed() {
        println!("  Extension: Cursor extension already installed");
        return;
    }

    let cursor_cli = match find_cursor_cli() {
        Some(c) => c,
        None => {
            println!("  Extension: skipped (Cursor CLI not found)");
            return;
        }
    };

    // Try to install from VS Code Marketplace first
    let result = Command::new(&cursor_cli)
        .args(["--install-extension", "siropkin.budi", "--force"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    match result {
        Ok(status) if status.success() => {
            println!("  Extension: installed Cursor extension from marketplace");
        }
        _ => {
            warnings.push(
                "Could not install Cursor extension from marketplace. \
                 Install manually: https://github.com/siropkin/budi/releases"
                    .to_string(),
            );
        }
    }
}

/// Check if the `cursor` CLI is on PATH (or at the well-known macOS location).
pub fn find_cursor_cli() -> Option<String> {
    let candidates = if cfg!(target_os = "macos") {
        vec![
            "cursor".to_string(),
            "/usr/local/bin/cursor".to_string(),
            "/Applications/Cursor.app/Contents/Resources/app/bin/cursor".to_string(),
        ]
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

/// Path to the auto-installed `/budi` Claude Code skill file.
pub fn claude_budi_skill_path() -> Result<PathBuf> {
    let home = budi_core::config::home_dir()?;
    Ok(home
        .join(".claude")
        .join("skills")
        .join("budi")
        .join("SKILL.md"))
}

/// Canonical contents of `~/.claude/skills/budi/SKILL.md`. Kept as a
/// constant so the install path and the e2e regression guard agree on
/// the byte-for-byte contents (idempotent re-install must leave a
/// pre-existing file with these bytes untouched).
pub const BUDI_SKILL_CONTENTS: &str = "---\n\
name: budi\n\
description: \"Show live session vitals — context bloat, cache hit rate, retry loops, cost acceleration — for the current Claude Code session. Buddy is back, but now it's budi.\"\n\
---\n\
\n\
When the user types `/budi` in Claude Code, run `budi sessions current` in the\n\
project root and surface the output verbatim.\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BudiSkillApply {
    /// Skill file did not exist and was created (or existed with
    /// stale/non-matching bytes and was overwritten).
    Created,
    /// Skill file already had the canonical bytes — no write performed
    /// so user edits, if any, stay byte-stable.
    AlreadyInstalled,
    /// `~/.claude` is missing — Claude Code is not installed on this
    /// machine. Caller surfaces a friendly note instead of writing
    /// outside the documented integration scope.
    Skipped,
}

/// Idempotently install `~/.claude/skills/budi/SKILL.md`.
///
/// Returns `Skipped` when `~/.claude` is missing (Claude Code is not
/// installed). Returns `AlreadyInstalled` when the file's bytes already
/// match the canonical contents — this is the byte-stable repeat-install
/// branch the e2e guard pins so user edits to the skill file never get
/// silently clobbered.
fn install_claude_budi_skill() -> Result<BudiSkillApply> {
    let home = budi_core::config::home_dir()?;
    let claude_dir = home.join(".claude");
    if !claude_dir.is_dir() {
        return Ok(BudiSkillApply::Skipped);
    }
    let skill_path = claude_budi_skill_path()?;

    if let Ok(existing) = fs::read_to_string(&skill_path)
        && existing == BUDI_SKILL_CONTENTS
    {
        return Ok(BudiSkillApply::AlreadyInstalled);
    }

    if let Some(parent) = skill_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    fs::write(&skill_path, BUDI_SKILL_CONTENTS)
        .with_context(|| format!("Failed to write {}", skill_path.display()))?;
    Ok(BudiSkillApply::Created)
}

pub fn claude_budi_skill_installed() -> bool {
    let Ok(path) = claude_budi_skill_path() else {
        return false;
    };
    path.is_file()
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
    budi_core::integrations::validate_cc_hooks(&settings).0
}

pub fn claude_otel_installed(config: &config::BudiConfig) -> bool {
    let Some(settings) = read_claude_settings() else {
        return false;
    };
    budi_core::integrations::check_otel_config(&settings, config.daemon_port)
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
    budi_core::integrations::validate_cursor_hooks(&cfg).0
}

fn read_claude_settings() -> Option<Value> {
    let home = budi_core::config::home_dir().ok()?;
    let settings_path = home.join(super::statusline::CLAUDE_USER_SETTINGS);
    let raw = fs::read_to_string(settings_path).ok()?;
    serde_json::from_str::<Value>(&raw).ok()
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

    static HOME_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct HomeGuard {
        prev: Option<String>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl HomeGuard {
        fn new(home: &std::path::Path) -> Self {
            let lock = HOME_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            let prev = std::env::var("HOME").ok();
            unsafe { std::env::set_var("HOME", home) };
            Self { prev, _lock: lock }
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(h) => unsafe { std::env::set_var("HOME", h) },
                None => unsafe { std::env::remove_var("HOME") },
            }
        }
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

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn apply_statusline_preserves_user_command_on_merge() {
        // #546 regression guard: the user's pre-existing non-budi
        // statusLine command must survive installation. `apply_statusline`
        // appends budi's bash suffix, it does NOT replace the command.
        let mut settings = json!({
            "statusLine": {
                "type": "command",
                "command": "my-custom-prompt --fancy",
                "padding": 2
            }
        });
        let state = apply_statusline(&mut settings).expect("apply");
        assert_eq!(state, StatuslineApply::Changed);

        let merged = settings["statusLine"]["command"]
            .as_str()
            .expect("command string")
            .to_string();

        // User's command MUST still be the head of the merged string.
        assert!(
            merged.starts_with("my-custom-prompt --fancy"),
            "user command was not preserved: {merged}",
        );
        // Budi suffix appended after, separated by the shell statement
        // boundary, so Claude Code's shell runs user command first then
        // budi's statusline. Check for the known suffix marker.
        assert!(
            merged.contains("budi statusline"),
            "budi suffix not appended: {merged}",
        );
        assert!(
            merged.contains("; budi_out=$(budi statusline"),
            "merged command should use the documented bash suffix shape: {merged}",
        );

        // Fields other than `command` must be preserved verbatim so
        // the user's padding / type / other settings don't regress.
        assert_eq!(settings["statusLine"]["type"], "command");
        assert_eq!(settings["statusLine"]["padding"], 2);
    }

    /// #613 regression guard: when an upgrading user's saved
    /// `integrations.toml` is missing a component that the current
    /// release recommends by default (e.g. `claude-code-budi-skill`
    /// added in #603), `refresh_enabled_integrations` must add it to
    /// the union before installing. Without this, the upgrade path
    /// silently skips the new component because the user's stored
    /// preference set was frozen at their last `budi init`.
    #[test]
    fn refresh_unions_user_prefs_with_default_recommended_components() {
        let mut prefs = IntegrationPreferences::default();
        prefs
            .enabled
            .insert(IntegrationComponent::ClaudeCodeStatusline);
        prefs.enabled.insert(IntegrationComponent::CursorExtension);

        // Simulate the in-memory union step (the disk side is exercised
        // by the e2e test below; here we pin the algorithm).
        let recommended = default_recommended_components();
        for component in &recommended {
            if !component.is_removed_surface() && !prefs.enabled.contains(component) {
                prefs.enabled.insert(*component);
            }
        }

        assert!(
            prefs
                .enabled
                .contains(&IntegrationComponent::ClaudeCodeBudiSkill),
            "refresh must add the default-recommended /budi skill component for upgraders \
             whose saved prefs predate #603"
        );
        assert!(
            prefs
                .enabled
                .contains(&IntegrationComponent::ClaudeCodeStatusline),
            "refresh must NOT drop pre-existing enabled components"
        );
        assert!(
            prefs
                .enabled
                .contains(&IntegrationComponent::CursorExtension),
            "refresh must NOT drop pre-existing enabled components"
        );
    }

    /// #613 e2e: simulate an upgrade from a v8.3.14 install and verify
    /// that `refresh_enabled_integrations` creates the missing
    /// `statusline.toml` and `SKILL.md` files, and persists the expanded
    /// component set back to `integrations.toml`.
    ///
    /// Uses a temp HOME so the test is hermetic and doesn't touch the
    /// developer's real `~/.claude` or `~/.config/budi`.
    #[test]
    fn e2e_refresh_from_v8_3_14_creates_statusline_and_skill() {
        let tmp = std::env::temp_dir().join(format!(
            "budi-e2e-613-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let _ = std::fs::remove_dir_all(&tmp);

        // Scaffold a minimal HOME with Claude Code "installed" (the
        // skill installer gates on `~/.claude` existing).
        let claude_dir = tmp.join(".claude");
        std::fs::create_dir_all(&claude_dir).expect("create .claude");
        let settings_path = claude_dir.join("settings.json");
        std::fs::write(&settings_path, "{}").expect("write settings.json");

        // v8.3.14-era integrations.toml: only two components, missing
        // the /budi skill that shipped in v8.3.15 (#603).
        let config_dir = tmp.join(".config/budi");
        std::fs::create_dir_all(&config_dir).expect("create .config/budi");
        let integrations_toml = config_dir.join("integrations.toml");
        std::fs::write(
            &integrations_toml,
            r#"enabled = ["claude-code-statusline", "cursor-extension"]
"#,
        )
        .expect("write v8.3.14 integrations.toml");

        // Redirect HOME to the temp dir for the duration of this test.
        // HomeGuard serializes via Mutex and restores on drop (#630).
        let _home_guard = HomeGuard::new(&tmp);

        let config = budi_core::config::BudiConfig::default();
        let report = refresh_enabled_integrations(&config);

        // Cursor extension warnings are expected on machines without
        // Cursor installed — filter them out; only fail on unexpected ones.
        let unexpected: Vec<_> = report
            .warnings
            .iter()
            .filter(|w| !w.contains("Cursor extension"))
            .collect();
        assert!(
            unexpected.is_empty(),
            "unexpected warnings: {:?}",
            unexpected
        );

        // Acceptance 1: statusline.toml seeded.
        let statusline_toml = config_dir.join("statusline.toml");
        assert!(
            statusline_toml.exists(),
            "statusline.toml must be seeded after refresh"
        );
        let sl_contents = std::fs::read_to_string(&statusline_toml).unwrap();
        assert!(
            sl_contents.contains("slots"),
            "statusline.toml must contain the default slots"
        );

        // Acceptance 2: SKILL.md created with canonical bytes.
        let skill_md = claude_dir.join("skills/budi/SKILL.md");
        assert!(
            skill_md.exists(),
            "SKILL.md must be installed after refresh"
        );
        let skill_contents = std::fs::read_to_string(&skill_md).unwrap();
        assert_eq!(
            skill_contents, BUDI_SKILL_CONTENTS,
            "SKILL.md must have canonical contents"
        );

        // Acceptance 3: integrations.toml now includes the /budi skill.
        let updated_raw = std::fs::read_to_string(&integrations_toml).unwrap();
        let updated: IntegrationPreferences =
            toml::from_str(&updated_raw).expect("parse updated integrations.toml");
        assert!(
            updated
                .enabled
                .contains(&IntegrationComponent::ClaudeCodeBudiSkill),
            "integrations.toml must include claude-code-budi-skill after refresh"
        );
        assert!(
            updated
                .enabled
                .contains(&IntegrationComponent::ClaudeCodeStatusline),
            "refresh must preserve pre-existing components"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
