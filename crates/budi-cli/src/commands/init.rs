use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use budi_core::config;
use serde_json::{Value, json};

use crate::daemon::ensure_daemon_running;

/// Run `budi init`. Prints warnings to stderr if hook installation had issues.
pub fn cmd_init(
    local: bool,
    repo_root: Option<PathBuf>,
    no_daemon: bool,
    no_open: bool,
    no_sync: bool,
) -> Result<()> {
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

    // Install all Claude Code integrations (settings.json) in a single read-modify-write pass.
    let hook_warnings = install_claude_code_settings(&config);
    let had_hook_warnings = !hook_warnings.is_empty();

    // Cursor hooks are in a separate file.
    if let Err(e) = install_cursor_hooks() {
        eprintln!(
            "{}  Warning:{} Cursor hooks: {e}",
            super::ansi("\x1b[33m"),
            super::ansi("\x1b[0m")
        );
    }

    // Cursor extension (embedded .vsix)
    install_cursor_extension();

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
                .unwrap_or_else(|_| "~/.local/share/budi".to_string())
        );
    } else {
        println!(
            "  Data:      {}",
            config::budi_home_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "~/.local/share/budi".to_string())
        );
    }
    println!("  Dashboard: {dashboard_url}");
    println!();
    if let Err(e) = sync_result {
        tracing::warn!("auto-sync failed: {e}");
        println!("  Sync: skipped (run `budi sync` manually).");
    }
    println!();
    let dim = super::ansi("\x1b[90m");
    println!("  {bold}Next steps:{reset}");
    println!("    1. Open the dashboard: {underline}{dashboard_url}{reset}");
    println!("    2. Run `budi stats` to see your spending");
    if is_reinit {
        println!(
            "    3. Run `budi sync --all` to load full history {dim}(only last 30 days were synced){reset}"
        );
    }
    println!();
    println!("  {dim}Restart Claude Code and Cursor to activate hooks and the status line.{reset}");
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

    Ok(())
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

// ---------------------------------------------------------------------------
// Consolidated Claude Code settings.json installation
// ---------------------------------------------------------------------------

/// Install all Claude Code integrations in a single read-modify-write pass.
/// Handles: legacy hook cleanup, statusline, CC hooks, MCP server, OTEL env vars.
/// Returns a list of warning messages for any hooks that failed.
fn install_claude_code_settings(config: &config::BudiConfig) -> Vec<String> {
    let result = (|| -> Result<Vec<String>> {
        let home = budi_core::config::home_dir()?;
        let settings_path = home.join(super::statusline::CLAUDE_USER_SETTINGS);
        let mut settings = super::read_json_or_default(&settings_path)?;
        let warnings = Vec::new();

        // 1. Remove legacy hooks (old-style with subcommand args)
        if super::statusline::remove_legacy_budi_hooks_from_value(&mut settings) {
            eprintln!(
                "  Cleaned up legacy budi hooks from {}",
                settings_path.display()
            );
        }

        // 2. Statusline
        match apply_statusline(&mut settings) {
            Ok(true) => println!("  Status line: configured in {}", settings_path.display()),
            Ok(false) => {} // already installed, message printed inside
            Err(e) => {
                let yellow = super::ansi("\x1b[33m");
                let reset = super::ansi("\x1b[0m");
                eprintln!("{yellow}  Warning:{reset} status line install failed: {e}");
            }
        }

        // 3. Claude Code hooks
        let hooks_changed = apply_cc_hooks(&mut settings);
        if hooks_changed {
            println!(
                "  Hooks: installed Claude Code hooks in {}",
                settings_path.display()
            );
        } else {
            println!("  Hooks: Claude Code hooks already installed");
        }

        // 4. MCP server
        if apply_mcp_server(&mut settings) {
            println!(
                "  MCP: installed budi server in {}",
                settings_path.display()
            );
        } else {
            println!("  MCP: budi server already configured");
        }

        // 5. OTEL env vars
        apply_otel_env_vars(&mut settings, config, &settings_path);

        // Single atomic write
        super::atomic_write_json(&settings_path, &settings)?;

        Ok(warnings)
    })();

    match result {
        Ok(warnings) => warnings,
        Err(e) => {
            let yellow = super::ansi("\x1b[33m");
            let reset = super::ansi("\x1b[0m");
            eprintln!("{yellow}  Warning:{reset} Claude Code setup failed: {e}");
            vec![format!("Claude Code settings: {e}")]
        }
    }
}

/// Apply statusline configuration. Returns Ok(true) if changed, Ok(false) if already set.
fn apply_statusline(settings: &mut Value) -> Result<bool> {
    if let Some(existing) = settings.get("statusLine") {
        if !existing.is_object() {
            anyhow::bail!("statusLine is not an object — fix it manually before installing");
        }
        let existing_cmd = existing
            .get("command")
            .and_then(|c| c.as_str())
            .unwrap_or("");
        if existing_cmd.contains("budi statusline") || existing_cmd.contains("budi_out=$(budi") {
            return Ok(false);
        }
        let merged = format!(
            "{existing_cmd}{}",
            super::statusline::BUDI_STATUSLINE_SUFFIX
        );
        settings["statusLine"]["command"] = Value::String(merged);
        return Ok(true);
    }

    settings["statusLine"] = json!({
        "type": "command",
        "command": super::statusline::BUDI_STATUSLINE_CMD,
        "padding": 0
    });
    Ok(true)
}

/// The budi hook command string — same for all hook events.
/// Wrapped with `|| true` so the hook never blocks the host agent.
const BUDI_HOOK_CMD: &str = "budi hook 2>/dev/null || true";

/// Apply Claude Code hooks. Returns true if any changes were made.
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
            "command": BUDI_HOOK_CMD,
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

        // Remove legacy budi hooks (without || true wrapper) to re-add with the safe version
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

/// Apply MCP server configuration. Returns true if changed.
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

/// Apply OTEL env vars. Prints status messages directly.
fn apply_otel_env_vars(
    settings: &mut Value,
    config: &config::BudiConfig,
    settings_path: &std::path::Path,
) {
    let obj = match settings.as_object_mut() {
        Some(o) => o,
        None => return,
    };
    let env = obj.entry("env").or_insert_with(|| json!({}));
    if !env.is_object() {
        *env = json!({});
    }
    let env_obj = env.as_object_mut().unwrap();

    let budi_endpoint = format!("http://127.0.0.1:{}", config.daemon_port);

    // Check if user has a custom OTEL endpoint pointing elsewhere
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
            return;
        }
    }

    let otel_vars = [
        ("CLAUDE_CODE_ENABLE_TELEMETRY", "1"),
        ("OTEL_EXPORTER_OTLP_ENDPOINT", &budi_endpoint),
        ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json"),
        ("OTEL_METRICS_EXPORTER", "otlp"),
        ("OTEL_LOGS_EXPORTER", "otlp"),
    ];

    let mut changed = false;
    for (key, value) in &otel_vars {
        let current = env_obj.get(*key).and_then(|v| v.as_str());
        if current != Some(value) {
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
}

// ---------------------------------------------------------------------------
// Public wrappers for update.rs (read-modify-write individually)
// ---------------------------------------------------------------------------

/// Install OTEL telemetry env vars into ~/.claude/settings.json.
/// Standalone wrapper for use by `budi update`.
pub fn install_otel_env_vars(config: &config::BudiConfig) {
    let result = (|| -> Result<()> {
        let home = budi_core::config::home_dir()?;
        let settings_path = home.join(super::statusline::CLAUDE_USER_SETTINGS);
        let mut settings = super::read_json_or_default(&settings_path)?;
        apply_otel_env_vars(&mut settings, config, &settings_path);
        super::atomic_write_json(&settings_path, &settings)?;
        Ok(())
    })();

    if let Err(e) = result {
        let yellow = super::ansi("\x1b[33m");
        let reset = super::ansi("\x1b[0m");
        eprintln!("{yellow}  Warning:{reset} OTEL setup failed: {e}");
    }
}

/// Install the budi MCP server in ~/.claude/settings.json.
/// Standalone wrapper for use by `budi update`.
pub fn install_mcp_server() {
    let result = (|| -> Result<()> {
        let home = budi_core::config::home_dir()?;
        let settings_path = home.join(super::statusline::CLAUDE_USER_SETTINGS);
        let mut settings = super::read_json_or_default(&settings_path)?;
        if apply_mcp_server(&mut settings) {
            println!(
                "  MCP: installed budi server in {}",
                settings_path.display()
            );
        } else {
            println!("  MCP: budi server already configured");
        }
        super::atomic_write_json(&settings_path, &settings)?;
        Ok(())
    })();

    if let Err(e) = result {
        let yellow = super::ansi("\x1b[33m");
        let reset = super::ansi("\x1b[0m");
        eprintln!("{yellow}  Warning:{reset} MCP server setup failed: {e}");
    }
}

// ---------------------------------------------------------------------------
// Hook installation (Cursor — separate file)
// ---------------------------------------------------------------------------

/// Install hooks into ~/.cursor/hooks.json.
fn install_cursor_hooks() -> Result<()> {
    let home = budi_core::config::home_dir()?;
    let hooks_path = home.join(".cursor/hooks.json");

    if let Some(parent) = hooks_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }

    let mut config = if hooks_path.exists() {
        let raw = fs::read_to_string(&hooks_path)
            .with_context(|| format!("Failed to read {}", hooks_path.display()))?;
        serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| json!({"version": 1, "hooks": {}}))
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
        "command": BUDI_HOOK_CMD,
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

/// Detect and auto-remove duplicate budi binaries from the non-active install source.
///
/// Handles two cases:
/// - Active source is Homebrew → removes `~/.local/bin` copies and `.bak` files
/// - Active source is `~/.local/bin` → runs `brew uninstall budi` if installed
///
/// Skips cleanup for dev builds (binary not in a known install location).
pub(crate) fn clean_duplicate_binaries() {
    let Ok(path_var) = std::env::var("PATH") else {
        return;
    };
    let Ok(current_exe) = std::env::current_exe().and_then(|p| p.canonicalize()) else {
        return;
    };

    let has_duplicates = ["budi", "budi-daemon"].iter().any(|bin_name| {
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
        found.len() > 1
    });
    if !has_duplicates {
        clean_backup_files();
        return;
    }

    let exe_str = current_exe.to_string_lossy();
    let exe_lower = exe_str.to_lowercase();
    let is_brew = exe_lower.contains("/cellar/") || exe_lower.contains("/homebrew/");
    let is_standalone = exe_str.contains("/.local/bin/");

    let green = super::ansi("\x1b[32m");
    let yellow = super::ansi("\x1b[33m");
    let reset = super::ansi("\x1b[0m");

    if is_brew {
        if let Ok(home) = config::home_dir() {
            let bin_dir = home.join(".local").join("bin");
            if bin_dir.is_dir() {
                for name in &["budi", "budi-daemon"] {
                    let target = bin_dir.join(name);
                    if target.exists() {
                        let is_different = target
                            .canonicalize()
                            .map(|r| r != current_exe)
                            .unwrap_or(true);
                        if is_different {
                            match fs::remove_file(&target) {
                                Ok(()) => {
                                    eprintln!(
                                        "  {green}✓{reset} Removed stale standalone binary: {}",
                                        target.display()
                                    );
                                }
                                Err(e) => {
                                    eprintln!(
                                        "  {yellow}!{reset} Could not remove {}: {e}",
                                        target.display()
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    } else if is_standalone {
        if brew_has_budi() {
            eprintln!("  Removing Homebrew copy of budi...");
            let status = Command::new("brew")
                .args(["uninstall", "budi", "--force"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            match status {
                Ok(s) if s.success() => {
                    eprintln!("  {green}✓{reset} Uninstalled Homebrew copy of budi");
                }
                Ok(_) | Err(_) => {
                    eprintln!(
                        "  {yellow}!{reset} Could not uninstall Homebrew copy. \
                         Run `brew uninstall budi` manually."
                    );
                }
            }
        }
    } else {
        eprintln!(
            "{yellow}  Warning:{reset} multiple budi binaries found in PATH. \
             Remove stale copies to avoid version mismatch."
        );
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

// ---------------------------------------------------------------------------
// Legacy hook detection (for upgrading from old format without `|| true`)
// ---------------------------------------------------------------------------

/// Check if a Claude Code hook entry uses the old format (without `|| true` wrapper).
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

/// Check if a Cursor hook entry uses the old format (without `|| true` wrapper).
fn is_legacy_cursor_hook(entry: &Value) -> bool {
    entry
        .get("command")
        .and_then(|c| c.as_str())
        .is_some_and(|c| c.trim() == "budi hook")
}

// ---------------------------------------------------------------------------
// Cursor extension auto-install
// ---------------------------------------------------------------------------

static CURSOR_EXTENSION_VSIX: &[u8] =
    include_bytes!("../../../../extensions/cursor-budi/cursor-budi.vsix");

/// Install the budi Cursor extension if Cursor is available and the extension
/// is either missing or outdated. Non-fatal — prints a warning on failure.
fn install_cursor_extension() {
    if CURSOR_EXTENSION_VSIX.is_empty() {
        return;
    }

    let cursor_cli = match find_cursor_cli() {
        Some(c) => c,
        None => return,
    };

    if is_cursor_extension_installed_via(&cursor_cli) {
        println!("  Extension: Cursor extension already installed");
        return;
    }

    let tmp_dir = std::env::temp_dir().join(format!("budi-vsix-{}", std::process::id()));
    if fs::create_dir_all(&tmp_dir).is_err() {
        return;
    }

    let vsix_path = tmp_dir.join("cursor-budi.vsix");
    if let Err(e) = fs::write(&vsix_path, CURSOR_EXTENSION_VSIX) {
        eprintln!(
            "{}  Warning:{} could not write extension to temp: {e}",
            super::ansi("\x1b[33m"),
            super::ansi("\x1b[0m")
        );
        let _ = fs::remove_dir_all(&tmp_dir);
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

    let _ = fs::remove_dir_all(&tmp_dir);

    match result {
        Ok(status) if status.success() => {
            println!("  Extension: installed Cursor extension");
        }
        Ok(_) => {
            let yellow = super::ansi("\x1b[33m");
            let reset = super::ansi("\x1b[0m");
            eprintln!("{yellow}  Warning:{reset} Cursor extension install failed");
        }
        Err(e) => {
            let yellow = super::ansi("\x1b[33m");
            let reset = super::ansi("\x1b[0m");
            eprintln!("{yellow}  Warning:{reset} could not run cursor CLI: {e}");
        }
    }
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

/// Check if `siropkin.budi` extension is already installed in Cursor.
fn is_cursor_extension_installed_via(cursor_cli: &str) -> bool {
    Command::new(cursor_cli)
        .arg("--list-extensions")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            let out = String::from_utf8_lossy(&o.stdout);
            out.lines()
                .any(|l| l.trim().eq_ignore_ascii_case("siropkin.budi"))
        })
        .unwrap_or(false)
}

/// Check if the budi Cursor extension is installed.
/// Used by doctor and integrations endpoint.
pub fn is_cursor_extension_installed() -> bool {
    match find_cursor_cli() {
        Some(cli) => is_cursor_extension_installed_via(&cli),
        None => false,
    }
}
