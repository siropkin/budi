use std::collections::BTreeSet;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use budi_core::config;

use crate::daemon::ensure_daemon_running;
use crate::{InitIntegrationsMode, StatuslinePreset};

/// Run `budi init`. Prints warnings to stderr if integration setup had issues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitOutcome {
    Success,
    PartialSuccess,
}

#[allow(clippy::too_many_arguments)]
pub fn cmd_init(
    local: bool,
    yes: bool,
    with: Vec<super::integrations::IntegrationComponent>,
    without: Vec<super::integrations::IntegrationComponent>,
    integrations_mode: InitIntegrationsMode,
    statusline_preset: Option<StatuslinePreset>,
    repo_root: Option<PathBuf>,
    no_daemon: bool,
    _no_open: bool,
    _no_sync: bool,
) -> Result<InitOutcome> {
    let repo_root = if local || repo_root.is_some() {
        let root = super::try_resolve_repo_root(repo_root);
        if root.is_none() {
            anyhow::bail!(
                "Not in a git repository. Use `budi init` (without --local) for global setup,\n\
                 or run from inside a git repo."
            );
        }
        root
    } else {
        None
    };

    let config = match &repo_root {
        Some(root) => {
            let cfg = config::load_or_default(root)?;
            config::ensure_repo_layout(root)?;
            config::save(root, &cfg)?;
            cfg
        }
        None => config::BudiConfig::default(),
    };

    clean_duplicate_binaries();
    check_daemon_binary_and_version();

    let agents_config = resolve_agents(yes, io::stdin().is_terminal())?;
    if let Err(e) = config::save_agents_config(&agents_config) {
        eprintln!(
            "{}  Warning:{} could not persist agent preferences: {e}",
            super::ansi("\x1b[33m"),
            super::ansi("\x1b[0m")
        );
    }
    print_agents_summary(&agents_config);
    let proxy_warnings = super::proxy_install::apply_auto_proxy_configuration(
        &agents_config,
        config.proxy.effective_port(),
    );

    let selected_integrations = resolve_init_integrations(
        local,
        yes,
        with,
        without,
        integrations_mode,
        io::stdin().is_terminal(),
        &agents_config,
    )?;
    let statusline_preset = resolve_statusline_preset(
        &selected_integrations,
        statusline_preset,
        !yes && io::stdin().is_terminal(),
    )?;

    if selected_integrations.is_empty() {
        println!("  Integrations: skipped");
    } else {
        let names = selected_integrations
            .iter()
            .map(|c| c.display_name())
            .collect::<Vec<_>>()
            .join(", ");
        println!("  Integrations: {names}");
    }

    let install_report =
        super::integrations::install_selected(&config, &selected_integrations, statusline_preset);
    let mut integration_warnings = install_report.warnings;
    integration_warnings.extend(
        proxy_warnings
            .into_iter()
            .map(|warning| format!("Auto-proxy config: {warning}")),
    );

    if repo_root.is_none() {
        let mut prefs = super::integrations::load_preferences();
        prefs.enabled = selected_integrations.clone();
        if statusline_preset.is_some() {
            prefs.statusline_preset = statusline_preset;
        }
        if let Err(e) = super::integrations::save_preferences(&prefs) {
            eprintln!(
                "{}  Warning:{} could not persist integration preferences: {e}",
                super::ansi("\x1b[33m"),
                super::ansi("\x1b[0m")
            );
        }
    }

    let had_integration_warnings = !integration_warnings.is_empty();

    if had_integration_warnings {
        eprintln!("  Warning: integration setup had issues:");
        for w in &integration_warnings {
            eprintln!("    - {w}");
        }
        eprintln!("  Run `budi doctor` to diagnose.");
    }

    // Ensure database schema is ready BEFORE starting daemon.
    if let Ok(db_path) = budi_core::analytics::db_path()
        && let Err(e) = budi_core::analytics::open_db_with_migration(&db_path)
    {
        eprintln!("  Database: schema setup failed: {e}");
    }

    if !no_daemon {
        println!("  Daemon: starting...");
        ensure_daemon_running(repo_root.as_deref(), &config)?;
        println!("  Daemon: running on {}", config.daemon_base_url());

        // Install autostart service so daemon survives reboots
        install_autostart_service(&config);
    }

    let bold_cyan = super::ansi("\x1b[1;36m");
    let bold = super::ansi("\x1b[1m");
    let dim = super::ansi("\x1b[90m");
    let reset = super::ansi("\x1b[0m");

    let status_suffix = if had_integration_warnings {
        " with warnings"
    } else {
        ""
    };

    println!();
    if let Some(ref root) = repo_root {
        println!(
            "{bold_cyan}  budi{reset} initialized{status_suffix} in {}",
            root.display()
        );
    } else {
        println!("{bold_cyan}  budi{reset} initialized{status_suffix} (global)");
    }
    println!();
    if let Some(ref root) = repo_root {
        println!(
            "  Data:      {}",
            config::repo_paths(root)
                .map(|p| p.data_dir.display().to_string())
                .unwrap_or_else(|_| "<unable to resolve repo data path>".to_string())
        );
    } else {
        println!(
            "  Data:      {}",
            config::budi_home_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "<unable to resolve budi home>".to_string())
        );
    }
    println!();
    println!("  {bold}Next steps:{reset}");
    println!(
        "    1. Start coding as usual (`claude`, `codex`, `cursor`, `gh copilot`) — proxy is auto-configured"
    );
    println!("       {dim}Need one-off bypass? Use `BUDI_BYPASS=1 budi launch <agent>`{reset}");
    println!(
        "    2. Import history: `budi import` {dim}(load past transcripts from Claude Code / Codex / Copilot / Cursor){reset}"
    );
    println!("    3. Health check:   `budi status`");
    println!("    4. Cloud dashboard:  https://app.getbudi.dev");
    println!();
    if selected_integrations.is_empty() {
        println!(
            "  {dim}No integrations were installed. Use `budi integrations install ...` anytime.{reset}"
        );
    }

    let has_cli_agents = agents_config.claude_code.enabled
        || agents_config.codex_cli.enabled
        || agents_config.copilot_cli.enabled;
    if has_cli_agents {
        let yellow = super::ansi("\x1b[33m");
        println!(
            "  {yellow}\u{26a0}  Restart your terminal{reset} {dim}(or `source ~/.zshrc`){reset} {yellow}to activate proxy routing for CLI agents.{reset}"
        );
        println!("     {dim}Already-running sessions are NOT going through the proxy yet.{reset}");
        println!(
            "     {dim}For immediate proxy routing without restart: budi launch <agent>{reset}"
        );
    } else if !selected_integrations.is_empty() {
        println!("  {dim}Restart Cursor/IDE apps to pick up updated proxy settings.{reset}");
    }
    println!();

    if had_integration_warnings {
        let yellow = super::ansi("\x1b[33m");
        let reset2 = super::ansi("\x1b[0m");
        eprintln!(
            "{yellow}  Warning:{reset2} {} integration issue(s) detected. Run `budi doctor` for details.",
            integration_warnings.len()
        );
    }

    if had_integration_warnings {
        Ok(InitOutcome::PartialSuccess)
    } else {
        Ok(InitOutcome::Success)
    }
}

fn resolve_agents(yes: bool, is_tty: bool) -> Result<config::AgentsConfig> {
    if let Some(existing) = config::load_agents_config()
        && (yes || !is_tty)
    {
        return Ok(existing);
    }

    if !is_tty || yes {
        return Ok(config::AgentsConfig::all_enabled());
    }

    println!();
    println!("  Select agents to track:");
    let claude_enabled = prompt_yes_no("  - Claude Code?", true)?;
    let codex_enabled = prompt_yes_no("  - Codex CLI?", true)?;
    let cursor_enabled = prompt_yes_no("  - Cursor?", true)?;
    let copilot_enabled = prompt_yes_no("  - Copilot CLI?", true)?;
    println!();

    Ok(config::AgentsConfig {
        claude_code: config::AgentEntry {
            enabled: claude_enabled,
        },
        codex_cli: config::AgentEntry {
            enabled: codex_enabled,
        },
        cursor: config::AgentEntry {
            enabled: cursor_enabled,
        },
        copilot_cli: config::AgentEntry {
            enabled: copilot_enabled,
        },
    })
}

fn print_agents_summary(agents: &config::AgentsConfig) {
    let green = super::ansi("\x1b[32m");
    let dim = super::ansi("\x1b[90m");
    let reset = super::ansi("\x1b[0m");
    let agents_list: Vec<&str> = [
        agents.claude_code.enabled.then_some("Claude Code"),
        agents.codex_cli.enabled.then_some("Codex CLI"),
        agents.cursor.enabled.then_some("Cursor"),
        agents.copilot_cli.enabled.then_some("Copilot CLI"),
    ]
    .into_iter()
    .flatten()
    .collect();
    if agents_list.is_empty() {
        println!("  {dim}Agents: none enabled{reset}");
    } else {
        println!("  {green}Agents:{reset} {}", agents_list.join(", "));
    }
}

fn resolve_init_integrations(
    local: bool,
    yes: bool,
    with: Vec<super::integrations::IntegrationComponent>,
    without: Vec<super::integrations::IntegrationComponent>,
    mode: InitIntegrationsMode,
    is_tty: bool,
    agents_config: &config::AgentsConfig,
) -> Result<BTreeSet<super::integrations::IntegrationComponent>> {
    let mut selected = match mode {
        InitIntegrationsMode::None => BTreeSet::new(),
        InitIntegrationsMode::All => super::integrations::all_components(),
        InitIntegrationsMode::Auto => {
            if !is_tty || yes {
                if local {
                    BTreeSet::new()
                } else {
                    super::integrations::default_recommended_components()
                }
            } else {
                prompt_for_integrations(local)?
            }
        }
    };

    for component in with {
        selected.insert(component);
    }
    for component in without {
        selected.remove(&component);
    }

    filter_integrations_by_agents(&mut selected, agents_config);
    selected.remove(&super::integrations::IntegrationComponent::ClaudeCodeHooks);
    selected.remove(&super::integrations::IntegrationComponent::ClaudeCodeOtel);
    selected.remove(&super::integrations::IntegrationComponent::CursorHooks);

    Ok(selected)
}

/// Remove integration components for agents that are not enabled.
fn filter_integrations_by_agents(
    selected: &mut BTreeSet<super::integrations::IntegrationComponent>,
    agents: &config::AgentsConfig,
) {
    use super::integrations::IntegrationComponent;
    if !agents.claude_code.enabled {
        selected.remove(&IntegrationComponent::ClaudeCodeHooks);
        selected.remove(&IntegrationComponent::ClaudeCodeOtel);
        selected.remove(&IntegrationComponent::ClaudeCodeStatusline);
    }
    if !agents.cursor.enabled {
        selected.remove(&IntegrationComponent::CursorHooks);
        selected.remove(&IntegrationComponent::CursorExtension);
    }
}

fn resolve_statusline_preset(
    selected: &BTreeSet<super::integrations::IntegrationComponent>,
    preset: Option<StatuslinePreset>,
    prompt: bool,
) -> Result<Option<StatuslinePreset>> {
    if !selected.contains(&super::integrations::IntegrationComponent::ClaudeCodeStatusline) {
        return Ok(None);
    }
    if preset.is_some() || !prompt {
        return Ok(preset);
    }

    println!();
    println!("  Choose Claude Code status line preset:");
    println!("    1) coach  (session cost + health)");
    println!("    2) cost   (today/week/month)");
    println!("    3) full   (session + health + today)");
    eprint!("  Preset [1]: ");
    io::stdout().flush().ok();
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("Failed to read stdin")?;
    let chosen = match input.trim() {
        "2" | "cost" => StatuslinePreset::Cost,
        "3" | "full" => StatuslinePreset::Full,
        _ => StatuslinePreset::Coach,
    };
    Ok(Some(chosen))
}

fn prompt_for_integrations(
    local: bool,
) -> Result<BTreeSet<super::integrations::IntegrationComponent>> {
    if local {
        let enable_global = prompt_yes_no(
            "  Install global integrations too (Claude status line and Cursor extension)?",
            false,
        )?;
        if !enable_global {
            return Ok(BTreeSet::new());
        }
    }

    println!();
    println!("  Select integrations to install:");
    let mut selected = BTreeSet::new();
    let defaults = [
        (
            super::integrations::IntegrationComponent::ClaudeCodeStatusline,
            true,
        ),
        (
            super::integrations::IntegrationComponent::CursorExtension,
            true,
        ),
    ];
    for (component, default_enabled) in defaults {
        let question = format!("  - {}?", component.display_name());
        if prompt_yes_no(&question, default_enabled)? {
            selected.insert(component);
        }
    }
    println!();
    Ok(selected)
}

fn prompt_yes_no(question: &str, default_yes: bool) -> Result<bool> {
    let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
    loop {
        eprint!("{question} {hint} ");
        io::stdout().flush().ok();
        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .context("Failed to read stdin")?;
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Ok(default_yes);
        }
        match trimmed {
            "y" | "Y" | "yes" | "YES" | "Yes" => return Ok(true),
            "n" | "N" | "no" | "NO" | "No" => return Ok(false),
            _ => {
                println!("  Please enter y or n.");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Daemon binary check (combined existence + version in one spawn)
// ---------------------------------------------------------------------------

/// Check budi-daemon availability and version match. Warns on missing or mismatch.
fn check_daemon_binary_and_version() {
    let cli_version = env!("CARGO_PKG_VERSION");
    let yellow = super::ansi("\x1b[33m");
    let reset = super::ansi("\x1b[0m");

    match Command::new("budi-daemon").arg("--version").output() {
        Ok(output) if output.status.success() => {
            let raw = String::from_utf8_lossy(&output.stdout);
            let daemon_version = raw
                .trim()
                .strip_prefix("budi-daemon ")
                .unwrap_or(raw.trim());
            if daemon_version != cli_version {
                eprintln!(
                    "{yellow}  Warning:{reset} version mismatch: CLI v{cli_version} but daemon v{daemon_version}. \
                     Run `budi update` or reinstall both binaries."
                );
            }
        }
        _ => {
            eprintln!(
                "{yellow}  Warning:{reset} budi-daemon not found on PATH. \
                 The daemon may fail to start."
            );
            eprintln!("  Ensure both budi and budi-daemon are installed in the same directory.");
        }
    }
}

// ---------------------------------------------------------------------------
// Duplicate binary cleanup
// ---------------------------------------------------------------------------

/// Detect duplicate `budi`/`budi-daemon` binaries in PATH and warn.
///
/// We do not delete or uninstall anything automatically — users should opt in
/// to binary removal to avoid destructive surprises.
pub(crate) fn clean_duplicate_binaries() {
    let Ok(path_var) = std::env::var("PATH") else {
        return;
    };
    let Ok(current_exe) = std::env::current_exe().and_then(|p| p.canonicalize()) else {
        return;
    };

    let duplicate_bins: Vec<(&str, Vec<PathBuf>)> = ["budi", "budi-daemon"]
        .iter()
        .filter_map(|bin_name| {
            let mut found: Vec<PathBuf> = Vec::new();
            for dir in std::env::split_paths(&path_var) {
                let candidate = dir.join(bin_name);
                if candidate.exists()
                    && let Ok(resolved) = candidate.canonicalize()
                    && !found.iter().any(|p| p == &resolved)
                {
                    found.push(resolved);
                }
            }
            if found.len() > 1 {
                Some((*bin_name, found))
            } else {
                None
            }
        })
        .collect();

    if duplicate_bins.is_empty() {
        clean_backup_files();
        return;
    }

    let exe_str = current_exe.to_string_lossy();
    let exe_lower = exe_str.to_lowercase();
    let is_brew = exe_lower.contains("/cellar/") || exe_lower.contains("/homebrew/");
    let is_standalone = exe_str.contains("/.local/bin/");

    let yellow = super::ansi("\x1b[33m");
    let reset = super::ansi("\x1b[0m");

    eprintln!(
        "{yellow}  Warning:{reset} multiple budi binaries found in PATH. \
         Keep only one install source to avoid CLI/daemon version mismatches."
    );
    for (name, paths) in &duplicate_bins {
        let rendered = paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!("    - {name}: {rendered}");
    }

    if is_brew {
        eprintln!("  Active install source: Homebrew");
        eprintln!("  Suggested cleanup: remove standalone copies from ~/.local/bin (if unused).");
    } else if is_standalone && brew_has_budi() {
        eprintln!("  Active install source: standalone (~/.local/bin)");
        eprintln!("  Suggested cleanup: run `brew uninstall budi`.");
    } else {
        eprintln!("  Suggested cleanup: remove stale copies and keep one location first on PATH.");
    }

    clean_backup_files();
}

fn clean_backup_files() {
    let Ok(home) = config::home_dir() else {
        return;
    };
    let bin_dir = home.join(".local").join("bin");
    let Ok(entries) = fs::read_dir(&bin_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let fname = entry.file_name();
        let fname = fname.to_string_lossy();
        if (fname.starts_with("budi.bak") || fname.starts_with("budi-daemon.bak"))
            && entry.path().is_file()
        {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn brew_has_budi() -> bool {
    Command::new("brew")
        .args(["list", "budi"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Install the platform-native autostart service (launchd / systemd / Task Scheduler).
fn install_autostart_service(config: &config::BudiConfig) {
    let daemon_bin = match crate::daemon::resolve_daemon_binary() {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "{}  Warning:{} could not resolve daemon binary for autostart: {e}",
                super::ansi("\x1b[33m"),
                super::ansi("\x1b[0m"),
            );
            return;
        }
    };

    match budi_core::autostart::install_service(
        &daemon_bin,
        &config.daemon_host,
        config.daemon_port,
    ) {
        Ok(()) => {
            let mechanism = budi_core::autostart::service_mechanism();
            println!("  Autostart: installed ({mechanism})");
        }
        Err(e) => {
            eprintln!(
                "{}  Warning:{} autostart setup failed: {e}",
                super::ansi("\x1b[33m"),
                super::ansi("\x1b[0m"),
            );
            eprintln!(
                "  The daemon will not auto-restart after reboot. Run `budi init` again to retry."
            );
        }
    }
}
