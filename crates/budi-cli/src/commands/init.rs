use std::collections::BTreeSet;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use budi_core::config;

use crate::daemon::ensure_daemon_running;
use crate::{InitIntegrationsMode, StatuslinePreset};

/// Run `budi init`. Prints warnings to stderr if hook installation had issues.
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
    no_sync: bool,
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
    let integration_warnings = install_report.warnings;

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

    // Detect re-init before sync — DB already exists means quick sync is enough.
    let is_reinit = if let Some(ref root) = repo_root {
        config::repo_paths(root)
            .map(|p| p.data_dir.join("analytics.db").exists())
            .unwrap_or(false)
    } else {
        budi_core::analytics::db_path()
            .map(|p| p.exists())
            .unwrap_or(false)
    };

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
    }

    // Fresh install: full history sync (users won't run `budi sync --all` manually).
    // Re-init: quick 30-day sync (fast, data already exists).
    let sync_result = if no_sync {
        Ok((0, 0))
    } else if is_reinit {
        println!("  Sync: syncing recent transcripts...");
        super::sync::init_quick_sync()
    } else {
        println!("  Sync: scanning transcripts (this may take a few minutes)...");
        super::sync::init_full_sync()
    };

    let dashboard_url = format!("{}/dashboard", config.daemon_base_url());

    let bold_cyan = super::ansi("\x1b[1;36m");
    let bold = super::ansi("\x1b[1m");
    let reset = super::ansi("\x1b[0m");

    let status_suffix = if had_integration_warnings {
        " with warnings"
    } else {
        ""
    };

    println!();
    if let Some(ref root) = repo_root {
        if is_reinit {
            println!(
                "{bold_cyan}  budi{reset} re-initialized{status_suffix} in {}",
                root.display()
            );
        } else {
            println!(
                "{bold_cyan}  budi{reset} initialized{status_suffix} in {}",
                root.display()
            );
        }
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
    let sync_counts = match sync_result {
        Ok(counts) => Some(counts),
        Err(e) => {
            tracing::warn!("auto-sync failed: {e}");
            println!("  Sync: skipped (run `budi sync` manually).");
            None
        }
    };
    println!();
    let dim = super::ansi("\x1b[90m");
    println!("  {bold}Next steps:{reset}");
    println!("    1. Run `budi stats` to see your spending");
    let mut next_step = 2usize;
    if is_reinit {
        println!(
            "    {next_step}. Run `budi sync --all` to load full history {dim}(only last 30 days were synced){reset}"
        );
        next_step += 1;
    }
    if !no_sync
        && let Some((_, messages_synced)) = sync_counts
        && messages_synced == 0
    {
        println!(
            "    {next_step}. No transcript data yet — open Claude Code or Cursor, send one prompt, then run `budi sync`"
        );
        next_step += 1;
    }
    println!("    {next_step}. {dim}Local dashboard (legacy): {dashboard_url}{reset}");
    println!();
    if selected_integrations.is_empty() {
        println!(
            "  {dim}No integrations were installed. Use `budi integrations install ...` anytime.{reset}"
        );
    } else {
        println!("  {dim}Restart Claude Code and Cursor to activate updated integrations.{reset}");
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
    let cursor_enabled = prompt_yes_no("  - Cursor?", true)?;
    println!();

    Ok(config::AgentsConfig {
        claude_code: config::AgentEntry {
            enabled: claude_enabled,
        },
        cursor: config::AgentEntry {
            enabled: cursor_enabled,
        },
    })
}

fn print_agents_summary(agents: &config::AgentsConfig) {
    let green = super::ansi("\x1b[32m");
    let dim = super::ansi("\x1b[90m");
    let reset = super::ansi("\x1b[0m");
    let agents_list: Vec<&str> = [
        agents.claude_code.enabled.then_some("Claude Code"),
        agents.cursor.enabled.then_some("Cursor"),
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
