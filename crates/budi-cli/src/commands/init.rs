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

    // Config defaults are fine without a repo root.
    let config = match &repo_root {
        Some(root) => {
            let cfg = config::load_or_default(root)?;
            config::ensure_repo_layout(root)?;
            config::save(root, &cfg)?;
            cfg
        }
        None => config::BudiConfig::default(),
    };

    warn_duplicate_binaries();
    check_daemon_binary();
    check_daemon_version();

    super::statusline::remove_legacy_hooks();
    install_statusline_if_missing();

    let hook_warnings = install_hooks();
    let had_hook_warnings = !hook_warnings.is_empty();

    install_mcp_server();
    install_otel_env_vars(&config);
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
    // On fresh install: creates tables. On upgrade: drops old schema, recreates.
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

    // Only open browser on fresh init (not re-init) and when --no-open is not set
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

fn install_statusline_if_missing() {
    let Ok(home) = budi_core::config::home_dir() else {
        return;
    };
    let settings_path = home.join(super::statusline::CLAUDE_USER_SETTINGS);

    match super::statusline::cmd_statusline_install() {
        Ok(()) => println!("  Status line: configured in {}", settings_path.display()),
        Err(e) => {
            let yellow = super::ansi("\x1b[33m");
            let reset = super::ansi("\x1b[0m");
            eprintln!("{yellow}  Warning:{reset} status line install failed: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Hook installation
// ---------------------------------------------------------------------------

/// The budi hook command string — same for all hook events.
/// Wrapped with `|| true` so the hook never blocks the host agent,
/// even if budi is not installed or crashes.
const BUDI_HOOK_CMD: &str = "budi hook 2>/dev/null || true";

/// Legacy hook command (without || true wrapper). Used for detection/cleanup.
const BUDI_HOOK_CMD_LEGACY: &str = "budi hook";

/// Install budi hooks for Claude Code and Cursor.
/// Merges with existing hooks — never overwrites non-budi entries.
/// Returns a list of warning messages for any hooks that failed to install.
fn install_hooks() -> Vec<String> {
    let mut warnings = Vec::new();
    if let Err(e) = install_claude_code_hooks() {
        warnings.push(format!("Claude Code hooks: {e}"));
    }
    if let Err(e) = install_cursor_hooks() {
        warnings.push(format!("Cursor hooks: {e}"));
    }
    warnings
}

/// Install hooks into ~/.claude/settings.json.
/// Uses Claude Code's nested format: hooks → EventName → [{ matcher, hooks: [{ type, command }] }]
fn install_claude_code_hooks() -> Result<()> {
    let home = budi_core::config::home_dir()?;
    let settings_path = home.join(super::statusline::CLAUDE_USER_SETTINGS);

    let mut settings = if settings_path.exists() {
        let raw = fs::read_to_string(&settings_path)
            .with_context(|| format!("Failed to read {}", settings_path.display()))?;
        serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    };
    if !settings.is_object() {
        settings = json!({});
    }

    let Some(hooks_obj) = settings.as_object_mut() else {
        anyhow::bail!("Claude Code settings is not a JSON object");
    };
    let hooks = hooks_obj.entry("hooks").or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }

    let cc_events = [
        "SessionStart",
        "SessionEnd",
        "PostToolUse",
        "SubagentStop",
        "PreCompact",
        "Stop",
        "UserPromptSubmit",
    ];

    let budi_hook_entry = json!({
        "matcher": "",
        "hooks": [{
            "type": "command",
            "command": BUDI_HOOK_CMD,
            "async": true
        }]
    });

    let mut changed = false;
    for event in &cc_events {
        let Some(hooks_map) = hooks.as_object_mut() else {
            anyhow::bail!("Claude Code hooks is not a JSON object");
        };
        let event_arr = hooks_map.entry(*event).or_insert_with(|| json!([]));
        if !event_arr.is_array() {
            *event_arr = json!([]);
        }

        // Remove any legacy budi hooks (without || true wrapper) so we can re-add with the safe version
        let Some(arr_mut) = event_arr.as_array_mut() else {
            continue;
        };
        let had_legacy = arr_mut.iter().any(is_legacy_cc_hook);
        if had_legacy {
            arr_mut.retain(|entry| !is_legacy_cc_hook(entry));
            changed = true;
        }

        // Check if a budi hook is already installed (normalize: strip whitespace, match core command)
        let already_installed = arr_mut.iter().any(|entry| {
            entry
                .get("hooks")
                .and_then(|h| h.as_array())
                .map(|hooks| {
                    hooks.iter().any(|h| {
                        h.get("command")
                            .and_then(|c| c.as_str())
                            .is_some_and(|c| c.trim().contains("budi hook"))
                    })
                })
                .unwrap_or(false)
        });

        if !already_installed {
            if let Some(arr) = event_arr.as_array_mut() {
                arr.push(budi_hook_entry.clone());
            }
            changed = true;
        }
    }

    if changed {
        let out = serde_json::to_string_pretty(&settings)?;
        fs::write(&settings_path, out)
            .with_context(|| format!("Failed to write {}", settings_path.display()))?;
        println!(
            "  Hooks: installed Claude Code hooks in {}",
            settings_path.display()
        );
    } else {
        println!("  Hooks: Claude Code hooks already installed");
    }
    Ok(())
}

/// Install hooks into ~/.cursor/hooks.json.
/// Uses Cursor's flat format: hooks → eventName → [{ command, type }]
fn install_cursor_hooks() -> Result<()> {
    let home = budi_core::config::home_dir()?;
    let hooks_path = home.join(".cursor/hooks.json");

    // Ensure directory exists
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

    let cursor_events = [
        "sessionStart",
        "sessionEnd",
        "postToolUse",
        "subagentStop",
        "preCompact",
        "stop",
        "afterFileEdit",
    ];

    let budi_hook_entry = json!({
        "command": BUDI_HOOK_CMD,
        "type": "command"
    });

    let mut changed = false;
    let Some(hooks) = config.get_mut("hooks") else {
        anyhow::bail!("Cursor hooks config missing hooks key");
    };

    for event in &cursor_events {
        let Some(hooks_map) = hooks.as_object_mut() else {
            anyhow::bail!("Cursor hooks is not a JSON object");
        };
        let event_arr = hooks_map.entry(*event).or_insert_with(|| json!([]));
        if !event_arr.is_array() {
            *event_arr = json!([]);
        }

        // Remove legacy hooks (without || true wrapper)
        let Some(arr_mut) = event_arr.as_array_mut() else {
            continue;
        };
        let had_legacy = arr_mut.iter().any(is_legacy_cursor_hook);
        if had_legacy {
            arr_mut.retain(|entry| !is_legacy_cursor_hook(entry));
            changed = true;
        }

        // Normalize: strip whitespace, match core command to avoid duplicates
        let already_installed = arr_mut.iter().any(|entry| {
            entry
                .get("command")
                .and_then(|c| c.as_str())
                .is_some_and(|c| c.trim().contains("budi hook"))
        });

        if !already_installed {
            if let Some(arr) = event_arr.as_array_mut() {
                arr.push(budi_hook_entry.clone());
            }
            changed = true;
        }
    }

    if changed {
        let out = serde_json::to_string_pretty(&config)?;
        fs::write(&hooks_path, out)
            .with_context(|| format!("Failed to write {}", hooks_path.display()))?;
        println!(
            "  Hooks: installed Cursor hooks in {}",
            hooks_path.display()
        );
    } else {
        println!("  Hooks: Cursor hooks already installed");
    }
    Ok(())
}

/// Check that budi-daemon is available on PATH before attempting to start it.
fn check_daemon_binary() {
    // Use a cross-platform check: try running the binary directly.
    let found = Command::new("budi-daemon")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !found {
        let yellow = super::ansi("\x1b[33m");
        let reset = super::ansi("\x1b[0m");
        eprintln!(
            "{yellow}  Warning:{reset} budi-daemon not found on PATH. \
             The daemon may fail to start."
        );
        eprintln!("  Ensure both budi and budi-daemon are installed in the same directory.");
    }
}

/// Warn if CLI and daemon versions don't match.
fn check_daemon_version() {
    let cli_version = env!("CARGO_PKG_VERSION");
    let daemon_version = Command::new("budi-daemon")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .strip_prefix("budi-daemon ")
                .unwrap_or(String::from_utf8_lossy(&o.stdout).trim())
                .to_string()
        });
    if let Some(ref dv) = daemon_version
        && dv != cli_version
    {
        let yellow = super::ansi("\x1b[33m");
        let reset = super::ansi("\x1b[0m");
        eprintln!(
            "{yellow}  Warning:{reset} version mismatch: CLI v{cli_version} but daemon v{dv}. \
                 Run `budi update` or reinstall both binaries."
        );
    }
}

/// Warn if there are multiple `budi` or `budi-daemon` binaries in PATH
/// (e.g. ~/.local/bin shadows Homebrew).
fn warn_duplicate_binaries() {
    let Ok(path_var) = std::env::var("PATH") else {
        return;
    };
    let Ok(current_exe) = std::env::current_exe().and_then(|p| p.canonicalize()) else {
        return;
    };

    for bin_name in &["budi", "budi-daemon"] {
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
            let yellow = super::ansi("\x1b[33m");
            let bold = super::ansi("\x1b[1m");
            let reset = super::ansi("\x1b[0m");
            eprintln!("{yellow}  Warning:{reset} multiple {bin_name} binaries found in PATH:");
            for path in &found {
                let marker = if *bin_name == "budi" && *path == current_exe {
                    " (active)"
                } else {
                    ""
                };
                eprintln!("    - {}{marker}", path.display());
            }
            eprintln!("  Remove the unused one to avoid version conflicts.");
            eprintln!(
                "  {bold}Tip:{reset} if you switched to Homebrew, run: rm ~/.local/bin/budi ~/.local/bin/budi-daemon"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// OTEL env var installation
// ---------------------------------------------------------------------------

/// Install OTEL telemetry env vars into ~/.claude/settings.json.
/// Merges with existing env — never overwrites user's custom OTEL endpoint
/// that points to a different host.
pub fn install_otel_env_vars(config: &config::BudiConfig) {
    let result = (|| -> Result<()> {
        let home = budi_core::config::home_dir()?;
        let settings_path = home.join(super::statusline::CLAUDE_USER_SETTINGS);

        let mut settings = if settings_path.exists() {
            let raw = fs::read_to_string(&settings_path)
                .with_context(|| format!("Failed to read {}", settings_path.display()))?;
            serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| json!({}))
        } else {
            json!({})
        };
        if !settings.is_object() {
            settings = json!({});
        }

        let obj = settings.as_object_mut().unwrap();
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
                return Ok(());
            }
            // Localhost but different port → update to budi's port (likely stale config)
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
            if let Some(parent) = settings_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let out = serde_json::to_string_pretty(&settings)?;
            fs::write(&settings_path, out)
                .with_context(|| format!("Failed to write {}", settings_path.display()))?;
            println!(
                "  OTEL: configured telemetry in {}",
                settings_path.display()
            );
        } else {
            println!("  OTEL: telemetry already configured");
        }

        Ok(())
    })();

    if let Err(e) = result {
        let yellow = super::ansi("\x1b[33m");
        let reset = super::ansi("\x1b[0m");
        eprintln!("{yellow}  Warning:{reset} OTEL setup failed: {e}");
    }
}

// ---------------------------------------------------------------------------
// MCP server installation
// ---------------------------------------------------------------------------

/// Install the budi MCP server in ~/.claude/settings.json.
/// Adds to mcpServers so Claude Code can discover and use budi tools.
fn install_mcp_server() {
    let result = (|| -> Result<()> {
        let home = budi_core::config::home_dir()?;
        let settings_path = home.join(super::statusline::CLAUDE_USER_SETTINGS);

        let mut settings = if settings_path.exists() {
            let raw = fs::read_to_string(&settings_path)
                .with_context(|| format!("Failed to read {}", settings_path.display()))?;
            serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| json!({}))
        } else {
            json!({})
        };
        if !settings.is_object() {
            settings = json!({});
        }

        let obj = settings.as_object_mut().unwrap();
        let mcp_servers = obj
            .entry("mcpServers")
            .or_insert_with(|| json!({}));
        if !mcp_servers.is_object() {
            *mcp_servers = json!({});
        }
        let mcp_obj = mcp_servers.as_object_mut().unwrap();

        // Resolve the budi binary path
        let budi_path = which_budi();

        let desired = json!({
            "command": budi_path,
            "args": ["mcp-serve"],
            "type": "stdio"
        });

        // Check if already installed with same config
        if mcp_obj.get("budi") == Some(&desired) {
            println!("  MCP: budi server already configured");
            return Ok(());
        }

        mcp_obj.insert("budi".to_string(), desired);

        if let Some(parent) = settings_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let out = serde_json::to_string_pretty(&settings)?;
        fs::write(&settings_path, out)
            .with_context(|| format!("Failed to write {}", settings_path.display()))?;
        println!(
            "  MCP: installed budi server in {}",
            settings_path.display()
        );
        Ok(())
    })();

    if let Err(e) = result {
        let yellow = super::ansi("\x1b[33m");
        let reset = super::ansi("\x1b[0m");
        eprintln!("{yellow}  Warning:{reset} MCP server setup failed: {e}");
    }
}

/// Find the budi binary path, preferring the same directory as the running binary.
fn which_budi() -> String {
    // Try to use the same directory as the running binary
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("budi");
            if candidate.exists() {
                return candidate.display().to_string();
            }
        }
    }
    // Fall back to PATH lookup
    "budi".to_string()
}

/// Check if a Claude Code hook entry is a legacy budi hook (without `|| true`).
fn is_legacy_cc_hook(entry: &Value) -> bool {
    entry
        .get("hooks")
        .and_then(|h| h.as_array())
        .map(|hooks| {
            hooks.iter().any(|h| {
                h.get("command").and_then(|c| c.as_str()).is_some_and(|c| {
                    let trimmed = c.trim();
                    trimmed == BUDI_HOOK_CMD_LEGACY && trimmed != BUDI_HOOK_CMD
                })
            })
        })
        .unwrap_or(false)
}

/// Check if a Cursor hook entry is a legacy budi hook (without `|| true`).
fn is_legacy_cursor_hook(entry: &Value) -> bool {
    entry
        .get("command")
        .and_then(|c| c.as_str())
        .is_some_and(|c| {
            let trimmed = c.trim();
            trimmed == BUDI_HOOK_CMD_LEGACY && trimmed != BUDI_HOOK_CMD
        })
}
