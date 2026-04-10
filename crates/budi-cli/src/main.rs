use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use tracing_subscriber::EnvFilter;

mod client;
mod commands;
mod daemon;

use crate::commands::integrations::{IntegrationComponent, StatuslinePreset};

const HEALTH_TIMEOUT_SECS: u64 = 3;

#[derive(Debug, Parser)]
#[command(name = "budi")]
#[command(about = "budi — AI cost analytics. Know where your tokens and money go.")]
#[command(version)]
#[command(
    after_help = "Get started:\n  budi init\n\nCommon commands:\n  budi stats              Show today's cost summary\n  budi stats --models     Cost breakdown by model\n  budi stats --branches   Cost breakdown by branch\n  budi integrations list  Show integration status\n  budi doctor             Check health: daemon, database, config\n  budi sync               Sync recent transcripts (last 30 days)\n  budi sync --force       Re-ingest all data from scratch (use after upgrades)\n  budi repair             Repair schema drift and run migration\n  budi open               Open the local dashboard (legacy)\n\nMore info: https://github.com/siropkin/budi"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Set up budi (starts daemon, lets you choose integrations, syncs existing data).
    Init {
        /// Initialize for the current git repo only (default: global)
        #[arg(long)]
        local: bool,
        /// Skip prompts and use default integration selection
        #[arg(long, default_value_t = false)]
        yes: bool,
        /// Explicitly install these integrations (repeatable, kebab-case names)
        #[arg(long = "with", value_enum)]
        with: Vec<IntegrationComponent>,
        /// Explicitly skip these integrations (repeatable, kebab-case names)
        #[arg(long = "without", value_enum)]
        without: Vec<IntegrationComponent>,
        /// Integration selection mode: auto (default), all, or none
        #[arg(long, value_enum, default_value_t = InitIntegrationsMode::Auto)]
        integrations: InitIntegrationsMode,
        /// Claude Code status line preset (coach=session health, cost=period)
        #[arg(long, value_enum)]
        statusline_preset: Option<StatuslinePreset>,
        #[arg(long, hide = true)]
        repo_root: Option<PathBuf>,
        #[arg(long, hide = true)]
        no_daemon: bool,
        /// Skip automatic sync of existing transcripts
        #[arg(long)]
        no_sync: bool,
    },
    /// Check health: daemon, database, config
    Doctor {
        /// Run full SQLite integrity_check (slower). Default uses quick_check.
        #[arg(long, default_value_t = false)]
        deep: bool,
        #[arg(long, hide = true)]
        repo_root: Option<PathBuf>,
    },
    /// Show usage analytics (only one view flag at a time: --projects, --branches, --branch, --models, or --tag)
    #[command(
        group(clap::ArgGroup::new("view").multiple(false).args(["projects", "branches", "branch", "models", "tag"])),
        after_help = "\
Examples:
  budi stats                       Today's cost summary (default)
  budi stats -p week               This week's summary
  budi stats -p month --models     Model breakdown for the month
  budi stats --branches            Branches ranked by cost (today)
  budi stats --branch main         Cost details for a specific branch
  budi stats --branch main --repo github.com/acme/app
  budi stats --projects -p all     All-time project costs
  budi stats --tag activity        Cost by activity type
  budi stats --provider cursor     Filter to Cursor only
  budi stats --format json         JSON output for scripting"
    )]
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
        /// Optional repository filter for --branch (recommended when branch names repeat)
        #[arg(long, requires = "branch")]
        repo: Option<String>,
        /// Show model usage breakdown
        #[arg(long, default_value_t = false)]
        models: bool,
        /// Filter by provider (e.g. claude_code, cursor). Only works with the default summary view.
        #[arg(long, conflicts_with = "view")]
        provider: Option<String>,
        /// Show cost breakdown by tag key (e.g. --tag ticket_id, --tag activity)
        #[arg(long)]
        tag: Option<String>,
        /// Output format: text (default) or json
        #[arg(short, long, value_enum, default_value_t = StatsFormat::Text)]
        format: StatsFormat,
    },
    /// Sync recent transcripts (last 30 days). Use --all for full history, --force to re-ingest from scratch.
    Sync {
        /// Load full transcript history (all time — may take a while)
        #[arg(long, conflicts_with = "force")]
        all: bool,
        /// Force re-sync: clears all data and re-ingests from scratch.
        /// Use after upgrading budi when the cost calculation has changed.
        #[arg(long, conflicts_with = "all")]
        force: bool,
    },
    /// Open the local dashboard in the browser (legacy — will be replaced by the Rich CLI)
    Open,
    /// Update budi to the latest version
    Update {
        /// Skip confirmation prompt
        #[arg(long)]
        yes: bool,
        /// Update to a specific version (e.g. 7.1.0 or v7.1.0)
        #[arg(long)]
        version: Option<String>,
    },
    /// Remove budi hooks, status line, and data (keeps binaries)
    Uninstall {
        /// Keep the analytics database and data
        #[arg(long)]
        keep_data: bool,
        /// Skip confirmation prompt
        #[arg(long)]
        yes: bool,
    },
    /// Run database migration explicitly (usually automatic with init/update)
    Migrate,
    /// Repair schema drift and run migration checks
    Repair,
    /// Receive hook events from Claude Code / Cursor (reads JSON from stdin, fire-and-forget)
    #[command(hide = true)]
    Hook {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        _args: Vec<String>,
    },
    /// Show session health vitals (context drag, cache efficiency, thrashing, cost acceleration)
    Health {
        /// Session ID to check (default: most recent session)
        #[arg(long)]
        session: Option<String>,
    },
    /// Show AI spending in your shell prompt (reads editor context from stdin when piped)
    Statusline {
        /// Install the status line in ~/.claude/settings.json
        #[arg(long, default_value_t = false)]
        install: bool,
        /// Output format: claude (ANSI+OSC8), starship (plain text), json, custom (uses config template)
        #[arg(long, value_enum, default_value_t = StatuslineFormat::Claude)]
        format: StatuslineFormat,
    },
    /// Manage optional integrations (install later, list current status)
    Integrations {
        #[command(subcommand)]
        action: IntegrationAction,
    },
}

#[derive(Debug, Subcommand)]
enum IntegrationAction {
    /// List all available integrations and whether they are installed
    List,
    /// Install selected integrations
    Install {
        /// Integrations to install (repeatable). If omitted, installs recommended set.
        #[arg(long = "with", value_enum)]
        with: Vec<IntegrationComponent>,
        /// Install every available integration
        #[arg(long, default_value_t = false)]
        all: bool,
        /// Statusline preset for Claude Code status line (coach=session health, cost=period)
        #[arg(long, value_enum)]
        statusline_preset: Option<StatuslinePreset>,
        /// Skip prompts and use defaults
        #[arg(long, default_value_t = false)]
        yes: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum InitIntegrationsMode {
    Auto,
    All,
    None,
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
    /// Plain text, no ANSI (for shell prompts)
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
            local,
            yes,
            with,
            without,
            integrations,
            statusline_preset,
            repo_root,
            no_daemon,
            no_sync,
        } => {
            let outcome = commands::init::cmd_init(
                local,
                yes,
                with,
                without,
                integrations,
                statusline_preset,
                repo_root,
                no_daemon,
                no_sync,
            )?;
            if matches!(outcome, commands::init::InitOutcome::PartialSuccess) {
                std::process::exit(2);
            }
            Ok(())
        }
        Commands::Doctor { deep, repo_root } => commands::doctor::cmd_doctor(repo_root, deep),
        Commands::Stats {
            period,
            projects,
            branches,
            branch,
            repo,
            models,
            provider,
            tag,
            format,
        } => {
            let json_output = matches!(format, StatsFormat::Json);
            commands::stats::cmd_stats(
                period,
                projects,
                branches,
                branch,
                repo,
                models,
                provider,
                tag,
                json_output,
            )
        }
        Commands::Sync { all, force } => {
            if force {
                commands::sync::cmd_force_sync()
            } else if all {
                commands::sync::cmd_history()
            } else {
                commands::sync::cmd_sync()
            }
        }
        Commands::Update { yes, version } => commands::update::cmd_update(yes, version),
        Commands::Uninstall { keep_data, yes } => {
            commands::uninstall::cmd_uninstall(keep_data, yes)
        }
        Commands::Open => commands::open::cmd_open(),
        Commands::Migrate => {
            let c = client::DaemonClient::connect()?;
            let result = c.migrate()?;
            let migrated = result
                .get("migrated")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let current = result.get("current").and_then(|v| v.as_u64()).unwrap_or(0);
            if migrated {
                let from = result.get("from").and_then(|v| v.as_u64()).unwrap_or(0);
                println!("Migrated database v{} → v{}.", from, current);
                let green = commands::ansi("\x1b[32m");
                let reset = commands::ansi("\x1b[0m");
                println!("{green}✓{reset} Migration complete.");
            } else {
                println!("Database schema is up to date (v{}).", current);
            }
            Ok(())
        }
        Commands::Repair => commands::repair::cmd_repair(),
        Commands::Hook { .. } => {
            // Hooks must NEVER block the host agent. Swallow all errors silently.
            let _ = commands::hook::cmd_hook();
            Ok(())
        }
        Commands::Health { session } => commands::health::cmd_health(session),
        Commands::Statusline { install, format } => {
            if install {
                commands::statusline::cmd_statusline_install()
            } else {
                commands::statusline::cmd_statusline(format)
            }
        }
        Commands::Integrations { action } => commands::integrations::cmd_integrations(action),
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
        assert!(lower.contains("repair"));
    }

    #[test]
    fn cli_parses_doctor_deep_flag() {
        let cli = Cli::try_parse_from(["budi", "doctor", "--deep"]).expect("doctor --deep parses");
        match cli.command {
            Commands::Doctor { deep, .. } => assert!(deep),
            _ => panic!("expected doctor command"),
        }
    }
}
