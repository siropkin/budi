use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use budi_core::config;

const SHELL_BLOCK_START: &str = "# >>> budi >>>";
const SHELL_BLOCK_END: &str = "# <<< budi <<<";
const CURSOR_BLOCK_START: &str = "// >>> budi >>>";
const CURSOR_BLOCK_END: &str = "// <<< budi <<<";

const CURSOR_OPENAI_BASE_URL_KEY: &str = "openai.baseUrl";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManagedAgent {
    Claude,
    Codex,
    Cursor,
    Copilot,
}

impl ManagedAgent {
    fn parse(value: &str) -> Result<Self> {
        let lower = value.trim().to_ascii_lowercase();
        match lower.as_str() {
            "claude" | "claude-code" => Ok(Self::Claude),
            "codex" | "codex-cli" | "codex-desktop" => Ok(Self::Codex),
            "cursor" => Ok(Self::Cursor),
            "copilot" | "copilot-cli" => Ok(Self::Copilot),
            "gemini" | "gemini-cli" => {
                anyhow::bail!("Gemini CLI is deferred in ADR-0082 (Tier 3) and not available yet")
            }
            _ => {
                anyhow::bail!("Unknown agent '{value}'. Supported: claude, codex, cursor, copilot")
            }
        }
    }

    fn display_name(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
            Self::Cursor => "Cursor",
            Self::Copilot => "Copilot CLI",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncState {
    Configured,
    Removed,
    Unchanged,
}

#[derive(Debug)]
struct BlockUpdateResult {
    changed: bool,
    had_block: bool,
    has_block: bool,
}

pub fn cmd_enable(agent_name: &str) -> Result<()> {
    set_agent_enabled(agent_name, true)
}

pub fn cmd_disable(agent_name: &str) -> Result<()> {
    set_agent_enabled(agent_name, false)
}

fn set_agent_enabled(agent_name: &str, enabled: bool) -> Result<()> {
    let managed = ManagedAgent::parse(agent_name)?;
    let mut agents = config::load_agents_config().unwrap_or_else(config::AgentsConfig::all_enabled);

    set_agent_flag(&mut agents, managed, enabled);
    config::save_agents_config(&agents)?;

    let repo_root = super::try_resolve_repo_root(None);
    let cfg = match &repo_root {
        Some(root) => config::load_or_default(root)?,
        None => config::BudiConfig::default(),
    };

    let warnings = apply_auto_proxy_configuration(&agents, cfg.proxy.effective_port());

    let action = if enabled { "enabled" } else { "disabled" };
    println!("{} {action}.", managed.display_name());
    if warnings.is_empty() {
        println!("Auto-proxy configuration updated.");
    } else {
        println!("Auto-proxy configuration updated with warnings:");
        for warning in warnings {
            println!("  - {warning}");
        }
    }
    Ok(())
}

fn set_agent_flag(agents: &mut config::AgentsConfig, managed: ManagedAgent, enabled: bool) {
    match managed {
        ManagedAgent::Claude => agents.claude_code.enabled = enabled,
        ManagedAgent::Codex => agents.codex_cli.enabled = enabled,
        ManagedAgent::Cursor => agents.cursor.enabled = enabled,
        ManagedAgent::Copilot => agents.copilot_cli.enabled = enabled,
    }
}

pub fn apply_auto_proxy_configuration(
    agents: &config::AgentsConfig,
    proxy_port: u16,
) -> Vec<String> {
    let mut warnings = Vec::new();

    let home = match config::home_dir() {
        Ok(path) => path,
        Err(e) => {
            warnings.push(format!("could not resolve home directory: {e}"));
            return warnings;
        }
    };

    let proxy_url = format!("http://localhost:{proxy_port}");

    match sync_shell_profile(&home, agents, &proxy_url) {
        Ok((path, SyncState::Configured)) => {
            println!(
                "  Proxy: configured CLI agent env vars in {}",
                path.display()
            );
        }
        Ok((path, SyncState::Removed)) => {
            println!(
                "  Proxy: removed CLI agent env vars from {}",
                path.display()
            );
        }
        Ok((_path, SyncState::Unchanged)) => {}
        Err(e) => warnings.push(format!("shell profile: {e}")),
    }

    match sync_cursor_settings(&home, agents.cursor.enabled, &proxy_url) {
        Ok((path, SyncState::Configured)) => {
            println!("  Proxy: configured Cursor base URL in {}", path.display());
        }
        Ok((path, SyncState::Removed)) => {
            println!(
                "  Proxy: removed Cursor proxy config from {}",
                path.display()
            );
        }
        Ok((_path, SyncState::Unchanged)) => {}
        Err(e) => warnings.push(format!("Cursor settings: {e}")),
    }

    match sync_codex_config(&home, agents.codex_cli.enabled, &proxy_url) {
        Ok((path, SyncState::Configured)) => {
            println!("  Proxy: configured Codex base URL in {}", path.display());
        }
        Ok((path, SyncState::Removed)) => {
            println!(
                "  Proxy: removed Codex proxy config from {}",
                path.display()
            );
        }
        Ok((_path, SyncState::Unchanged)) => {}
        Err(e) => warnings.push(format!("Codex config: {e}")),
    }

    warnings
}

pub fn doctor_auto_proxy_issues(agents: &config::AgentsConfig, proxy_port: u16) -> Vec<String> {
    let mut issues = Vec::new();

    let home = match config::home_dir() {
        Ok(path) => path,
        Err(e) => {
            issues.push(format!(
                "Could not resolve home directory for auto-proxy checks: {e}"
            ));
            return issues;
        }
    };

    let proxy_url = format!("http://localhost:{proxy_port}");

    let has_cli_agents =
        agents.claude_code.enabled || agents.codex_cli.enabled || agents.copilot_cli.enabled;

    match detect_shell_profile_path(&home) {
        Some(path) => {
            let raw = fs::read_to_string(&path).unwrap_or_default();
            let block = extract_block(&raw, SHELL_BLOCK_START, SHELL_BLOCK_END);
            if has_cli_agents {
                if let Some(block_text) = block {
                    if agents.claude_code.enabled && !block_text.contains("ANTHROPIC_BASE_URL") {
                        issues.push(format!(
                            "Claude proxy env var missing in {}. Run `budi enable claude`.",
                            path.display()
                        ));
                    }
                    if agents.codex_cli.enabled && !block_text.contains("OPENAI_BASE_URL") {
                        issues.push(format!(
                            "Codex proxy env var missing in {}. Run `budi enable codex`.",
                            path.display()
                        ));
                    }
                    if agents.copilot_cli.enabled
                        && (!block_text.contains("COPILOT_PROVIDER_BASE_URL")
                            || !block_text.contains("COPILOT_PROVIDER_TYPE"))
                    {
                        issues.push(format!(
                            "Copilot proxy env vars missing in {}. Run `budi enable copilot`.",
                            path.display()
                        ));
                    }
                } else {
                    issues.push(format!(
                        "Auto-proxy shell block missing in {}. Run `budi init`.",
                        path.display()
                    ));
                }
            } else if block.is_some() {
                issues.push(format!(
                    "Shell profile {} still has a budi proxy block but no CLI agents are enabled.",
                    path.display()
                ));
            }
        }
        None => {
            if has_cli_agents {
                issues.push(
                    "Could not detect a zsh/bash shell profile for CLI auto-proxy env vars."
                        .to_string(),
                );
            }
        }
    }

    let codex_path = codex_config_path(&home);
    let codex_raw = fs::read_to_string(&codex_path).unwrap_or_default();
    let codex_block = extract_block(&codex_raw, SHELL_BLOCK_START, SHELL_BLOCK_END);
    if agents.codex_cli.enabled {
        if let Some(block_text) = codex_block {
            if !block_text.contains(&format!("openai_base_url = \"{proxy_url}\"")) {
                issues.push(format!(
                    "Codex proxy URL is not set to {} in {}.",
                    proxy_url,
                    codex_path.display()
                ));
            }
        } else {
            issues.push(format!(
                "Codex proxy config missing in {}. Run `budi enable codex`.",
                codex_path.display()
            ));
        }
    } else if codex_block.is_some() {
        issues.push(format!(
            "Codex config {} still has budi proxy settings while Codex is disabled.",
            codex_path.display()
        ));
    }

    let cursor_path = resolve_cursor_settings_path(&home);
    let cursor_raw = fs::read_to_string(&cursor_path).unwrap_or_default();
    let cursor_block = extract_block(&cursor_raw, CURSOR_BLOCK_START, CURSOR_BLOCK_END);
    if agents.cursor.enabled {
        if let Some(block_text) = cursor_block {
            if !block_text.contains(&format!(
                "\"{CURSOR_OPENAI_BASE_URL_KEY}\": \"{proxy_url}\""
            )) {
                issues.push(format!(
                    "Cursor base URL is not set to {} in {}.",
                    proxy_url,
                    cursor_path.display()
                ));
            }
        } else {
            issues.push(format!(
                "Cursor proxy config missing in {}. Run `budi enable cursor`.",
                cursor_path.display()
            ));
        }
    } else if cursor_block.is_some() {
        issues.push(format!(
            "Cursor settings {} still has budi proxy settings while Cursor is disabled.",
            cursor_path.display()
        ));
    }

    issues
}

fn sync_shell_profile(
    home: &Path,
    agents: &config::AgentsConfig,
    proxy_url: &str,
) -> Result<(PathBuf, SyncState)> {
    let path = detect_shell_profile_path(home).ok_or_else(|| {
        anyhow::anyhow!("could not detect ~/.zshrc, ~/.bashrc, or ~/.bash_profile")
    })?;

    let mut lines = Vec::new();
    if agents.claude_code.enabled {
        lines.push(format!("  export ANTHROPIC_BASE_URL=\"{proxy_url}\""));
        lines.push("  export CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1".to_string());
    }
    if agents.codex_cli.enabled {
        lines.push(format!("  export OPENAI_BASE_URL=\"{proxy_url}\""));
    }
    if agents.copilot_cli.enabled {
        lines.push(format!(
            "  export COPILOT_PROVIDER_BASE_URL=\"{proxy_url}\""
        ));
        lines.push("  export COPILOT_PROVIDER_TYPE=\"openai\"".to_string());
    }

    let block = if lines.is_empty() {
        None
    } else {
        let mut block = String::new();
        block.push_str(SHELL_BLOCK_START);
        block.push('\n');
        block
            .push_str("# Do not edit this block manually. Use `budi disable <agent>` to remove.\n");
        block.push_str("if [ \"${BUDI_BYPASS:-0}\" != \"1\" ]; then\n");
        for line in lines {
            block.push_str(&line);
            block.push('\n');
        }
        block.push_str("fi\n");
        block.push_str(SHELL_BLOCK_END);
        Some(block)
    };

    let updated =
        update_text_file_block(&path, SHELL_BLOCK_START, SHELL_BLOCK_END, block.as_deref())?;
    Ok((
        path,
        match (updated.had_block, updated.has_block) {
            (_, true) if updated.changed => SyncState::Configured,
            (true, false) if updated.changed => SyncState::Removed,
            _ => SyncState::Unchanged,
        },
    ))
}

fn sync_codex_config(home: &Path, enabled: bool, proxy_url: &str) -> Result<(PathBuf, SyncState)> {
    let path = codex_config_path(home);

    let block = if enabled {
        Some(format!(
            "{SHELL_BLOCK_START}\n# Managed by budi. Use `budi disable codex` to remove.\nopenai_base_url = \"{proxy_url}\"\n{SHELL_BLOCK_END}"
        ))
    } else {
        None
    };

    let updated =
        update_text_file_block(&path, SHELL_BLOCK_START, SHELL_BLOCK_END, block.as_deref())?;
    Ok((
        path,
        match (updated.had_block, updated.has_block) {
            (_, true) if updated.changed => SyncState::Configured,
            (true, false) if updated.changed => SyncState::Removed,
            _ => SyncState::Unchanged,
        },
    ))
}

fn sync_cursor_settings(
    home: &Path,
    enabled: bool,
    proxy_url: &str,
) -> Result<(PathBuf, SyncState)> {
    let path = resolve_cursor_settings_path(home);

    let raw = fs::read_to_string(&path).unwrap_or_else(|_| "{}\n".to_string());
    let (without_block, had_block) =
        strip_managed_block(&raw, CURSOR_BLOCK_START, CURSOR_BLOCK_END)?;

    let mut cleaned = if had_block {
        trim_trailing_comma_before_closing_brace(&without_block)
    } else {
        without_block
    };

    let has_block = enabled;
    if enabled {
        let block = format!(
            "  {CURSOR_BLOCK_START}\n  // Managed by budi. Use `budi disable cursor` to remove.\n  \"{CURSOR_OPENAI_BASE_URL_KEY}\": \"{proxy_url}\"\n  {CURSOR_BLOCK_END}"
        );
        cleaned = insert_jsonc_block_before_closing_brace(&cleaned, &block)?;
    }

    let changed = cleaned != raw;
    if changed {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }
        fs::write(&path, cleaned).with_context(|| format!("Failed writing {}", path.display()))?;
    }

    Ok((
        path,
        match (had_block, has_block) {
            (_, true) if changed => SyncState::Configured,
            (true, false) if changed => SyncState::Removed,
            _ => SyncState::Unchanged,
        },
    ))
}

fn update_text_file_block(
    path: &Path,
    start: &str,
    end: &str,
    new_block: Option<&str>,
) -> Result<BlockUpdateResult> {
    let raw = fs::read_to_string(path).unwrap_or_default();
    let (without_block, had_block) = strip_managed_block(&raw, start, end)?;

    let mut output = without_block.trim_end().to_string();
    if let Some(block) = new_block {
        if !output.is_empty() {
            output.push_str("\n\n");
        }
        output.push_str(block.trim_end());
    }
    if !output.is_empty() {
        output.push('\n');
    }

    let changed = output != raw;
    if changed {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }
        fs::write(path, output).with_context(|| format!("Failed writing {}", path.display()))?;
    }

    Ok(BlockUpdateResult {
        changed,
        had_block,
        has_block: new_block.is_some(),
    })
}

fn strip_managed_block(raw: &str, start: &str, end: &str) -> Result<(String, bool)> {
    let mut lines = Vec::new();
    let mut in_block = false;
    let mut had_block = false;

    for line in raw.lines() {
        let trimmed = line.trim();
        if !in_block && trimmed == start {
            in_block = true;
            had_block = true;
            continue;
        }
        if in_block {
            if trimmed == end {
                in_block = false;
            }
            continue;
        }
        lines.push(line);
    }

    if in_block {
        anyhow::bail!("found `{start}` without matching `{end}`")
    }

    let mut out = lines.join("\n");
    if raw.ends_with('\n') && !out.ends_with('\n') {
        out.push('\n');
    }
    Ok((out, had_block))
}

fn detect_shell_profile_path(home: &Path) -> Option<PathBuf> {
    if cfg!(target_os = "windows") {
        return None;
    }

    let zsh = home.join(".zshrc");
    let bashrc = home.join(".bashrc");
    let bash_profile = home.join(".bash_profile");

    if let Ok(shell) = std::env::var("SHELL") {
        let lower = shell.to_ascii_lowercase();
        if lower.contains("zsh") {
            return Some(zsh);
        }
        if lower.contains("bash") {
            if bashrc.exists() {
                return Some(bashrc);
            }
            return Some(bash_profile);
        }
    }

    if zsh.exists() {
        Some(zsh)
    } else if bashrc.exists() {
        Some(bashrc)
    } else {
        Some(bash_profile)
    }
}

fn resolve_cursor_settings_path(home: &Path) -> PathBuf {
    let candidates = cursor_settings_candidates(home);
    candidates
        .iter()
        .find(|candidate| candidate.exists())
        .cloned()
        .unwrap_or_else(|| candidates[0].clone())
}

fn cursor_settings_candidates(home: &Path) -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        vec![
            home.join("Library/Application Support/Cursor/User/settings.json"),
            home.join(".cursor/settings.json"),
        ]
    }

    #[cfg(target_os = "windows")]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            vec![
                PathBuf::from(appdata).join("Cursor/User/settings.json"),
                home.join("AppData/Roaming/Cursor/User/settings.json"),
            ]
        } else {
            vec![home.join("AppData/Roaming/Cursor/User/settings.json")]
        }
    }

    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        vec![
            home.join(".config/Cursor/User/settings.json"),
            home.join(".cursor/settings.json"),
        ]
    }
}

fn codex_config_path(home: &Path) -> PathBuf {
    codex_config_path_with_env(home, std::env::var("CODEX_HOME").ok().as_deref())
}

fn codex_config_path_with_env(home: &Path, codex_home: Option<&str>) -> PathBuf {
    if let Some(raw) = codex_home {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed).join("config.toml");
        }
    }
    home.join(".codex/config.toml")
}

fn insert_jsonc_block_before_closing_brace(raw: &str, block: &str) -> Result<String> {
    let Some(close_idx) = raw.rfind('}') else {
        anyhow::bail!("settings.json is not a JSON object")
    };

    let before = &raw[..close_idx];
    let after = &raw[close_idx..];

    let mut out = before.trim_end().to_string();

    let needs_comma = match previous_significant_jsonc_char(before) {
        Some('{') | None => false,
        Some(',') => false,
        Some(_) => true,
    };

    if needs_comma {
        out.push(',');
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(block);
    out.push('\n');
    out.push_str(after.trim_start());
    if !out.ends_with('\n') {
        out.push('\n');
    }

    Ok(out)
}

fn previous_significant_jsonc_char(raw: &str) -> Option<char> {
    for line in raw.lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("//") {
            continue;
        }
        return trimmed.chars().last();
    }
    None
}

fn trim_trailing_comma_before_closing_brace(raw: &str) -> String {
    let Some(close_idx) = raw.rfind('}') else {
        return raw.to_string();
    };

    let mut idx = close_idx;
    let bytes = raw.as_bytes();
    while idx > 0 && bytes[idx - 1].is_ascii_whitespace() {
        idx -= 1;
    }

    if idx > 0 && bytes[idx - 1] == b',' {
        let mut out = String::new();
        out.push_str(&raw[..idx - 1]);
        out.push_str(&raw[idx..]);
        out
    } else {
        raw.to_string()
    }
}

fn extract_block(raw: &str, start: &str, end: &str) -> Option<String> {
    let mut in_block = false;
    let mut block_lines = Vec::new();

    for line in raw.lines() {
        let trimmed = line.trim();
        if !in_block {
            if trimmed == start {
                in_block = true;
                block_lines.push(line.to_string());
            }
            continue;
        }

        block_lines.push(line.to_string());
        if trimmed == end {
            return Some(block_lines.join("\n"));
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_managed_block_removes_block() {
        let raw = "a\n# >>> budi >>>\nexport X=1\n# <<< budi <<<\nb\n";
        let (clean, had_block) =
            strip_managed_block(raw, SHELL_BLOCK_START, SHELL_BLOCK_END).expect("strip");
        assert!(had_block);
        assert_eq!(clean, "a\nb\n");
    }

    #[test]
    fn insert_jsonc_block_adds_comma_when_needed() {
        let raw = "{\n  \"a\": 1\n}\n";
        let out = insert_jsonc_block_before_closing_brace(
            raw,
            "  // >>> budi >>>\n  \"b\": 2\n  // <<< budi <<<",
        )
        .expect("insert");
        assert!(out.contains("\"a\": 1,\n  // >>> budi >>>"));
        assert!(out.contains("\"b\": 2"));
    }

    #[test]
    fn trim_trailing_comma_before_closing_brace_removes_dangling_comma() {
        let raw = "{\n  \"a\": 1,\n}\n";
        let out = trim_trailing_comma_before_closing_brace(raw);
        assert_eq!(out, "{\n  \"a\": 1\n}\n");
    }

    #[test]
    fn codex_home_overrides_default_path() {
        let path =
            codex_config_path_with_env(Path::new("/home/test"), Some("/tmp/codex-home-test"));
        assert_eq!(path, PathBuf::from("/tmp/codex-home-test/config.toml"));
    }
}
