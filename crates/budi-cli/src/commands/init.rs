use std::fs;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use budi_core::config;
use budi_core::legacy_proxy::{self, LegacyProxyFile, LegacyProxyScan};
use budi_core::provider::Provider;

use crate::daemon::ensure_daemon_running;

pub fn cmd_init(cleanup: bool, yes: bool, no_daemon: bool) -> Result<()> {
    if cleanup {
        run_cleanup_flow(yes)?;
    }

    clean_duplicate_binaries();
    check_daemon_binary_and_version();

    let config = config::BudiConfig::default();
    let data_dir = config::budi_home_dir()?;
    fs::create_dir_all(&data_dir)
        .with_context(|| format!("Failed to create {}", data_dir.display()))?;

    if let Ok(db_path) = budi_core::analytics::db_path()
        && let Err(e) = budi_core::analytics::open_db_with_migration(&db_path)
    {
        eprintln!("  Database: schema setup failed: {e}");
    }

    let detected_agents = detect_agents();

    if !no_daemon {
        ensure_daemon_running(None, &config)?;
        println!("  Daemon: ready on {}", config.daemon_base_url());
        print_autostart_status(&config);
    }

    let bold_cyan = super::ansi("\x1b[1;36m");
    let dim = super::ansi("\x1b[90m");
    let reset = super::ansi("\x1b[0m");

    println!();
    println!("{bold_cyan}  budi{reset} initialized");
    println!("  Data dir: {}", data_dir.display());
    print_detected_agents(&detected_agents);
    println!();
    println!(
        "  Start coding as usual {dim}(`claude`, `codex`, `cursor`, `gh copilot` — budi tails local transcripts automatically){reset}"
    );
    println!(
        "  Run `budi doctor` for a full health check {dim}(daemon, tailer, schema, transcript visibility){reset}"
    );

    Ok(())
}

fn run_cleanup_flow(yes: bool) -> Result<()> {
    let scan = legacy_proxy::scan()?;
    let managed_files = scan
        .files
        .iter()
        .filter(|file| file.has_managed_blocks())
        .collect::<Vec<_>>();

    if managed_files.is_empty() {
        println!("No managed 8.0/8.1 Budi proxy blocks found. Nothing to clean.");
        print_manual_review_findings(&scan);
        print_current_shell_warning(&scan);
        println!();
        return Ok(());
    }

    if !yes && !std::io::stdin().is_terminal() {
        anyhow::bail!(
            "Non-interactive terminal. Use `budi init --cleanup --yes` to skip per-file confirmation."
        );
    }

    println!("Reviewing legacy 8.0/8.1 proxy residue...");

    let mut removed = 0usize;
    let mut skipped = 0usize;

    for file in managed_files {
        println!();
        print_cleanup_preview(file);

        let should_apply = yes || prompt_for_cleanup(file)?;
        if should_apply {
            file.apply_cleanup()?;
            removed += 1;
            println!(
                "  Removed managed Budi proxy block(s) from {}",
                file.path.display()
            );
        } else {
            skipped += 1;
            println!("  Skipped {}", file.path.display());
        }

        if file.has_fuzzy_findings() {
            print_file_manual_review(file);
        }
    }

    print_manual_review_findings(&scan);
    print_current_shell_warning(&scan);
    println!();
    println!("Cleanup summary: removed {removed} file(s), skipped {skipped}.");
    Ok(())
}

fn print_cleanup_preview(file: &LegacyProxyFile) {
    println!(
        "  {} ({})",
        file.path.display(),
        file.surface.display_name()
    );
    println!("    diff preview:");
    for block in &file.managed_blocks {
        let range = match block.end_line {
            Some(end_line) => format!("lines {}-{}", block.start_line, end_line),
            None => format!("line {} to EOF (unterminated block)", block.start_line),
        };
        println!("      - remove managed block at {range}");
        for line in &block.lines {
            println!("      - {}", line);
        }
    }
    if file.surface == budi_core::legacy_proxy::LegacyProxySurface::CursorSettings {
        println!("      - clean up any dangling JSON comma left behind by the removed block");
    }
}

fn prompt_for_cleanup(file: &LegacyProxyFile) -> Result<bool> {
    eprint!(
        "Remove the managed Budi proxy block(s) from {}? [y/N] ",
        file.path.display()
    );
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .context("Failed to read stdin")?;
    Ok(matches!(answer.trim(), "y" | "Y"))
}

fn print_manual_review_findings(scan: &LegacyProxyScan) {
    let fuzzy_only = scan
        .files
        .iter()
        .filter(|file| !file.has_managed_blocks() && file.has_fuzzy_findings())
        .collect::<Vec<_>>();
    if fuzzy_only.is_empty() {
        return;
    }
    println!();
    println!("Manual review only (not auto-removed):");
    for file in fuzzy_only {
        print_file_manual_review(file);
    }
}

fn print_file_manual_review(file: &LegacyProxyFile) {
    println!("  {}:", file.path.display());
    for finding in &file.fuzzy_findings {
        println!(
            "    ! line {} ({}) {}",
            finding.line_number, finding.label, finding.snippet
        );
    }
}

fn print_current_shell_warning(scan: &LegacyProxyScan) {
    if scan.exported_env_vars.is_empty() {
        return;
    }
    let rendered = scan
        .exported_env_vars
        .iter()
        .map(|entry| format!("{}={}", entry.key, entry.value))
        .collect::<Vec<_>>()
        .join(", ");
    println!();
    println!("Current shell still has legacy proxy env vars loaded: {rendered}");
    println!(
        "Open a fresh terminal after cleanup so those stale exports disappear from the live session."
    );
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DetectedAgent {
    display_name: &'static str,
    roots: Vec<PathBuf>,
}

fn detect_agents() -> Vec<DetectedAgent> {
    detect_agents_from_providers(budi_core::provider::all_providers())
}

fn detect_agents_from_providers(providers: Vec<Box<dyn Provider>>) -> Vec<DetectedAgent> {
    let mut detected = providers
        .into_iter()
        .filter_map(|provider| {
            let display_name = provider.display_name();
            let mut roots = provider.watch_roots();
            roots.sort();
            roots.dedup();
            if roots.is_empty() {
                None
            } else {
                Some(DetectedAgent {
                    display_name,
                    roots,
                })
            }
        })
        .collect::<Vec<_>>();
    detected.sort_by(|a, b| a.display_name.cmp(b.display_name));
    detected
}

fn print_detected_agents(agents: &[DetectedAgent]) {
    let green = super::ansi("\x1b[32m");
    let dim = super::ansi("\x1b[90m");
    let reset = super::ansi("\x1b[0m");
    if agents.is_empty() {
        println!("  {dim}Detected agents:{reset} none yet (no transcript roots found on disk)");
        return;
    }

    let names = agents
        .iter()
        .map(|agent| agent.display_name)
        .collect::<Vec<_>>()
        .join(", ");
    println!("  {green}Detected agents:{reset} {names}");
}

fn print_autostart_status(config: &config::BudiConfig) {
    use budi_core::autostart::ServiceStatus;

    let mechanism = budi_core::autostart::service_mechanism();
    match budi_core::autostart::service_status() {
        ServiceStatus::NotInstalled => install_autostart_service(config),
        ServiceStatus::Installed | ServiceStatus::Running => {
            println!("  Autostart: already installed ({mechanism})");
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

#[cfg(test)]
mod tests {
    use super::{DetectedAgent, detect_agents_from_providers};
    use anyhow::Result;
    use budi_core::provider::{DiscoveredFile, Provider};
    use std::path::{Path, PathBuf};

    struct StubProvider {
        display_name: &'static str,
        roots: Vec<PathBuf>,
    }

    impl Provider for StubProvider {
        fn name(&self) -> &'static str {
            "stub"
        }

        fn display_name(&self) -> &'static str {
            self.display_name
        }

        fn is_available(&self) -> bool {
            true
        }

        fn discover_files(&self) -> Result<Vec<DiscoveredFile>> {
            Ok(Vec::new())
        }

        fn parse_file(
            &self,
            _path: &Path,
            _content: &str,
            _offset: usize,
        ) -> Result<(Vec<budi_core::jsonl::ParsedMessage>, usize)> {
            Ok((Vec::new(), 0))
        }

        fn watch_roots(&self) -> Vec<PathBuf> {
            self.roots.clone()
        }
    }

    #[test]
    fn detect_agents_only_includes_providers_with_existing_roots() {
        let providers: Vec<Box<dyn Provider>> = vec![
            Box::new(StubProvider {
                display_name: "Claude Code",
                roots: vec![PathBuf::from("/tmp/.claude/projects")],
            }),
            Box::new(StubProvider {
                display_name: "Cursor",
                roots: Vec::new(),
            }),
        ];

        let detected = detect_agents_from_providers(providers);

        assert_eq!(
            detected,
            vec![DetectedAgent {
                display_name: "Claude Code",
                roots: vec![PathBuf::from("/tmp/.claude/projects")],
            }]
        );
    }

    #[test]
    fn detect_agents_sorts_and_dedups_roots() {
        let providers: Vec<Box<dyn Provider>> = vec![
            Box::new(StubProvider {
                display_name: "Codex",
                roots: vec![
                    PathBuf::from("/tmp/.codex/archived_sessions"),
                    PathBuf::from("/tmp/.codex/sessions"),
                    PathBuf::from("/tmp/.codex/sessions"),
                ],
            }),
            Box::new(StubProvider {
                display_name: "Claude Code",
                roots: vec![PathBuf::from("/tmp/.claude/projects")],
            }),
        ];

        let detected = detect_agents_from_providers(providers);

        assert_eq!(detected[0].display_name, "Claude Code");
        assert_eq!(detected[1].display_name, "Codex");
        assert_eq!(
            detected[1].roots,
            vec![
                PathBuf::from("/tmp/.codex/archived_sessions"),
                PathBuf::from("/tmp/.codex/sessions"),
            ]
        );
    }
}
