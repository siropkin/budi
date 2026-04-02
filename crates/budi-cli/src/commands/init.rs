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
    no_open: bool,
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

    let selected_integrations = resolve_init_integrations(
        local,
        yes,
        with,
        without,
        integrations_mode,
        io::stdin().is_terminal(),
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
    let hook_warnings = install_report.warnings;

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

    let had_hook_warnings = !hook_warnings.is_empty();

    if had_hook_warnings {
        eprintln!("  Warning: hook installation had issues:");
        for w in &hook_warnings {
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
    let underline = super::ansi("\x1b[4m");
    let reset = super::ansi("\x1b[0m");

    let status_suffix = if had_hook_warnings {
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
    println!("  Dashboard: {dashboard_url}");
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
    println!("    1. Open the dashboard: {underline}{dashboard_url}{reset}");
    println!("    2. Run `budi stats` to see your spending");
    let mut next_step = 3usize;
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
    }
    println!();
    if selected_integrations.is_empty() {
        println!(
            "  {dim}No integrations were installed. Use `budi integrations install ...` anytime.{reset}"
        );
    } else {
        println!("  {dim}Restart Claude Code and Cursor to activate updated integrations.{reset}");
    }
    println!();

    if !no_open && !is_reinit {
        open_url_in_browser(&dashboard_url);
    }

    if had_hook_warnings {
        let yellow = super::ansi("\x1b[33m");
        let reset2 = super::ansi("\x1b[0m");
        eprintln!(
            "{yellow}  Warning:{reset2} {} hook issue(s) detected. Run `budi doctor` for details.",
            hook_warnings.len()
        );
    }

    if had_hook_warnings {
        Ok(InitOutcome::PartialSuccess)
    } else {
        Ok(InitOutcome::Success)
    }
}

pub fn open_url_in_browser(url: &str) {
    let result = if cfg!(target_os = "macos") {
        Command::new("open")
            .arg(url)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
    } else if cfg!(target_os = "windows") {
        Command::new("cmd")
            .args(["/C", "start", "", url])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
    } else {
        Command::new("xdg-open")
            .arg(url)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
    };
    if let Err(e) = result {
        tracing::debug!("Could not open browser: {e}");
    }
}

fn resolve_init_integrations(
    local: bool,
    yes: bool,
    with: Vec<super::integrations::IntegrationComponent>,
    without: Vec<super::integrations::IntegrationComponent>,
    mode: InitIntegrationsMode,
    is_tty: bool,
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

    Ok(selected)
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
            "  Install global integrations too (Claude/Cursor hooks, status line, MCP, OTEL)?",
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
            super::integrations::IntegrationComponent::ClaudeCodeHooks,
            true,
        ),
        (
            super::integrations::IntegrationComponent::ClaudeCodeMcp,
            true,
        ),
        (
            super::integrations::IntegrationComponent::ClaudeCodeOtel,
            true,
        ),
        (
            super::integrations::IntegrationComponent::ClaudeCodeStatusline,
            true,
        ),
        (super::integrations::IntegrationComponent::CursorHooks, true),
        (
            super::integrations::IntegrationComponent::CursorExtension,
            true,
        ),
        (super::integrations::IntegrationComponent::Starship, false),
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
