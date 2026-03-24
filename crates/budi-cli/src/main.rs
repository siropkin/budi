use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use tracing_subscriber::EnvFilter;

mod client;
mod commands;
mod daemon;

const HEALTH_TIMEOUT_SECS: u64 = 3;
const STATUS_TIMEOUT_SECS: u64 = 120;
const HOOK_LOG_LOCK_TIMEOUT_MS: u64 = 800;
const HOOK_LOG_LOCK_STALE_SECS: u64 = 30;

#[derive(Debug, Parser)]
#[command(name = "budi")]
#[command(about = "budi — AI cost analytics. Know where your tokens and money go.")]
#[command(version)]
#[command(after_help = "Get started:\n  budi init --global")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Set up budi (use --global for all repos, or run in a repo for local setup)
    Init {
        #[arg(long, hide = true)]
        repo_root: Option<PathBuf>,
        #[arg(long, hide = true)]
        no_daemon: bool,
        /// Install hooks globally in ~/.claude/settings.json (works for all repos)
        #[arg(long, default_value_t = false)]
        global: bool,
    },
    /// Check repo health: config, hooks, daemon
    Doctor {
        #[arg(long, hide = true)]
        repo_root: Option<PathBuf>,
    },
    #[command(hide = true)]
    Repo {
        #[command(subcommand)]
        command: RepoCommands,
    },
    #[command(hide = true)]
    Hook {
        #[command(subcommand)]
        command: HookCommands,
    },
    /// Show usage analytics
    Stats {
        /// Time period to show (default: today)
        #[arg(long, short, value_enum, default_value_t = StatsPeriod::Today)]
        period: StatsPeriod,
        /// Show details for a specific session (ID or prefix)
        #[arg(long)]
        session: Option<String>,
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
        /// List sessions with stats
        #[arg(long, default_value_t = false)]
        sessions: bool,
        /// Filter by provider (e.g. claude_code, cursor)
        #[arg(long)]
        provider: Option<String>,
        /// Show cost breakdown by tag (e.g. --tag ticket_id or --tag team=platform)
        #[arg(long)]
        tag: Option<String>,
        /// Output as JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Sync transcripts into the analytics database
    Sync {
        /// Regenerate all tags from existing data (useful after updating tag extraction)
        #[arg(long, default_value_t = false)]
        backfill_tags: bool,
    },
    /// Open the budi dashboard in the browser
    Open,
    /// Update budi to the latest version
    Update,
    /// Run database migration (usually runs automatically with sync/update)
    #[command(hide = true)]
    Migrate,
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

#[derive(Debug, Subcommand)]
enum HookCommands {
    #[command(hide = true)]
    UserPromptSubmit,
    #[command(hide = true)]
    PostToolUse,
    #[command(hide = true)]
    SessionStart,
    #[command(hide = true)]
    SessionEnd,
    #[command(hide = true)]
    SubagentStart,
}

#[derive(Debug, Subcommand)]
enum RepoCommands {
    #[command(hide = true)]
    List {
        #[arg(long, default_value_t = false)]
        stale_only: bool,
    },
    #[command(hide = true)]
    Remove {
        #[arg(long)]
        repo_root: PathBuf,
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
    #[command(hide = true)]
    Wipe {
        #[arg(long, default_value_t = false)]
        confirm: bool,
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
    /// Show daemon status for the current repo
    Status {
        #[arg(long, hide = true)]
        repo_root: Option<PathBuf>,
    },
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
            global,
        } => commands::init::cmd_init(repo_root, no_daemon, global),
        Commands::Doctor { repo_root } => commands::doctor::cmd_doctor(repo_root),
        Commands::Repo { command } => match command {
            RepoCommands::List { stale_only } => commands::repo::cmd_repo_list(stale_only),
            RepoCommands::Remove { repo_root, dry_run } => {
                commands::repo::cmd_repo_remove(repo_root, dry_run)
            }
            RepoCommands::Wipe { confirm, dry_run } => {
                commands::repo::cmd_repo_wipe(confirm, dry_run)
            }
            RepoCommands::Status { repo_root } => commands::repo::cmd_status(repo_root),
        },
        Commands::Hook { command } => match command {
            HookCommands::UserPromptSubmit => commands::hook::cmd_hook_user_prompt_submit(),
            HookCommands::PostToolUse => commands::hook::cmd_hook_post_tool_use(),
            HookCommands::SessionStart => commands::hook::cmd_hook_session_start(),
            HookCommands::SessionEnd => commands::hook::cmd_hook_session_end(),
            HookCommands::SubagentStart => commands::hook::cmd_hook_subagent_start(),
        },
        Commands::Stats {
            period,
            session,
            projects,
            branches,
            branch,
            models,
            sessions,
            provider,
            tag,
            json,
        } => commands::stats::cmd_stats(
            period, session, projects, branches, branch, models, sessions, provider, tag, json,
        ),
        Commands::Sync { backfill_tags } => commands::sync::cmd_sync_with_options(backfill_tags),
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
        assert!(lower.contains("repo"));
        assert!(lower.contains("stats"));
        assert!(lower.contains("sync"));
    }
}
