use std::net::{SocketAddr, TcpStream};
use std::process::Command;
use std::time::Duration;

use anyhow::Result;
use budi_core::config::{self, BudiConfig};

use crate::commands::ansi;
use crate::daemon;

const PROXY_ENV_KEYS: &[&str] = &[
    "ANTHROPIC_BASE_URL",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
    "OPENAI_BASE_URL",
    "COPILOT_PROVIDER_BASE_URL",
    "COPILOT_PROVIDER_TYPE",
];

// ─── Agent Definitions (ADR-0082 §1) ────────────────────────────────────────

/// Env-var builder for Claude Code (Tier 1, Anthropic Messages protocol).
fn claude_env(port: u16) -> Vec<(&'static str, String)> {
    vec![
        ("ANTHROPIC_BASE_URL", format!("http://localhost:{port}")),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1".to_string()),
    ]
}

/// Env-var builder for Codex CLI (Tier 1, OpenAI Chat Completions protocol).
fn codex_env(port: u16) -> Vec<(&'static str, String)> {
    vec![("OPENAI_BASE_URL", format!("http://localhost:{port}"))]
}

/// Env-var builder for Copilot CLI (Tier 2, proprietary BYOK env vars).
fn copilot_env(port: u16) -> Vec<(&'static str, String)> {
    vec![
        (
            "COPILOT_PROVIDER_BASE_URL",
            format!("http://localhost:{port}"),
        ),
        ("COPILOT_PROVIDER_TYPE", "openai".to_string()),
    ]
}

type EnvVarBuilder = fn(u16) -> Vec<(&'static str, String)>;

struct AgentDef {
    /// Short name used on the command line (e.g. "claude").
    name: &'static str,
    /// Human-readable display name (e.g. "Claude Code").
    display_name: &'static str,
    /// Primary binary to exec.
    binary: &'static str,
    /// Extra leading arguments injected before user args (e.g. ["copilot"] for `gh copilot`).
    binary_prefix_args: &'static [&'static str],
    /// Builds the env-var set to inject for the given proxy port.
    env_vars: Option<EnvVarBuilder>,
    /// If set, this agent cannot be CLI-launched; print these instructions instead.
    instructions: Option<&'static str>,
    /// If set, this agent is not yet supported; print this message and exit.
    unsupported_msg: Option<&'static str>,
}

const AGENTS: &[AgentDef] = &[
    AgentDef {
        name: "claude",
        display_name: "Claude Code",
        binary: "claude",
        binary_prefix_args: &[],
        env_vars: Some(claude_env),
        instructions: None,
        unsupported_msg: None,
    },
    AgentDef {
        name: "codex",
        display_name: "Codex CLI",
        binary: "codex",
        binary_prefix_args: &[],
        env_vars: Some(codex_env),
        instructions: None,
        unsupported_msg: None,
    },
    AgentDef {
        name: "copilot",
        display_name: "Copilot CLI",
        binary: "gh",
        binary_prefix_args: &["copilot"],
        env_vars: Some(copilot_env),
        instructions: None,
        unsupported_msg: None,
    },
    AgentDef {
        name: "cursor",
        display_name: "Cursor",
        binary: "cursor",
        binary_prefix_args: &[],
        env_vars: None,
        instructions: Some(
            "Cursor cannot be launched via CLI wrapper — it requires GUI configuration.\n\
             \n\
             To route Cursor through the budi proxy:\n\
             \n\
             1. Open Cursor Settings\n\
             2. Go to Models\n\
             3. Set \"Override OpenAI Base URL\" to: http://localhost:{port}\n\
             4. Restart Cursor\n\
             \n\
             The proxy is running and ready to accept connections.",
        ),
        unsupported_msg: None,
    },
    AgentDef {
        name: "gemini",
        display_name: "Gemini CLI",
        binary: "gemini",
        binary_prefix_args: &[],
        env_vars: None,
        instructions: None,
        unsupported_msg: Some(
            "Gemini CLI is not yet supported by the budi proxy.\n\
             Gemini uses a different API format that requires a separate protocol handler.\n\
             This will be added in a future release.",
        ),
    },
];

fn find_agent(name: &str) -> Option<&'static AgentDef> {
    let lower = name.to_lowercase();
    AGENTS.iter().find(|a| {
        a.name == lower
            || (a.name == "claude" && lower == "claude-code")
            || (a.name == "codex" && lower == "codex-cli")
            || (a.name == "copilot" && lower == "copilot-cli")
    })
}

// ─── Command ─────────────────────────────────────────────────────────────────

pub fn cmd_launch(
    agent_name: &str,
    proxy_port_override: Option<u16>,
    args: &[String],
) -> Result<()> {
    let green = ansi("\x1b[32m");
    let yellow = ansi("\x1b[33m");
    let dim = ansi("\x1b[2m");
    let reset = ansi("\x1b[0m");
    let bold = ansi("\x1b[1m");

    // ── Resolve agent ────────────────────────────────────────────────────

    let agent = find_agent(agent_name).ok_or_else(|| {
        let known: Vec<&str> = AGENTS.iter().map(|a| a.name).collect();
        anyhow::anyhow!(
            "Unknown agent: {agent_name}\n\n\
             Supported agents: {}\n\n\
             Usage: budi launch <agent> [-- <args>...]",
            known.join(", ")
        )
    })?;

    // Tier 3 — not yet supported
    if let Some(msg) = agent.unsupported_msg {
        eprintln!("{yellow}⚠{reset} {msg}");
        return Ok(());
    }

    // ── Load config & resolve proxy port ─────────────────────────────────

    let repo_root = crate::commands::try_resolve_repo_root(None);
    let config = match &repo_root {
        Some(root) => config::load_or_default(root)?,
        None => BudiConfig::default(),
    };

    // Precedence per ADR-0082 §3: env > CLI flag > config > default
    let proxy_port = resolve_proxy_port(proxy_port_override, &config);
    let bypass_proxy = bypass_requested();

    // ── GUI-only agents (Cursor) ─────────────────────────────────────────

    if let Some(tpl) = agent.instructions {
        eprintln!(
            "{bold}{}{reset} requires manual configuration.",
            agent.display_name
        );
        eprintln!();

        // Still start the daemon so the proxy is ready
        daemon::ensure_daemon_running(repo_root.as_deref(), &config)?;
        eprintln!("{green}✓{reset} Proxy running on port {proxy_port}");
        eprintln!();
        eprintln!("{}", tpl.replace("{port}", &proxy_port.to_string()));
        return Ok(());
    }

    // ── Binary checks ─────────────────────────────────────────────────────
    if !binary_exists(agent.binary) {
        // Special case: codex CLI not found, check for Codex Desktop app
        if agent.name == "codex" {
            #[cfg(target_os = "macos")]
            if std::path::Path::new("/Applications/Codex.app").exists() {
                eprintln!(
                    "{yellow}Codex Desktop detected but Codex CLI is not installed.{reset}\n\
                     \n\
                     To route Codex Desktop through the budi proxy, add to ~/.codex/config.toml:\n\
                     \n\
                     openai_base_url = \"http://localhost:{proxy_port}\"\n\
                     \n\
                     Then restart Codex Desktop.",
                    proxy_port = resolve_proxy_port(proxy_port_override, &config),
                );
                return Ok(());
            }
        }

        anyhow::bail!(
            "{} binary '{}' not found in PATH.\n\
             Install {} first, then run: budi launch {}",
            agent.display_name,
            agent.binary,
            agent.display_name,
            agent.name
        );
    }

    // Copilot CLI: verify `gh copilot` extension is installed
    if agent.name == "copilot" && !gh_copilot_available() {
        anyhow::bail!(
            "GitHub Copilot CLI not found.\n\
             Install with: gh extension install github/gh-copilot\n\
             Then run: budi launch copilot"
        );
    }

    // ── Proxy pre-flight (unless bypass requested) ───────────────────────
    if !bypass_proxy {
        if !config.proxy.effective_enabled() {
            anyhow::bail!(
                "Proxy is disabled. Enable it in config.toml:\n\n\
                 [proxy]\n\
                 enabled = true\n\n\
                 Or set BUDI_PROXY_ENABLED=true"
            );
        }

        daemon::ensure_daemon_running(repo_root.as_deref(), &config)?;

        if !proxy_port_is_listening(proxy_port) {
            anyhow::bail!(
                "Proxy port {proxy_port} is not listening.\n\
                 The daemon may have started without the proxy enabled.\n\
                 Check: budi doctor"
            );
        }
    }

    // ── Copilot: warn if COPILOT_MODEL is not set ────────────────────────

    if agent.name == "copilot" && std::env::var("COPILOT_MODEL").is_err() {
        eprintln!(
            "{yellow}⚠{reset} COPILOT_MODEL is not set. \
             Copilot CLI requires this env var to select a model.\n  \
             Example: export COPILOT_MODEL=gpt-4o\n"
        );
    }

    // ── Build and exec ───────────────────────────────────────────────────

    let env_vars = if bypass_proxy {
        Vec::new()
    } else {
        (agent.env_vars.expect("launchable agent must have env_vars"))(proxy_port)
    };

    if bypass_proxy {
        eprintln!(
            "{yellow}⚠{reset} Launching {bold}{}{reset} with proxy bypass {dim}(BUDI_BYPASS=1){reset}",
            agent.display_name
        );
    } else {
        eprintln!(
            "{green}✓{reset} Launching {bold}{}{reset} through budi proxy {dim}(port {proxy_port}){reset}",
            agent.display_name
        );
        for (key, val) in &env_vars {
            eprintln!("  {dim}{key}={val}{reset}");
        }
    }
    eprintln!();

    let mut cmd = Command::new(agent.binary);
    cmd.args(agent.binary_prefix_args);
    cmd.args(args);
    if bypass_proxy {
        for key in PROXY_ENV_KEYS {
            cmd.env_remove(key);
        }
    } else {
        for (key, val) in &env_vars {
            cmd.env(key, val);
        }
    }

    // On Unix, exec() replaces the budi process with the agent.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        // exec only returns on error
        anyhow::bail!("Failed to exec {}: {}", agent.display_name, err);
    }

    // On non-Unix, spawn and forward the exit code.
    #[cfg(not(unix))]
    {
        let status = cmd
            .status()
            .map_err(|e| anyhow::anyhow!("Failed to launch {}: {}", agent.display_name, e))?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Resolve proxy port per ADR-0082 §3: env > CLI flag > config > default.
fn resolve_proxy_port(cli_flag: Option<u16>, config: &BudiConfig) -> u16 {
    if let Ok(val) = std::env::var("BUDI_PROXY_PORT")
        && let Ok(port) = val.trim().parse::<u16>()
    {
        return port;
    }
    if let Some(port) = cli_flag {
        return port;
    }
    config.proxy.port
}

fn bypass_requested() -> bool {
    std::env::var("BUDI_BYPASS")
        .ok()
        .is_some_and(|value| value.trim() == "1")
}

/// Check if a binary is available in PATH.
fn binary_exists(name: &str) -> bool {
    if let Ok(paths) = std::env::var("PATH") {
        for dir in std::env::split_paths(&std::ffi::OsString::from(&paths)) {
            if dir.join(name).is_file() {
                return true;
            }
            #[cfg(windows)]
            {
                if dir.join(format!("{name}.exe")).is_file() {
                    return true;
                }
            }
        }
    }
    false
}

/// Check if `gh copilot --help` succeeds (extension is installed).
fn gh_copilot_available() -> bool {
    Command::new("gh")
        .args(["copilot", "--help"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Check that the proxy TCP port is accepting connections.
fn proxy_port_is_listening(port: u16) -> bool {
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok()
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_agent_by_name() {
        assert_eq!(find_agent("claude").unwrap().name, "claude");
        assert_eq!(find_agent("codex").unwrap().name, "codex");
        assert_eq!(find_agent("copilot").unwrap().name, "copilot");
        assert_eq!(find_agent("cursor").unwrap().name, "cursor");
        assert_eq!(find_agent("gemini").unwrap().name, "gemini");
    }

    #[test]
    fn find_agent_aliases() {
        assert_eq!(find_agent("claude-code").unwrap().name, "claude");
        assert_eq!(find_agent("Claude-Code").unwrap().name, "claude");
        assert_eq!(find_agent("CLAUDE").unwrap().name, "claude");
        assert_eq!(find_agent("codex-cli").unwrap().name, "codex");
        assert_eq!(find_agent("copilot-cli").unwrap().name, "copilot");
    }

    #[test]
    fn find_agent_unknown() {
        assert!(find_agent("vim").is_none());
        assert!(find_agent("").is_none());
    }

    #[test]
    fn claude_env_sets_anthropic_base_url() {
        let vars = claude_env(9878);
        assert_eq!(vars.len(), 2);
        assert_eq!(vars[0].0, "ANTHROPIC_BASE_URL");
        assert_eq!(vars[0].1, "http://localhost:9878");
        assert_eq!(vars[1].0, "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC");
        assert_eq!(vars[1].1, "1");
    }

    #[test]
    fn codex_env_sets_openai_base_url() {
        let vars = codex_env(9878);
        assert_eq!(vars.len(), 1);
        assert_eq!(vars[0].0, "OPENAI_BASE_URL");
        assert_eq!(vars[0].1, "http://localhost:9878");
    }

    #[test]
    fn copilot_env_sets_byok_vars() {
        let vars = copilot_env(9999);
        assert_eq!(vars.len(), 2);
        assert_eq!(vars[0].0, "COPILOT_PROVIDER_BASE_URL");
        assert_eq!(vars[0].1, "http://localhost:9999");
        assert_eq!(vars[1].0, "COPILOT_PROVIDER_TYPE");
        assert_eq!(vars[1].1, "openai");
    }

    #[test]
    fn custom_port_in_env_vars() {
        let vars = claude_env(1234);
        assert_eq!(vars[0].1, "http://localhost:1234");
    }

    #[test]
    fn cursor_has_instructions() {
        let agent = find_agent("cursor").unwrap();
        assert!(agent.instructions.is_some());
        assert!(agent.env_vars.is_none());
    }

    #[test]
    fn gemini_is_unsupported() {
        let agent = find_agent("gemini").unwrap();
        assert!(agent.unsupported_msg.is_some());
    }

    #[test]
    fn resolve_proxy_port_defaults() {
        let config = BudiConfig::default();
        // When no env var or CLI flag, uses config default (9878)
        let port = resolve_proxy_port(None, &config);
        assert_eq!(port, 9878);
    }

    #[test]
    fn resolve_proxy_port_cli_flag_overrides_config() {
        let config = BudiConfig::default();
        let port = resolve_proxy_port(Some(1234), &config);
        assert_eq!(port, 1234);
    }

    #[test]
    fn copilot_uses_gh_binary_with_prefix() {
        let agent = find_agent("copilot").unwrap();
        assert_eq!(agent.binary, "gh");
        assert_eq!(agent.binary_prefix_args, &["copilot"]);
    }

    #[test]
    fn cursor_instructions_contain_port_placeholder() {
        let agent = find_agent("cursor").unwrap();
        let instructions = agent.instructions.unwrap();
        assert!(instructions.contains("{port}"));
    }
}
