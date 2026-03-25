use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use tracing_subscriber::EnvFilter;

mod client;
mod commands;
mod daemon;

const HEALTH_TIMEOUT_SECS: u64 = 3;

#[derive(Debug, Parser)]
#[command(name = "budi")]
#[command(about = "budi — AI cost analytics. Know where your tokens and money go.")]
#[command(version)]
#[command(after_help = "Get started:\n  budi init")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Set up budi (starts daemon, installs status line, syncs existing data)
    Init {
        #[arg(long, hide = true)]
        repo_root: Option<PathBuf>,
        #[arg(long, hide = true)]
        no_daemon: bool,
    },
    /// Check health: daemon, database, config
    Doctor {
        #[arg(long, hide = true)]
        repo_root: Option<PathBuf>,
    },
    /// Show usage analytics
    Stats {
        /// Time period to show (default: today)
        #[arg(long, short, value_enum, default_value_t = StatsPeriod::Today)]
        period: StatsPeriod,
        /// Show repositories ranked by cost
        #[arg(long, default_value_t = false)]
        projects: bool,
        /// Show branches ranked by cost
        #[arg(long, default_value_t = false)]
        branches: bool,
        /// Show cost for a specific branch
        #[arg(long)]
        branch: Option<String>,
        /// Show model usage breakdown
        #[arg(long, default_value_t = false)]
        models: bool,
        /// Filter by provider (e.g. claude_code, cursor)
        #[arg(long)]
        provider: Option<String>,
        /// Show cost breakdown by tag key (e.g. --tag ticket_id, --tag branch)
        #[arg(long)]
        tag: Option<String>,
        /// Output format: text (default) or json
        #[arg(long, value_enum, default_value_t = StatsFormat::Text)]
        format: StatsFormat,
        /// Shorthand for --format json
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Sync recent transcripts (last 7 days)
    Sync,
    /// Load full transcript history (all time — may take a while)
    History,
    /// Open the budi dashboard in the browser
    Open,
    /// Update budi to the latest version
    Update,
    /// Run database migration explicitly (usually automatic with sync/update)
    #[command(hide = true)]
    Migrate,
    /// Receive hook events from Claude Code / Cursor (reads JSON from stdin, fire-and-forget)
    #[command(hide = true)]
    Hook {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        _args: Vec<String>,
    },
    /// Print a compact status line (reads stdin, outputs one line)
    Statusline {
        /// Install the status line in ~/.claude/settings.json
        #[arg(long, default_value_t = false)]
        install: bool,
        /// Output format: claude (ANSI+OSC8), starship (plain text), json, custom (uses config template)
        #[arg(long, value_enum, default_value_t = StatuslineFormat::Claude)]
        format: StatuslineFormat,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum StatsPeriod {
    Today,
    Week,
    Month,
    All,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq)]
pub enum StatsFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq)]
pub enum StatuslineFormat {
    /// ANSI colors + OSC 8 hyperlinks (for Claude Code statusline)
    Claude,
    /// Plain text, no ANSI (for Starship / shell prompts)
    Starship,
    /// Raw JSON
    Json,
    /// Custom format from ~/.config/budi/statusline.toml
    Custom,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();

    match cli.command {
        Commands::Init {
            repo_root,
            no_daemon,
        } => commands::init::cmd_init(repo_root, no_daemon),
        Commands::Doctor { repo_root } => commands::doctor::cmd_doctor(repo_root),
        Commands::Stats {
            period,
            projects,
            branches,
            branch,
            models,
            provider,
            tag,
            format,
            json,
        } => {
            let json_output = json || matches!(format, StatsFormat::Json);
            commands::stats::cmd_stats(
                period, projects, branches, branch, models, provider, tag, json_output,
            )
        }
        Commands::Sync => commands::sync::cmd_sync(),
        Commands::History => commands::sync::cmd_history(),
        Commands::Open => commands::open::cmd_open(),
        Commands::Update => commands::update::cmd_update(),
        Commands::Migrate => {
            let c = client::DaemonClient::connect()?;
            let result = c.migrate()?;
            let migrated = result.get("migrated").and_then(|v| v.as_bool()).unwrap_or(false);
            let current = result.get("current").and_then(|v| v.as_u64()).unwrap_or(0);
            if migrated {
                let from = result.get("from").and_then(|v| v.as_u64()).unwrap_or(0);
                println!("Migrated database v{} → v{}.", from, current);
                println!("\x1b[32m✓\x1b[0m Migration complete.");
            } else {
                println!("Database schema is up to date (v{}).", current);
            }
            Ok(())
        }
        Commands::Hook { .. } => commands::hook::cmd_hook(),
        Commands::Statusline { install, format } => {
            if install {
                commands::statusline::cmd_statusline_install()
            } else {
                commands::statusline::cmd_statusline(format)
            }
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_parses_init() {
        let _ = Cli::command();
    }

    #[test]
    fn daemon_command_match_is_port_scoped() {
        let cmd = "/usr/local/bin/budi-daemon serve --host 127.0.0.1 --port 7878";
        assert!(daemon::is_budi_daemon_command_for_port(cmd, 7878));
        assert!(!daemon::is_budi_daemon_command_for_port(cmd, 9999));
        assert!(!daemon::is_budi_daemon_command_for_port(
            "python3 -m http.server 7878",
            7878
        ));
    }

    #[test]
    fn help_shows_expected_commands() {
        let mut command = Cli::command();
        let help = command.render_help().to_string();
        let lower = help.to_ascii_lowercase();
        assert!(lower.contains("init"));
        assert!(lower.contains("doctor"));
        assert!(lower.contains("stats"));
        assert!(lower.contains("sync"));
    }
}
