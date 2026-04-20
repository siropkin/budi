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
    after_help = "Get started:\n  budi init\n\nCommon commands:\n  budi stats              Show today's cost summary\n  budi stats --models     Cost breakdown by model\n  budi stats --branches   Cost breakdown by branch\n  budi sessions           List recent sessions with cost and vitals\n  budi sessions <id>      Session detail: cost, models, vitals, tags\n  budi vitals             Session health vitals for the most recent session\n  budi status             Quick check: daemon and today's spend\n  budi doctor             Full diagnostic: daemon, tailer, schema, transcript visibility\n  budi cloud status       Cloud sync readiness and last-synced-at\n  budi cloud sync         Push queued local data to the cloud now\n  budi autostart status   Check daemon autostart service\n  budi db import          Import historical transcripts from disk\n  budi db import --force  Re-ingest all data from scratch (use after upgrades)\n  budi db repair          Repair schema drift and run migration\n  budi db migrate         Run database migration explicitly\n\nMore info: https://github.com/siropkin/budi"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Set up budi (daemon + autostart) and show detected agents.
    Init {
        /// Remove legacy 8.0/8.1 proxy residue after showing a consent-first cleanup flow
        #[arg(long, default_value_t = false)]
        cleanup: bool,
        /// Skip cleanup confirmation prompts
        #[arg(long, default_value_t = false)]
        yes: bool,
        #[arg(long, hide = true)]
        no_daemon: bool,
    },
    /// Check health: daemon, tailer, schema, transcript visibility
    Doctor {
        /// Run full SQLite integrity_check (slower). Default uses quick_check.
        #[arg(long, default_value_t = false)]
        deep: bool,
        #[arg(long, hide = true)]
        repo_root: Option<PathBuf>,
    },
    /// Show usage analytics (only one view flag at a time: --projects, --branches, --branch, --tickets, --ticket, --activities, --activity, --files, --file, --models, or --tag)
    #[command(
        group(clap::ArgGroup::new("view").multiple(false).args(["projects", "branches", "branch", "tickets", "ticket", "activities", "activity", "files", "file", "models", "tag"])),
        after_help = "\
Examples:
  budi stats                       Today's cost summary (default)
  budi stats -p week               This week's summary
  budi stats -p month --models     Model breakdown for the month
  budi stats --branches            Branches ranked by cost (today)
  budi stats --branch main         Cost details for a specific branch
  budi stats --branch main --repo github.com/acme/app
  budi stats --tickets             Tickets ranked by cost (today)
  budi stats --ticket ENG-123      Cost details for a specific ticket
  budi stats --ticket ENG-123 --repo github.com/acme/app
  budi stats --activities          Activities ranked by cost (today)
  budi stats --activity bugfix     Cost details for a specific activity
  budi stats --files               Files ranked by cost (today)
  budi stats --file src/main.rs    Cost details for a specific file
  budi stats --projects -p all     All-time project costs
  budi stats --tag activity        Raw cost breakdown by the activity tag
  budi stats --provider cursor     Filter to Cursor only
  budi stats --format json         JSON output for scripting"
    )]
    Stats {
        /// Time period to show (today, week, month, all, or relative like 1d, 7d, 1m)
        #[arg(long, short, default_value = "today")]
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
        /// Show tickets ranked by cost (sourced from the `ticket_id` tag).
        /// Mirrors `--branches` so ticket attribution is a first-class CLI
        /// dimension alongside branches and repos.
        #[arg(long, default_value_t = false)]
        tickets: bool,
        /// Show cost details for a specific ticket id (e.g. ENG-123).
        /// Mirrors `--branch <NAME>` and includes a per-branch breakdown
        /// of where the ticket was worked on.
        #[arg(long, value_name = "ID")]
        ticket: Option<String>,
        /// Show activities ranked by cost (sourced from the `activity` tag
        /// emitted by the prompt classifier). Mirrors `--tickets` so
        /// activity attribution is a first-class CLI dimension.
        #[arg(long, default_value_t = false)]
        activities: bool,
        /// Show cost details for a specific activity (e.g. `bugfix`,
        /// `refactor`). Mirrors `--ticket <ID>` and includes a per-branch
        /// breakdown so you can see where each kind of work was done.
        #[arg(long, value_name = "NAME")]
        activity: Option<String>,
        /// Show files ranked by cost (sourced from the `file_path` tag
        /// emitted by the pipeline when tool-call arguments point at a
        /// file inside the repo root). Mirrors `--tickets` / `--activities`
        /// so file-level attribution is a first-class CLI dimension.
        /// Added in R1.4 (#292).
        #[arg(long, default_value_t = false)]
        files: bool,
        /// Show cost details for a specific file (repo-relative path,
        /// forward-slashed, inside the repo root). Mirrors `--ticket <ID>`
        /// and includes per-branch and per-ticket breakdowns so you can see
        /// which tickets touched the file. Added in R1.4 (#292).
        #[arg(long, value_name = "PATH")]
        file: Option<String>,
        /// Optional repository filter for --branch, --ticket, --activity,
        /// or --file (recommended when names repeat across repos).
        #[arg(long)]
        repo: Option<String>,
        /// Show model usage breakdown
        #[arg(long, default_value_t = false)]
        models: bool,
        /// Filter by provider (e.g. claude_code, cursor, codex, copilot_cli, openai). Only works with the default summary view.
        #[arg(long, conflicts_with = "view")]
        provider: Option<String>,
        /// Show cost breakdown by tag key (e.g. --tag ticket_id, --tag activity)
        #[arg(long)]
        tag: Option<String>,
        /// Output format: text (default) or json
        #[arg(short, long, value_enum, default_value_t = StatsFormat::Text)]
        format: StatsFormat,
    },
    /// Update budi to the latest version
    Update {
        /// Skip confirmation prompt
        #[arg(long)]
        yes: bool,
        /// Update to a specific version (e.g. 7.1.0 or v7.1.0)
        #[arg(long)]
        version: Option<String>,
    },
    /// Remove budi configuration, integrations, and data (keeps binaries)
    Uninstall {
        /// Keep the analytics database and data
        #[arg(long)]
        keep_data: bool,
        /// Skip confirmation prompt
        #[arg(long)]
        yes: bool,
    },
    /// Database admin commands (migrate, repair, import historical transcripts)
    ///
    /// Groups the previously-bare `budi migrate` / `budi repair` /
    /// `budi import` verbs under a single namespace (R2.1 CLI audit
    /// follow-up, #368). The bare verbs still parse in 8.2.x as hidden
    /// backward-compatibility aliases but print a one-per-day stderr
    /// deprecation hint and are slated for removal in 8.3.
    #[command(after_help = "\
Examples:
  budi db migrate            Run database migration explicitly
  budi db repair             Repair schema drift and run migration checks
  budi db import             Import historical transcripts from disk
  budi db import --force     Re-ingest all data from scratch (use after upgrades)")]
    Db {
        #[command(subcommand)]
        action: DbAction,
    },
    /// Deprecated: moved to `budi db migrate`. Still functional in 8.2 for backward compatibility; will be removed in 8.3.
    #[command(hide = true)]
    Migrate,
    /// Deprecated: moved to `budi db repair`. Still functional in 8.2 for backward compatibility; will be removed in 8.3.
    #[command(hide = true)]
    Repair,
    /// Deprecated: moved to `budi db import`. Still functional in 8.2 for backward compatibility; will be removed in 8.3.
    #[command(hide = true)]
    Import {
        /// Clear all data and re-ingest from scratch.
        /// Use after upgrading budi when the cost calculation has changed.
        #[arg(long, default_value_t = false)]
        force: bool,
    },
    /// Show session health vitals (context drag, cache efficiency, thrashing, cost acceleration)
    Vitals {
        /// Session ID to check (default: most recent session)
        #[arg(long)]
        session: Option<String>,
    },
    /// Deprecated: renamed to `budi vitals`. Still functional in 8.2 for backward compatibility; will be removed in 8.3.
    #[command(hide = true)]
    Health {
        /// Session ID to check (default: most recent session)
        #[arg(long)]
        session: Option<String>,
    },
    /// List recent sessions or show session detail
    #[command(after_help = "\
Examples:
  budi sessions                    Recent sessions (today)
  budi sessions -p week            This week's sessions
  budi sessions --search claude    Filter by search term
  budi sessions --ticket ENG-123   Sessions tagged with a ticket
  budi sessions --activity bugfix  Sessions classified as bug-fix work
  budi sessions <session-id>       Show detail for a specific session
  budi sessions --format json      JSON output for scripting")]
    Sessions {
        /// Session ID for detail view (omit for session list)
        #[arg()]
        session_id: Option<String>,
        /// Time period for session list (today, week, month, all, or relative like 1d, 7d, 1m)
        #[arg(long, short, default_value = "today")]
        period: StatsPeriod,
        /// Filter sessions by search term (model, repo, branch, provider)
        #[arg(long)]
        search: Option<String>,
        /// Filter sessions by ticket id (e.g. ENG-123). Matches the
        /// `ticket_id` tag emitted by the git enricher when the branch name
        /// contains a recognised ID.
        #[arg(long, value_name = "ID")]
        ticket: Option<String>,
        /// Filter sessions by activity (e.g. `bugfix`, `refactor`). Matches
        /// the `activity` tag emitted by the prompt classifier; promoted to
        /// a first-class session filter in 8.1 (#305).
        #[arg(long, value_name = "NAME")]
        activity: Option<String>,
        /// Max sessions to show (default: 20)
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Output format: text (default) or json
        #[arg(short, long, value_enum, default_value_t = StatsFormat::Text)]
        format: StatsFormat,
    },
    /// Quick overview: daemon and today's cost (is everything working?)
    Status,
    /// Show AI spending in your shell prompt (reads editor context from stdin when piped)
    ///
    /// Emits the shared provider-scoped status contract (ADR-0088 §4, #224).
    /// Rolling `1d` / `7d` / `30d` windows. The `--format claude` surface is
    /// automatically scoped to `claude_code` usage; downstream consumers
    /// (Cursor extension, cloud dashboard) pass `--provider` explicitly.
    #[command(after_help = "\
Examples:
  budi statusline                              Default quiet output scoped to the Claude Code surface
  budi statusline --format json                Emit the shared status contract (JSON)
  budi statusline --format json --provider cursor   Consume the same shape for the Cursor surface
  budi statusline --install                    Install budi into the Claude Code status line")]
    Statusline {
        /// Install the status line in ~/.claude/settings.json
        #[arg(long, default_value_t = false)]
        install: bool,
        /// Output format: claude (ANSI+OSC8), starship (plain text), json, custom (uses config template)
        #[arg(long, value_enum, default_value_t = StatuslineFormat::Claude)]
        format: StatuslineFormat,
        /// Scope all costs to a single provider (claude_code, cursor, codex, copilot_cli).
        /// Defaults to `claude_code` when `--format claude` is used so the
        /// Claude Code statusline never shows blended multi-provider totals.
        #[arg(long)]
        provider: Option<String>,
    },
    /// Manage optional integrations (install later, list current status)
    Integrations {
        #[command(subcommand)]
        action: IntegrationAction,
    },
    /// Manage the daemon autostart service (launchd / systemd / Task Scheduler)
    #[command(after_help = "\
Examples:
  budi autostart status      Check if autostart is installed and running
  budi autostart install     Install the autostart service
  budi autostart uninstall   Remove the autostart service")]
    Autostart {
        #[command(subcommand)]
        action: AutostartAction,
    },
    /// Manual cloud sync and cloud freshness reporting
    ///
    /// `budi cloud sync` pushes queued local rollups and session summaries
    /// to the cloud now (same work the background worker runs on an
    /// interval — ADR-0083 §9, issue #225). `budi cloud status` reports
    /// whether cloud sync is enabled, when it last succeeded, and how many
    /// records are queued locally.
    #[command(after_help = "\
Examples:
  budi cloud status              Show cloud sync readiness and last sync
  budi cloud sync                Push queued local data to the cloud now
  budi cloud sync --format json  JSON output (exit code 2 on failure)")]
    Cloud {
        #[command(subcommand)]
        action: CloudAction,
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

#[derive(Debug, Subcommand)]
enum CloudAction {
    /// Show cloud sync readiness and last-synced-at
    Status {
        /// Output format: text (default) or json
        #[arg(short, long, value_enum, default_value_t = StatsFormat::Text)]
        format: StatsFormat,
    },
    /// Push queued local data (daily rollups, session summaries) to the cloud now
    Sync {
        /// Output format: text (default) or json
        #[arg(short, long, value_enum, default_value_t = StatsFormat::Text)]
        format: StatsFormat,
    },
}

#[derive(Debug, Subcommand)]
enum DbAction {
    /// Run database migration explicitly (usually automatic with init/update)
    Migrate,
    /// Repair schema drift and run migration checks
    Repair,
    /// Import historical transcripts from Claude Code, Codex, Copilot CLI, and Cursor into the analytics database.
    ///
    /// Use --force to clear all data and re-ingest from scratch (e.g. after upgrades).
    Import {
        /// Clear all data and re-ingest from scratch.
        /// Use after upgrading budi when the cost calculation has changed.
        #[arg(long, default_value_t = false)]
        force: bool,
    },
}

#[derive(Debug, Subcommand)]
enum AutostartAction {
    /// Show whether the autostart service is installed and running
    Status,
    /// Install the autostart service (daemon starts at login)
    Install,
    /// Remove the autostart service
    Uninstall,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StatsPeriod {
    Today,
    Week,
    Month,
    All,
    Days(u32),
    Weeks(u32),
    Months(u32),
}

impl std::str::FromStr for StatsPeriod {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "today" => Ok(StatsPeriod::Today),
            "week" => Ok(StatsPeriod::Week),
            "month" => Ok(StatsPeriod::Month),
            "all" => Ok(StatsPeriod::All),
            _ => {
                let len = s.len();
                if len < 2 {
                    return Err(format!("Invalid period format: {}", s));
                }
                let (num_str, unit) = s.split_at(len - 1);
                let num = num_str
                    .parse::<u32>()
                    .map_err(|_| format!("Invalid number in period: {}", s))?;
                match unit.to_lowercase().as_str() {
                    "d" => Ok(StatsPeriod::Days(num)),
                    "w" => Ok(StatsPeriod::Weeks(num)),
                    "m" => Ok(StatsPeriod::Months(num)),
                    _ => Err(format!("Invalid unit in period: {}. Use d, w, or m.", s)),
                }
            }
        }
    }
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
            cleanup,
            yes,
            no_daemon,
        } => commands::init::cmd_init(cleanup, yes, no_daemon),
        Commands::Doctor { deep, repo_root } => commands::doctor::cmd_doctor(repo_root, deep),
        Commands::Stats {
            period,
            projects,
            branches,
            branch,
            tickets,
            ticket,
            activities,
            activity,
            files,
            file,
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
                tickets,
                ticket,
                activities,
                activity,
                files,
                file,
                repo,
                models,
                provider,
                tag,
                json_output,
            )
        }
        Commands::Update { yes, version } => commands::update::cmd_update(yes, version),
        Commands::Uninstall { keep_data, yes } => {
            commands::uninstall::cmd_uninstall(keep_data, yes)
        }
        Commands::Db { action } => match action {
            DbAction::Migrate => commands::db::cmd_db_migrate(),
            DbAction::Repair => commands::repair::cmd_repair(),
            DbAction::Import { force } => commands::import::cmd_import(force),
        },
        Commands::Migrate => {
            commands::db::nudge_db_alias("migrate");
            commands::db::cmd_db_migrate()
        }
        Commands::Repair => {
            commands::db::nudge_db_alias("repair");
            commands::repair::cmd_repair()
        }
        Commands::Import { force } => {
            commands::db::nudge_db_alias("import");
            commands::import::cmd_import(force)
        }
        Commands::Vitals { session } => commands::vitals::cmd_vitals(session),
        Commands::Health { session } => {
            commands::vitals::nudge_health_alias();
            commands::vitals::cmd_vitals(session)
        }
        Commands::Statusline {
            install,
            format,
            provider,
        } => {
            if install {
                commands::statusline::cmd_statusline_install()
            } else {
                commands::statusline::cmd_statusline(format, provider)
            }
        }
        Commands::Sessions {
            session_id,
            period,
            search,
            ticket,
            activity,
            limit,
            format,
        } => {
            let json_output = matches!(format, StatsFormat::Json);
            if let Some(id) = session_id {
                commands::sessions::cmd_session_detail(&id, json_output)
            } else {
                commands::sessions::cmd_sessions(
                    period,
                    search.as_deref(),
                    ticket.as_deref(),
                    activity.as_deref(),
                    limit,
                    json_output,
                )
            }
        }
        Commands::Status => commands::status::cmd_status(),
        Commands::Integrations { action } => commands::integrations::cmd_integrations(action),
        Commands::Autostart { action } => match action {
            AutostartAction::Status => commands::autostart::cmd_autostart_status(),
            AutostartAction::Install => commands::autostart::cmd_autostart_install(),
            AutostartAction::Uninstall => commands::autostart::cmd_autostart_uninstall(),
        },
        Commands::Cloud { action } => match action {
            CloudAction::Status { format } => commands::cloud::cmd_cloud_status(format),
            CloudAction::Sync { format } => commands::cloud::cmd_cloud_sync(format),
        },
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
        assert!(lower.contains("autostart"));
        // `budi db` is the canonical DB admin namespace in 8.2.1
        // (#368). The bare `budi migrate` / `budi repair` / `budi import`
        // verbs are hidden aliases and must not appear in the subcommand
        // list so new users learn the canonical names.
        assert!(
            lower.contains("\n  db "),
            "top-level help should advertise the `budi db` namespace"
        );
        assert!(
            !lower.contains("\n  migrate "),
            "deprecated bare `budi migrate` should be hidden from help"
        );
        assert!(
            !lower.contains("\n  repair "),
            "deprecated bare `budi repair` should be hidden from help"
        );
        assert!(
            !lower.contains("\n  import "),
            "deprecated bare `budi import` should be hidden from help"
        );
        assert!(
            !lower.contains("\n  sync"),
            "sync command should be removed"
        );
    }

    #[test]
    fn cli_parses_db_subcommands() {
        let cli = Cli::try_parse_from(["budi", "db", "migrate"]).expect("budi db migrate parses");
        assert!(matches!(
            cli.command,
            Commands::Db {
                action: DbAction::Migrate
            }
        ));

        let cli = Cli::try_parse_from(["budi", "db", "repair"]).expect("budi db repair parses");
        assert!(matches!(
            cli.command,
            Commands::Db {
                action: DbAction::Repair
            }
        ));

        let cli = Cli::try_parse_from(["budi", "db", "import"]).expect("budi db import parses");
        match cli.command {
            Commands::Db {
                action: DbAction::Import { force },
            } => assert!(!force),
            _ => panic!("expected db import command"),
        }

        let cli = Cli::try_parse_from(["budi", "db", "import", "--force"])
            .expect("budi db import --force parses");
        match cli.command {
            Commands::Db {
                action: DbAction::Import { force },
            } => assert!(force),
            _ => panic!("expected db import --force command"),
        }
    }

    #[test]
    fn cli_still_parses_deprecated_db_bare_verbs() {
        // The bare `budi migrate` / `budi repair` / `budi import` verbs
        // keep parsing in 8.2.x so existing aliases, wiki docs, and
        // third-party scripts keep working for the full deprecation
        // window (slated for removal in 8.3, see #368).
        let cli = Cli::try_parse_from(["budi", "migrate"]).expect("budi migrate (alias) parses");
        assert!(matches!(cli.command, Commands::Migrate));

        let cli = Cli::try_parse_from(["budi", "repair"]).expect("budi repair (alias) parses");
        assert!(matches!(cli.command, Commands::Repair));

        let cli = Cli::try_parse_from(["budi", "import"]).expect("budi import (alias) parses");
        assert!(matches!(cli.command, Commands::Import { force: false }));

        let cli = Cli::try_parse_from(["budi", "import", "--force"])
            .expect("budi import --force (alias) parses");
        assert!(matches!(cli.command, Commands::Import { force: true }));
    }

    #[test]
    fn cli_parses_autostart_subcommands() {
        for sub in &["status", "install", "uninstall"] {
            Cli::try_parse_from(["budi", "autostart", sub])
                .unwrap_or_else(|_| panic!("budi autostart {sub} should parse"));
        }
    }

    #[test]
    fn cli_rejects_removed_proxy_commands() {
        assert!(Cli::try_parse_from(["budi", "launch", "claude"]).is_err());
        assert!(Cli::try_parse_from(["budi", "enable", "claude"]).is_err());
        assert!(Cli::try_parse_from(["budi", "disable", "cursor"]).is_err());
        assert!(Cli::try_parse_from(["budi", "proxy-install", "claude"]).is_err());
    }

    #[test]
    fn cli_parses_doctor_deep_flag() {
        let cli = Cli::try_parse_from(["budi", "doctor", "--deep"]).expect("doctor --deep parses");
        match cli.command {
            Commands::Doctor { deep, .. } => assert!(deep),
            _ => panic!("expected doctor command"),
        }
    }

    #[test]
    fn cli_parses_stats_tickets_flag() {
        let cli =
            Cli::try_parse_from(["budi", "stats", "--tickets"]).expect("budi stats --tickets");
        match cli.command {
            Commands::Stats {
                tickets, ticket, ..
            } => {
                assert!(tickets);
                assert!(ticket.is_none());
            }
            _ => panic!("expected stats command"),
        }
    }

    #[test]
    fn cli_parses_stats_ticket_value_flag() {
        let cli = Cli::try_parse_from(["budi", "stats", "--ticket", "PAVA-2057"])
            .expect("budi stats --ticket PAVA-2057");
        match cli.command {
            Commands::Stats {
                tickets, ticket, ..
            } => {
                assert!(!tickets);
                assert_eq!(ticket.as_deref(), Some("PAVA-2057"));
            }
            _ => panic!("expected stats command"),
        }
    }

    #[test]
    fn cli_stats_view_flags_are_mutually_exclusive() {
        // --tickets vs --ticket
        assert!(Cli::try_parse_from(["budi", "stats", "--tickets", "--ticket", "X-1"]).is_err());
        // --tickets vs --branches
        assert!(Cli::try_parse_from(["budi", "stats", "--tickets", "--branches"]).is_err());
        // --ticket vs --branch
        assert!(
            Cli::try_parse_from(["budi", "stats", "--ticket", "X-1", "--branch", "main"]).is_err()
        );
        // --tickets vs --models
        assert!(Cli::try_parse_from(["budi", "stats", "--tickets", "--models"]).is_err());
    }

    #[test]
    fn cli_stats_ticket_accepts_repo_filter() {
        let cli = Cli::try_parse_from([
            "budi",
            "stats",
            "--ticket",
            "PAVA-2057",
            "--repo",
            "siropkin/budi",
        ])
        .expect("budi stats --ticket --repo");
        match cli.command {
            Commands::Stats { ticket, repo, .. } => {
                assert_eq!(ticket.as_deref(), Some("PAVA-2057"));
                assert_eq!(repo.as_deref(), Some("siropkin/budi"));
            }
            _ => panic!("expected stats command"),
        }
    }

    #[test]
    fn cli_parses_sessions_ticket_flag() {
        let cli = Cli::try_parse_from(["budi", "sessions", "--ticket", "PAVA-2057"])
            .expect("budi sessions --ticket parses");
        match cli.command {
            Commands::Sessions { ticket, .. } => {
                assert_eq!(ticket.as_deref(), Some("PAVA-2057"));
            }
            _ => panic!("expected sessions command"),
        }
    }

    #[test]
    fn cli_parses_stats_activities_flag() {
        let cli = Cli::try_parse_from(["budi", "stats", "--activities"])
            .expect("budi stats --activities parses");
        match cli.command {
            Commands::Stats {
                activities,
                activity,
                ..
            } => {
                assert!(activities);
                assert!(activity.is_none());
            }
            _ => panic!("expected stats command"),
        }
    }

    #[test]
    fn cli_parses_stats_activity_value_flag() {
        let cli = Cli::try_parse_from(["budi", "stats", "--activity", "bugfix"])
            .expect("budi stats --activity bugfix parses");
        match cli.command {
            Commands::Stats {
                activities,
                activity,
                ..
            } => {
                assert!(!activities);
                assert_eq!(activity.as_deref(), Some("bugfix"));
            }
            _ => panic!("expected stats command"),
        }
    }

    #[test]
    fn cli_stats_activity_is_mutually_exclusive_with_other_views() {
        // --activities vs --tickets / --branches / --activity / --models
        assert!(
            Cli::try_parse_from(["budi", "stats", "--activities", "--tickets"]).is_err(),
            "--activities and --tickets must be mutually exclusive"
        );
        assert!(
            Cli::try_parse_from(["budi", "stats", "--activities", "--branches"]).is_err(),
            "--activities and --branches must be mutually exclusive"
        );
        assert!(
            Cli::try_parse_from(["budi", "stats", "--activities", "--activity", "bugfix"]).is_err(),
            "--activities and --activity must be mutually exclusive"
        );
        assert!(
            Cli::try_parse_from(["budi", "stats", "--activity", "bugfix", "--models"]).is_err(),
            "--activity and --models must be mutually exclusive"
        );
    }

    #[test]
    fn cli_stats_activity_accepts_repo_filter() {
        let cli = Cli::try_parse_from([
            "budi",
            "stats",
            "--activity",
            "bugfix",
            "--repo",
            "siropkin/budi",
        ])
        .expect("budi stats --activity --repo parses");
        match cli.command {
            Commands::Stats { activity, repo, .. } => {
                assert_eq!(activity.as_deref(), Some("bugfix"));
                assert_eq!(repo.as_deref(), Some("siropkin/budi"));
            }
            _ => panic!("expected stats command"),
        }
    }

    #[test]
    fn cli_parses_stats_files_flag() {
        let cli =
            Cli::try_parse_from(["budi", "stats", "--files"]).expect("budi stats --files parses");
        match cli.command {
            Commands::Stats { files, file, .. } => {
                assert!(files);
                assert!(file.is_none());
            }
            _ => panic!("expected stats command"),
        }
    }

    #[test]
    fn cli_parses_stats_file_value_flag() {
        let cli = Cli::try_parse_from(["budi", "stats", "--file", "crates/budi-core/src/lib.rs"])
            .expect("budi stats --file <path> parses");
        match cli.command {
            Commands::Stats { files, file, .. } => {
                assert!(!files);
                assert_eq!(file.as_deref(), Some("crates/budi-core/src/lib.rs"));
            }
            _ => panic!("expected stats command"),
        }
    }

    #[test]
    fn cli_stats_file_is_mutually_exclusive_with_other_views() {
        // --files vs --tickets / --branches / --file / --models etc.
        assert!(
            Cli::try_parse_from(["budi", "stats", "--files", "--tickets"]).is_err(),
            "--files and --tickets must be mutually exclusive"
        );
        assert!(
            Cli::try_parse_from(["budi", "stats", "--files", "--activities"]).is_err(),
            "--files and --activities must be mutually exclusive"
        );
        assert!(
            Cli::try_parse_from(["budi", "stats", "--files", "--file", "x.rs"]).is_err(),
            "--files and --file must be mutually exclusive"
        );
        assert!(
            Cli::try_parse_from(["budi", "stats", "--file", "x.rs", "--models"]).is_err(),
            "--file and --models must be mutually exclusive"
        );
    }

    #[test]
    fn cli_stats_file_accepts_repo_filter() {
        let cli = Cli::try_parse_from([
            "budi",
            "stats",
            "--file",
            "src/main.rs",
            "--repo",
            "siropkin/budi",
        ])
        .expect("budi stats --file --repo parses");
        match cli.command {
            Commands::Stats { file, repo, .. } => {
                assert_eq!(file.as_deref(), Some("src/main.rs"));
                assert_eq!(repo.as_deref(), Some("siropkin/budi"));
            }
            _ => panic!("expected stats command"),
        }
    }

    #[test]
    fn cli_parses_cloud_subcommands() {
        let cli = Cli::try_parse_from(["budi", "cloud", "sync"]).expect("budi cloud sync parses");
        match cli.command {
            Commands::Cloud {
                action: CloudAction::Sync { format },
            } => assert!(matches!(format, StatsFormat::Text)),
            _ => panic!("expected cloud sync command"),
        }

        let cli = Cli::try_parse_from(["budi", "cloud", "status", "--format", "json"])
            .expect("budi cloud status --format json parses");
        match cli.command {
            Commands::Cloud {
                action: CloudAction::Status { format },
            } => assert!(matches!(format, StatsFormat::Json)),
            _ => panic!("expected cloud status command"),
        }
    }

    #[test]
    fn help_lists_cloud_commands() {
        let mut command = Cli::command();
        let help = command.render_help().to_string();
        let lower = help.to_ascii_lowercase();
        assert!(
            lower.contains("cloud"),
            "top-level help should advertise cloud subcommand"
        );
        assert!(
            lower.contains("budi cloud sync"),
            "top-level help should mention `budi cloud sync`"
        );
    }

    #[test]
    fn cli_parses_vitals_command() {
        let cli = Cli::try_parse_from(["budi", "vitals"]).expect("budi vitals parses");
        match cli.command {
            Commands::Vitals { session } => assert!(session.is_none()),
            _ => panic!("expected vitals command"),
        }

        let cli = Cli::try_parse_from(["budi", "vitals", "--session", "abc"])
            .expect("budi vitals --session abc parses");
        match cli.command {
            Commands::Vitals { session } => assert_eq!(session.as_deref(), Some("abc")),
            _ => panic!("expected vitals command"),
        }
    }

    #[test]
    fn cli_still_parses_deprecated_health_alias() {
        // `budi health` must keep parsing in 8.2.x so existing user aliases,
        // statusline snippets, and third-party scripts keep working for the
        // full deprecation window. The alias is hidden from `--help` but
        // still wired through `Commands::Health`.
        let cli = Cli::try_parse_from(["budi", "health"]).expect("budi health (alias) parses");
        assert!(matches!(cli.command, Commands::Health { .. }));

        let cli = Cli::try_parse_from(["budi", "health", "--session", "abc"])
            .expect("budi health --session abc (alias) parses");
        match cli.command {
            Commands::Health { session } => assert_eq!(session.as_deref(), Some("abc")),
            _ => panic!("expected deprecated health alias"),
        }
    }

    #[test]
    fn help_advertises_vitals_and_hides_health_alias() {
        let mut command = Cli::command();
        let help = command.render_help().to_string();
        let lower = help.to_ascii_lowercase();
        assert!(
            lower.contains("vitals"),
            "top-level help should advertise `budi vitals`"
        );
        // The deprecated alias stays functional but is hidden from the
        // primary help output so new users learn the canonical name.
        assert!(
            !help.contains("\n  health "),
            "deprecated `budi health` should not appear in the subcommand list"
        );
    }

    #[test]
    fn cli_parses_sessions_activity_flag() {
        let cli = Cli::try_parse_from(["budi", "sessions", "--activity", "bugfix"])
            .expect("budi sessions --activity parses");
        match cli.command {
            Commands::Sessions {
                ticket, activity, ..
            } => {
                assert!(ticket.is_none());
                assert_eq!(activity.as_deref(), Some("bugfix"));
            }
            _ => panic!("expected sessions command"),
        }
    }
}
