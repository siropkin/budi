use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use budi_core::installer_residue;
use budi_core::legacy_proxy;
use serde_json::Value;

use super::statusline::CLAUDE_USER_SETTINGS;

pub(crate) fn cmd_uninstall(keep_data: bool, yes: bool) -> Result<()> {
    let green = super::ansi("\x1b[32m");
    let yellow = super::ansi("\x1b[33m");
    let reset = super::ansi("\x1b[0m");

    // 1. Stop the daemon
    print!("Stopping daemon... ");
    match stop_daemon() {
        Ok(true) => println!("{green}✓{reset} stopped"),
        Ok(false) => println!("{yellow}not running{reset}"),
        Err(e) => println!("{yellow}warning: {e}{reset}"),
    }

    // 2. Remove hooks from Claude Code
    let home = budi_core::config::home_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    if home.is_empty() {
        eprintln!(
            "{yellow}warning: could not determine home directory — skipping hook/config cleanup{reset}"
        );
    } else {
        cleanup_legacy_proxy_residue(green, yellow, reset);

        // Remove the auto-installed `/budi` Claude Code skill (#603).
        print!("Removing /budi Claude Code skill... ");
        match remove_budi_skill() {
            Ok(true) => println!("{green}✓{reset} removed"),
            Ok(false) => println!("none found"),
            Err(e) => println!("{yellow}warning: {e}{reset}"),
        }

        // 2-6. Remove Claude Code integrations (single file pass)
        match remove_all_from_claude_code(&home) {
            Ok((hooks, otel, mcp, statusline)) => {
                let label = |removed| {
                    if removed {
                        format!("{green}✓{reset} removed")
                    } else {
                        "none found".to_string()
                    }
                };
                println!("Removing Claude Code hooks... {}", label(hooks));
                println!("Removing OTEL env vars... {}", label(otel));
                println!("Removing MCP server... {}", label(mcp));
                println!("Removing status line... {}", label(statusline));
            }
            Err(e) => {
                println!("Removing Claude Code integrations... {yellow}warning: {e}{reset}");
            }
        }

        // 3. Remove hooks from Cursor
        print!("Removing Cursor hooks... ");
        match remove_cursor_hooks(&home) {
            Ok(true) => {
                let hooks_path = PathBuf::from(&home).join(".cursor/hooks.json");
                if verify_no_budi_hooks_cursor(&hooks_path) {
                    println!("{green}✓{reset} removed (verified)");
                } else {
                    println!(
                        "{yellow}✓{reset} removed but some budi hooks may remain — check {}",
                        hooks_path.display()
                    );
                }
            }
            Ok(false) => println!("none found"),
            Err(e) => println!("{yellow}warning: {e}{reset}"),
        }

        // 7. Remove data
        if !keep_data {
            if !yes {
                if !std::io::stdin().is_terminal() {
                    anyhow::bail!(
                        "Non-interactive terminal. Use `budi uninstall --yes` to skip confirmation."
                    );
                }
                eprint!("Remove all analytics data and config? This cannot be undone. [y/N] ");
                let mut answer = String::new();
                std::io::stdin()
                    .read_line(&mut answer)
                    .context("Failed to read stdin")?;
                if !matches!(answer.trim(), "y" | "Y") {
                    println!("Keeping data.");
                    print_binary_removal_hint();
                    return Ok(());
                }
            }
            match remove_data() {
                Ok(report) => print_data_removal(&report, green, yellow, reset),
                Err(e) => println!("Removing data... {yellow}warning: {e}{reset}"),
            }
            print!("Removing config... ");
            match remove_config() {
                Ok(true) => println!("{green}✓{reset} removed"),
                Ok(false) => println!("none found"),
                Err(e) => println!("{yellow}warning: {e}{reset}"),
            }
        } else {
            println!("Keeping data and config (--keep-data)");
        }
    }

    // 8. Remove autostart service (launchd / systemd / Task Scheduler)
    {
        let mechanism = budi_core::autostart::service_mechanism();
        print!("Removing autostart service ({mechanism})... ");
        match budi_core::autostart::uninstall_service() {
            Ok(true) => println!("{green}✓{reset} removed"),
            Ok(false) => println!("none found"),
            Err(e) => println!("{yellow}warning: {e}{reset}"),
        }
    }

    // 9. Remove the autostart service log (macOS only — launchd writes
    // StandardOutPath/StandardErrorPath outside the data dir).
    if let Some(log_path) = budi_core::autostart::service_log_path() {
        print!("Removing daemon log ({})... ", log_path.display());
        match remove_file_if_exists(&log_path) {
            Ok(true) => println!("{green}✓{reset} removed"),
            Ok(false) => println!("none found"),
            Err(e) => println!("{yellow}warning: {e}{reset}"),
        }
    }

    // 10. Remove the `# Added by budi installer` block the standalone shell
    // installer writes to the user's shell profile. Consent-first: on an
    // interactive run we show the diff and ask per file unless --yes.
    remove_installer_shell_residue(yes, green, yellow, reset)?;

    println!();
    println!("{green}✓{reset} budi uninstalled.");
    println!();
    let bold = super::ansi("\x1b[1m");
    println!("{bold}Important:{reset} Binaries are still installed. Remove them manually:");
    print_binary_removal_hint();

    Ok(())
}

fn cleanup_legacy_proxy_residue(green: &str, yellow: &str, reset: &str) {
    print!("Removing legacy 8.0/8.1 proxy residue... ");
    let scan = match legacy_proxy::scan() {
        Ok(scan) => scan,
        Err(e) => {
            println!("{yellow}warning: {e}{reset}");
            return;
        }
    };

    let mut removed_paths = Vec::new();
    for file in scan.files.iter().filter(|file| file.has_managed_blocks()) {
        match file.apply_cleanup() {
            Ok(true) => removed_paths.push(file.path.display().to_string()),
            Ok(false) => {}
            Err(e) => {
                println!("{yellow}warning: {e}{reset}");
                return;
            }
        }
    }

    if removed_paths.is_empty() {
        println!("none found");
    } else {
        println!("{green}✓{reset} removed from {}", removed_paths.join(", "));
    }

    if scan.total_fuzzy_findings() > 0 {
        println!(
            "  {yellow}warning:{reset} manual edits still reference the old proxy and were not auto-removed:"
        );
        for file in scan.files.iter().filter(|file| file.has_fuzzy_findings()) {
            println!("    {}:", file.path.display());
            for finding in &file.fuzzy_findings {
                println!(
                    "      line {} ({}) {}",
                    finding.line_number, finding.label, finding.snippet
                );
            }
        }
    }

    if !scan.exported_env_vars.is_empty() {
        let rendered = scan
            .exported_env_vars
            .iter()
            .map(|entry| format!("{}={}", entry.key, entry.value))
            .collect::<Vec<_>>()
            .join(", ");
        println!("  Current shell still exports legacy proxy env vars: {rendered}");
        println!(
            "  Open a fresh terminal if you want the current session to drop those values too."
        );
    }
}

fn stop_daemon() -> Result<bool> {
    if cfg!(target_os = "windows") {
        let output = Command::new("taskkill")
            .args(["/F", "/IM", "budi-daemon.exe"])
            .output()
            .context("failed to run taskkill")?;
        Ok(output.status.success())
    } else {
        let output = Command::new("pkill")
            .args(["-f", "budi-daemon"])
            .output()
            .context("failed to run pkill")?;
        Ok(output.status.success())
    }
}

/// Remove all budi integrations from ~/.claude/settings.json in a single read-modify-write pass.
/// Returns (hooks_removed, otel_removed, mcp_removed, statusline_removed).
fn remove_all_from_claude_code(home: &str) -> Result<(bool, bool, bool, bool)> {
    let settings_path = PathBuf::from(home).join(CLAUDE_USER_SETTINGS);
    if !settings_path.exists() {
        return Ok((false, false, false, false));
    }

    let mut settings = super::read_json_or_default(&settings_path)?;

    // 1. Remove hooks
    let hooks_removed = {
        let mut changed = false;
        if let Some(hooks) = settings.get_mut("hooks").and_then(|h| h.as_object_mut()) {
            let events: Vec<String> = hooks.keys().cloned().collect();
            for event in events {
                if let Some(arr) = hooks.get_mut(&event).and_then(|v| v.as_array_mut()) {
                    let before = arr.len();
                    arr.retain(|entry| !super::is_budi_cc_hook_entry(entry));
                    if arr.len() < before {
                        changed = true;
                    }
                    if arr.is_empty() {
                        hooks.remove(&event);
                    }
                }
            }
            if hooks.is_empty()
                && let Some(obj) = settings.as_object_mut()
            {
                obj.remove("hooks");
            }
        }
        changed
    };

    // 2. Remove OTEL env vars
    let otel_removed = {
        let mut changed = false;
        let is_budi_endpoint = settings
            .get("env")
            .and_then(|e| e.as_object())
            .is_some_and(|env| {
                let endpoint_local = env
                    .get("OTEL_EXPORTER_OTLP_ENDPOINT")
                    .and_then(|v| v.as_str())
                    .is_some_and(|url| {
                        let lower = url.to_lowercase();
                        lower.contains("127.0.0.1") || lower.contains("localhost")
                    });
                endpoint_local
                    && env
                        .get("CLAUDE_CODE_ENABLE_TELEMETRY")
                        .and_then(|v| v.as_str())
                        == Some("1")
                    && env
                        .get("OTEL_EXPORTER_OTLP_PROTOCOL")
                        .and_then(|v| v.as_str())
                        == Some("http/json")
            });
        if is_budi_endpoint
            && let Some(env) = settings.get_mut("env").and_then(|e| e.as_object_mut())
        {
            for key in &[
                "CLAUDE_CODE_ENABLE_TELEMETRY",
                "OTEL_EXPORTER_OTLP_ENDPOINT",
                "OTEL_EXPORTER_OTLP_PROTOCOL",
                "OTEL_METRICS_EXPORTER",
                "OTEL_LOGS_EXPORTER",
            ] {
                if env.remove(*key).is_some() {
                    changed = true;
                }
            }
            if env.is_empty()
                && let Some(obj) = settings.as_object_mut()
            {
                obj.remove("env");
            }
        }
        changed
    };

    // 3. Remove MCP server
    let mcp_removed = {
        let removed = settings
            .get_mut("mcpServers")
            .and_then(|m| m.as_object_mut())
            .and_then(|mcp| mcp.remove("budi"))
            .is_some();
        if removed
            && let Some(mcp) = settings.get("mcpServers").and_then(|m| m.as_object())
            && mcp.is_empty()
            && let Some(obj) = settings.as_object_mut()
        {
            obj.remove("mcpServers");
        }
        removed
    };

    // 4. Remove statusline
    let statusline_removed = settings
        .as_object_mut()
        .and_then(|obj| obj.remove("statusLine"))
        .is_some();

    if hooks_removed || otel_removed || mcp_removed || statusline_removed {
        super::atomic_write_json(&settings_path, &settings)?;
    }

    Ok((hooks_removed, otel_removed, mcp_removed, statusline_removed))
}

/// Remove `~/.claude/skills/budi/SKILL.md` (and the empty `budi/` dir
/// when it has no other entries). Idempotent — returns `false` when
/// the file did not exist.
fn remove_budi_skill() -> Result<bool> {
    let skill_path = match super::integrations::claude_budi_skill_path() {
        Ok(p) => p,
        Err(_) => return Ok(false),
    };
    if !skill_path.exists() {
        return Ok(false);
    }
    fs::remove_file(&skill_path)
        .with_context(|| format!("Failed to remove {}", skill_path.display()))?;
    if let Some(parent) = skill_path.parent()
        && parent.exists()
        && fs::read_dir(parent)
            .map(|mut iter| iter.next().is_none())
            .unwrap_or(false)
    {
        let _ = fs::remove_dir(parent);
    }
    Ok(true)
}

fn remove_cursor_hooks(home: &str) -> Result<bool> {
    let hooks_path = PathBuf::from(home).join(".cursor/hooks.json");
    if !hooks_path.exists() {
        return Ok(false);
    }

    let mut config = super::read_json_or_default(&hooks_path)?;

    let Some(hooks) = config.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
        return Ok(false);
    };

    let mut changed = false;
    let events: Vec<String> = hooks.keys().cloned().collect();
    for event in events {
        let Some(arr) = hooks.get_mut(&event).and_then(|v| v.as_array_mut()) else {
            continue;
        };

        let before = arr.len();
        arr.retain(|entry| !super::is_budi_cursor_hook_entry(entry));
        if arr.len() < before {
            changed = true;
        }

        if arr.is_empty() {
            hooks.remove(&event);
        }
    }

    if changed {
        super::atomic_write_json(&hooks_path, &config)?;
    }

    Ok(changed)
}

/// Per-entry summary of what was removed from the data dir.
#[derive(Debug, Default)]
struct DataRemovalReport {
    data_dir: Option<PathBuf>,
    /// Known contract files that existed and were removed (pretty names).
    named_items: Vec<String>,
    /// Count of extra entries beyond the named contract set.
    extra_entry_count: usize,
}

impl DataRemovalReport {
    fn missing(&self) -> bool {
        self.data_dir.is_none()
    }

    fn total_entries(&self) -> usize {
        self.named_items.len() + self.extra_entry_count
    }
}

fn remove_data() -> Result<DataRemovalReport> {
    let data_dir = budi_core::config::budi_home_dir()?;
    if !data_dir.exists() {
        return Ok(DataRemovalReport::default());
    }

    let inventory = inventory_data_dir(&data_dir);

    fs::remove_dir_all(&data_dir)
        .with_context(|| format!("Failed to remove {}", data_dir.display()))?;

    Ok(DataRemovalReport {
        data_dir: Some(data_dir),
        named_items: inventory.named_items,
        extra_entry_count: inventory.extra_entry_count,
    })
}

struct DataInventory {
    named_items: Vec<String>,
    extra_entry_count: usize,
}

/// Walk the data dir once and classify its top-level entries against the
/// documented ADR-0083 / ADR-0086 contract. The caller deletes the dir
/// afterwards — this inventory is purely so the CLI can print a
/// human-readable enumeration of what was removed.
fn inventory_data_dir(data_dir: &Path) -> DataInventory {
    let entries = match fs::read_dir(data_dir) {
        Ok(entries) => entries,
        Err(_) => {
            return DataInventory {
                named_items: Vec::new(),
                extra_entry_count: 0,
            };
        }
    };

    let mut db_file = false;
    let mut db_sidecar_count = 0usize;
    let mut repos_subdir_count: Option<usize> = None;
    let mut has_cursor_sessions = false;
    let mut has_pricing_json = false;
    let mut has_upgrade_flags = false;
    let mut extra = 0usize;

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy().to_string();
        match name_str.as_str() {
            "analytics.db" => db_file = true,
            "analytics.db-shm" | "analytics.db-wal" => db_sidecar_count += 1,
            "cursor-sessions.json" => has_cursor_sessions = true,
            "pricing.json" => has_pricing_json = true,
            "repos" => {
                let count = fs::read_dir(entry.path())
                    .map(|iter| iter.flatten().count())
                    .unwrap_or(0);
                repos_subdir_count = Some(count);
            }
            "upgrade-flags" => has_upgrade_flags = true,
            _ => extra += 1,
        }
    }

    let mut named_items = Vec::new();
    if db_file {
        let label = if db_sidecar_count > 0 {
            format!("analytics.db (+{} sidecar)", db_sidecar_count)
        } else {
            "analytics.db".to_string()
        };
        named_items.push(label);
    } else if db_sidecar_count > 0 {
        named_items.push(format!("analytics.db sidecar ({db_sidecar_count})"));
    }
    if let Some(count) = repos_subdir_count {
        let word = if count == 1 { "repo" } else { "repos" };
        named_items.push(format!("repos/ ({count} {word})"));
    }
    if has_cursor_sessions {
        named_items.push("cursor-sessions.json".to_string());
    }
    if has_pricing_json {
        named_items.push("pricing.json".to_string());
    }
    if has_upgrade_flags {
        named_items.push("upgrade-flags/".to_string());
    }

    DataInventory {
        named_items,
        extra_entry_count: extra,
    }
}

fn print_data_removal(report: &DataRemovalReport, green: &str, yellow: &str, reset: &str) {
    if report.missing() {
        println!("Removing data... none found");
        return;
    }
    let Some(dir) = &report.data_dir else {
        return;
    };

    let total = report.total_entries();
    if total == 0 {
        println!(
            "Removing data... {green}✓{reset} removed empty {}",
            dir.display()
        );
        return;
    }

    println!(
        "Removing data... {green}✓{reset} removed {} from {}",
        pluralize_items(total),
        dir.display()
    );
    for item in &report.named_items {
        println!("    - {item}");
    }
    if report.extra_entry_count > 0 {
        println!(
            "    - {yellow}+{}{reset} other entr{}",
            report.extra_entry_count,
            if report.extra_entry_count == 1 {
                "y"
            } else {
                "ies"
            }
        );
    }
}

fn pluralize_items(count: usize) -> String {
    if count == 1 {
        "1 item".to_string()
    } else {
        format!("{count} items")
    }
}

fn remove_file_if_exists(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    fs::remove_file(path).with_context(|| format!("Failed to remove {}", path.display()))?;
    Ok(true)
}

fn remove_installer_shell_residue(yes: bool, green: &str, yellow: &str, reset: &str) -> Result<()> {
    let scan = match installer_residue::scan() {
        Ok(scan) => scan,
        Err(e) => {
            println!("Removing installer PATH block... {yellow}warning: {e}{reset}");
            return Ok(());
        }
    };

    if !scan.has_residue() {
        println!("Removing installer PATH block... none found");
        return Ok(());
    }

    println!("Removing installer PATH block:");

    let interactive = std::io::stdin().is_terminal();
    for residue in &scan.files {
        println!("  {}:", residue.path.display());
        for line in &residue.removed_lines {
            println!("    - L{}: {}", line.line_number, line.content);
        }

        let apply = if yes {
            true
        } else if !interactive {
            println!("    {yellow}skipped{reset} (non-interactive; rerun with --yes to remove)");
            false
        } else {
            prompt_confirm(&format!(
                "  Remove these lines from {}? [y/N] ",
                residue.path.display()
            ))?
        };

        if !apply {
            continue;
        }

        match residue.apply_cleanup() {
            Ok(true) => println!("    {green}✓{reset} removed"),
            Ok(false) => println!("    (no change)"),
            Err(e) => println!("    {yellow}warning: {e}{reset}"),
        }
    }
    Ok(())
}

fn prompt_confirm(prompt: &str) -> Result<bool> {
    eprint!("{prompt}");
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .context("Failed to read stdin")?;
    Ok(matches!(answer.trim(), "y" | "Y"))
}

fn remove_config() -> Result<bool> {
    let config_dir = budi_core::config::budi_config_dir()?;
    if !config_dir.exists() {
        return Ok(false);
    }
    fs::remove_dir_all(&config_dir)?;
    Ok(true)
}

fn print_binary_removal_hint() {
    println!("To remove the binaries:");
    if cfg!(target_os = "windows") {
        let bin_dir = std::env::var("LOCALAPPDATA")
            .map(|d| format!("{}\\budi\\bin", d))
            .unwrap_or_else(|_| "%LOCALAPPDATA%\\budi\\bin".to_string());
        println!("  # If installed via PowerShell:");
        println!("  Remove-Item -Recurse -Force \"{}\"", bin_dir);
    } else {
        println!("  # If installed via Homebrew:");
        println!("  brew uninstall budi");
        println!("  # If installed via shell script:");
        println!("  rm ~/.local/bin/budi ~/.local/bin/budi-daemon");
    }
}

/// Re-read Cursor hooks and confirm no budi hooks remain.
fn verify_no_budi_hooks_cursor(path: &PathBuf) -> bool {
    let Ok(raw) = fs::read_to_string(path) else {
        return true;
    };
    let Ok(config) = serde_json::from_str::<Value>(&raw) else {
        return false;
    };
    let Some(hooks) = config.get("hooks").and_then(|h| h.as_object()) else {
        return true;
    };
    !hooks.values().any(|arr| {
        arr.as_array()
            .map(|a| a.iter().any(super::is_budi_cursor_hook_entry))
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_dir(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "budi-uninstall-test-{name}-{}-{stamp}",
            std::process::id()
        ))
    }

    #[test]
    fn inventory_enumerates_known_contract_files() {
        let dir = unique_temp_dir("inventory-known");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("analytics.db"), b"db").unwrap();
        fs::write(dir.join("analytics.db-shm"), b"shm").unwrap();
        fs::write(dir.join("analytics.db-wal"), b"wal").unwrap();
        fs::write(dir.join("cursor-sessions.json"), b"{}").unwrap();
        fs::write(dir.join("pricing.json"), b"{}").unwrap();
        fs::create_dir_all(dir.join("repos/repo-a")).unwrap();
        fs::create_dir_all(dir.join("repos/repo-b")).unwrap();
        fs::create_dir_all(dir.join("upgrade-flags")).unwrap();

        let inv = inventory_data_dir(&dir);
        assert_eq!(inv.extra_entry_count, 0);
        assert!(
            inv.named_items
                .iter()
                .any(|s| s.starts_with("analytics.db"))
        );
        assert!(inv.named_items.iter().any(|s| s.starts_with("repos/ (2")));
        assert!(inv.named_items.iter().any(|s| s == "cursor-sessions.json"));
        assert!(inv.named_items.iter().any(|s| s == "pricing.json"));
        assert!(inv.named_items.iter().any(|s| s == "upgrade-flags/"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn inventory_counts_unknown_entries_separately() {
        let dir = unique_temp_dir("inventory-unknown");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("analytics.db"), b"db").unwrap();
        fs::write(dir.join("stray.tmp"), b"x").unwrap();
        fs::write(dir.join("other-leftover"), b"y").unwrap();

        let inv = inventory_data_dir(&dir);
        assert_eq!(inv.extra_entry_count, 2);
        assert!(
            inv.named_items
                .iter()
                .any(|s| s.starts_with("analytics.db"))
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn remove_file_if_exists_is_idempotent() {
        let dir = unique_temp_dir("remove-file");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("daemon.log");
        fs::write(&path, b"hi").unwrap();

        assert!(remove_file_if_exists(&path).unwrap());
        assert!(!remove_file_if_exists(&path).unwrap());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pluralize_items_matches_english() {
        assert_eq!(pluralize_items(1), "1 item");
        assert_eq!(pluralize_items(2), "2 items");
        assert_eq!(pluralize_items(0), "0 items");
    }
}
