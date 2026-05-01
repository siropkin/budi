use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use budi_core::config;
use budi_core::provider::Provider;

use crate::daemon::ensure_daemon_running;

pub fn cmd_init(no_integrations: bool, no_daemon: bool) -> Result<()> {
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

    // #521: auto-seed `device_id` in `~/.config/budi/cloud.toml` when
    // cloud sync is opted in but the field is still commented out.
    // Matches the template comment's long-standing promise that
    // `budi init` seeds these values on a real enable. Prints a
    // single-line status so the user sees whether the seeding just
    // happened, already existed, or was skipped because cloud is off.
    // `org_id` is NOT auto-generated (it has to come from the dashboard
    // Settings page); we nudge the user separately when it's missing.
    announce_cloud_device_id_seeding();

    if !no_integrations {
        install_default_integrations(&config);
    }

    // #548: auto-import historical transcripts so `budi stats` has
    // data immediately after setup. `cmd_import(false, false)` is
    // idempotent (ingest skips already-seen message ids via the
    // per-path offset table), so this is safe to run on every
    // `budi init` — first install walks the full history, repeat
    // inits become fast no-ops. Skipped under `--no-daemon` because
    // the import routes through `/sync/*` on the daemon we didn't
    // start. Import failure is warn-only — init stays successful.
    if !no_daemon {
        println!();
        if let Err(e) = super::import::cmd_import(false, false) {
            let yellow = super::ansi("\x1b[33m");
            let reset = super::ansi("\x1b[0m");
            eprintln!("{yellow}  Warning:{reset} historical import failed: {e:#}");
            eprintln!("  Run `budi db import` to retry.");
        }
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

/// #521: seed `cloud.device_id` into `~/.config/budi/cloud.toml` when
/// the user has opted into cloud sync but the field is still the
/// commented template line. Pre-8.3.2 the template promised that
/// `budi init` would do this but the seeding logic was never wired up;
/// users were left with `budi cloud status = enabled but not fully
/// configured` after following the documented flow.
///
/// On `Generated`, print a short confirmation + a nudge to set
/// `org_id` (which the dashboard Settings page exposes). On
/// `AlreadySet`, stay silent — subsequent `budi init` runs should not
/// nag. On `Skipped`, stay silent — cloud sync isn't opted in, so
/// the user hasn't asked budi to touch `cloud.toml`. Failures print a
/// warning but do NOT abort the overall `budi init`, because the rest
/// of init (daemon / autostart / integrations) is independent.
fn announce_cloud_device_id_seeding() {
    let dim = super::ansi("\x1b[90m");
    let green = super::ansi("\x1b[32m");
    let yellow = super::ansi("\x1b[33m");
    let reset = super::ansi("\x1b[0m");
    match budi_core::config::seed_cloud_device_id_if_needed() {
        Ok(budi_core::config::SeedDeviceIdOutcome::Generated(id)) => {
            // Surface the first 8 chars of the UUID so an operator
            // with two devices can tell them apart in the dashboard
            // without having to open the TOML.
            let short = id.split('-').next().unwrap_or(&id);
            println!(
                "  {green}✓{reset} Cloud device_id seeded ({short}…) in ~/.config/budi/cloud.toml"
            );
            // Nudge for org_id if it's still missing. Checking via
            // `load_cloud_config` keeps this one read-only pass; the
            // file was just mutated above, so the re-load is
            // intentional.
            let cfg = budi_core::config::load_cloud_config();
            if cfg.org_id.is_none() {
                println!(
                    "  {yellow}!{reset} Set {dim}org_id{reset} in ~/.config/budi/cloud.toml {dim}(copy from Settings page at https://app.getbudi.dev/dashboard/settings){reset}"
                );
            }
        }
        Ok(budi_core::config::SeedDeviceIdOutcome::AlreadySet) => {
            // Quiet — user has already completed setup, no nag.
        }
        Ok(budi_core::config::SeedDeviceIdOutcome::Skipped) => {
            // Quiet — no cloud.toml or cloud isn't enabled. Fresh users
            // who haven't run `budi cloud init` see nothing.
        }
        Err(e) => {
            eprintln!("  {yellow}!{reset} Could not seed cloud.device_id: {e:#}");
        }
    }
}

/// Install the default recommended integrations (Claude Code statusline,
/// Cursor extension) without prompting. This is the 8.3 fresh-user-flow
/// contract for #454: `budi init` leaves Claude Code with a working Budi
/// statusline. Callers that want a silent init (CI, containers, hand-rolled
/// Claude / Cursor settings) pass `--no-integrations`.
///
/// The underlying installer is idempotent — Claude statusline merges the
/// existing command when one is present, and the Cursor extension install
/// is a no-op if the extension is already present — so calling this on
/// every `budi init` is safe for repeat runs.
///
/// We drop the Claude Code statusline from the default set when `~/.claude`
/// does not exist: Claude Code is not installed on this machine, so
/// silently creating `~/.claude/settings.json` here would be a hidden
/// directory-creation side effect outside the documented scope.
fn install_default_integrations(config: &config::BudiConfig) {
    let mut selected = super::integrations::default_recommended_components();

    if !claude_code_installed() {
        selected.remove(&super::integrations::IntegrationComponent::ClaudeCodeStatusline);
    }

    if selected.is_empty() {
        return;
    }

    // #604: snapshot statusline state before install so we can decide
    // whether to surface the discoverability hint. A user who already
    // has both the budi marker in `~/.claude/settings.json` and a
    // `~/.config/budi/statusline.toml` file has been onboarded and may
    // have customized their config — repeating the hint would nag.
    let statusline_in_selection =
        selected.contains(&super::integrations::IntegrationComponent::ClaudeCodeStatusline);
    let statusline_was_installed =
        statusline_in_selection && super::integrations::claude_statusline_installed();
    let statusline_toml_existed = config::statusline_config_path()
        .map(|p| p.exists())
        .unwrap_or(false);

    let report = super::integrations::install_selected(config, &selected, None);

    let mut prefs = super::integrations::load_preferences();
    prefs
        .enabled
        .retain(|component| !component.is_removed_surface());
    for component in &selected {
        prefs.enabled.insert(*component);
    }
    let _ = super::integrations::save_preferences(&prefs);

    if !report.warnings.is_empty() {
        let yellow = super::ansi("\x1b[33m");
        let reset = super::ansi("\x1b[0m");
        for warning in &report.warnings {
            eprintln!("{yellow}  Warning:{reset} {warning}");
        }
    }

    if should_print_statusline_hint(
        statusline_in_selection,
        statusline_was_installed,
        statusline_toml_existed,
    ) {
        print_statusline_discoverability_hint();
    }
}

/// #604: pure suppression rule for the statusline customization hint.
/// Print on first install of the statusline, or when the budi marker
/// is in `~/.claude/settings.json` but `statusline.toml` is missing
/// (pre-#600 install state). Stay quiet when the user is fully
/// onboarded, or when the statusline isn't part of this install path
/// at all.
fn should_print_statusline_hint(
    statusline_in_selection: bool,
    statusline_was_installed: bool,
    statusline_toml_existed: bool,
) -> bool {
    if !statusline_in_selection {
        return false;
    }
    !(statusline_was_installed && statusline_toml_existed)
}

fn print_statusline_discoverability_hint() {
    let dim = super::ansi("\x1b[90m");
    let reset = super::ansi("\x1b[0m");
    println!("  Status line: cost preset {dim}(rolling 1d / 7d / 30d){reset}");
    println!(
        "    Customize: ~/.config/budi/statusline.toml {dim}— try `budi integrations install --statusline-preset coach` for live session vitals{reset}"
    );
}

fn claude_code_installed() -> bool {
    let Ok(home) = config::home_dir() else {
        return false;
    };
    home.join(".claude").is_dir()
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
    use super::{DetectedAgent, detect_agents_from_providers, should_print_statusline_hint};
    use anyhow::Result;
    use budi_core::provider::{DiscoveredFile, Provider};
    use std::path::{Path, PathBuf};

    #[test]
    fn statusline_hint_prints_on_fresh_install() {
        // Neither the budi marker nor the toml file exist yet — the user
        // is being onboarded for the first time, so the hint is on.
        assert!(should_print_statusline_hint(true, false, false));
    }

    #[test]
    fn statusline_hint_suppressed_when_already_onboarded() {
        // Budi marker present in claude settings AND statusline.toml
        // exists — the user has already gone through onboarding (and may
        // have edited the file). Stay silent.
        assert!(!should_print_statusline_hint(true, true, true));
    }

    #[test]
    fn statusline_hint_prints_when_marker_present_but_toml_missing() {
        // Pre-#600 install state: claude settings have the budi marker
        // but `statusline.toml` was never seeded. Showing the hint
        // surfaces the new file the user has just been given.
        assert!(should_print_statusline_hint(true, true, false));
    }

    #[test]
    fn statusline_hint_prints_when_toml_present_but_marker_missing() {
        // User uninstalled and is reinstalling the statusline. The toml
        // file is theirs to keep, but they're going through install
        // again — surface the hint.
        assert!(should_print_statusline_hint(true, false, true));
    }

    #[test]
    fn statusline_hint_suppressed_when_statusline_not_in_selection() {
        // `~/.claude` does not exist on this machine (or some other
        // gate stripped the statusline from the selection). The hint is
        // about a thing that wasn't installed — don't print it.
        assert!(!should_print_statusline_hint(false, false, false));
        assert!(!should_print_statusline_hint(false, true, true));
    }

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
