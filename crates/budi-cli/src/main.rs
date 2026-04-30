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
        /// Skip the default recommended-integrations install (Claude Code
        /// statusline + Cursor extension). Useful for CI, containers, or
        /// when the user manages Claude / Cursor settings by hand.
        #[arg(long, default_value_t = false)]
        no_integrations: bool,
        #[arg(long, hide = true)]
        no_daemon: bool,
    },
    /// Check health: daemon, tailer, schema, transcript visibility
    Doctor {
        /// Run full SQLite integrity_check (slower). Default uses quick_check.
        #[arg(long, default_value_t = false)]
        deep: bool,
        /// Suppress individual PASS lines on a green run — WARN / FAIL
        /// lines and the final summary still render. Useful for CI
        /// gates and for a glance-only human check on a working box.
        #[arg(long, default_value_t = false)]
        quiet: bool,
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
  budi stats -p 7d                 Last 7 days (rolling window)
  budi stats -p 2w                 Last 2 weeks (rolling window)
  budi stats -p 1m                 Last 1 month (rolling, calendar months)
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
        #[arg(long, default_value_t = false)]
        files: bool,
        /// Show cost details for a specific file (repo-relative path,
        /// forward-slashed, inside the repo root). Mirrors `--ticket <ID>`
        /// and includes per-branch and per-ticket breakdowns so you can see
        /// which tickets touched the file.
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
        /// Break down cost by the given tag KEY (e.g. `--tag ticket_id`,
        /// `--tag activity`, `--tag custom_key`). Unlike `--tickets`
        /// and `--activities`, which are first-class per-dimension
        /// views with their own ranking shape, `--tag <KEY>` is the
        /// escape hatch for any tag key the pipeline emits — including
        /// custom keys that the CLI doesn't hard-code a flag for. The
        /// output ranks rows by the tag's VALUES for that key.
        #[arg(long, value_name = "KEY")]
        tag: Option<String>,
        /// Maximum rows to show in breakdown views (`--projects`,
        /// `--branches`, `--tickets`, `--activities`, `--files`,
        /// `--models`, `--tag`). `0` = no cap (show every matching row).
        /// Truncated rows collapse into an `(other N: $X)` aggregate so
        /// the Total footer always reconciles to the cent.
        #[arg(long, default_value_t = 30)]
        limit: usize,
        /// Maximum characters for labels and label-like extra columns
        /// (branch / file path / ticket id) in breakdown views. Values
        /// longer than this truncate with a middle ellipsis (`…`). The
        /// default balances readability on an 80-col terminal with the
        /// natural length of file paths.
        #[arg(long, default_value_t = 40)]
        label_width: usize,
        /// Include zero-cost `(model not yet attributed)` rows in
        /// `--models` output. By default, Cursor-lag transient rows
        /// that carry no backing cost are collapsed into a
        /// suppressed-count footnote; pass `--include-pending` to
        /// see them as their own row. Rows with real Cursor-Auto
        /// cost always render regardless of this flag.
        #[arg(long, default_value_t = false)]
        include_pending: bool,
        /// Break out the `(no repository)` bucket in `--projects` into a
        /// per-folder breakdown keyed on the cwd basename. Off by
        /// default so the main Repositories table stays clean of
        /// `Desktop` / `~` / scratch-dir rows.
        #[arg(long, default_value_t = false)]
        include_non_repo: bool,
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
    /// Groups migrate / repair / import under a single namespace. The
    /// pre-8.2.1 bare verbs (`budi migrate` / `budi repair` /
    /// `budi import`) were removed in 8.3.0; use `budi db <verb>` instead.
    #[command(after_help = "\
Examples:
  budi db migrate                Run database migration explicitly
  budi db repair                 Repair schema drift and run migration checks
  budi db import                 Import historical transcripts from disk
  budi db import --force         Re-ingest all data from scratch (use after upgrades)
  budi db import --format json   JSON output with per-agent breakdown (for scripting)")]
    Db {
        #[command(subcommand)]
        action: DbAction,
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
  budi sessions -p 7d              Sessions in the last 7 days (rolling)
  budi sessions -p 2w              Sessions in the last 2 weeks (rolling)
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
        /// a first-class session filter in 8.1.
        #[arg(long, value_name = "NAME")]
        activity: Option<String>,
        /// Max sessions to show (default: 20)
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Render the full 36-character session UUID instead of the
        /// 8-character short form (useful for scripting and for
        /// `budi sessions <id>` lookup).
        #[arg(long, default_value_t = false)]
        full_uuid: bool,
        /// Output format: text (default) or json
        #[arg(short, long, value_enum, default_value_t = StatsFormat::Text)]
        format: StatsFormat,
    },
    /// Quick overview: daemon and today's cost (is everything working?)
    Status,
    /// Show AI spending in your shell prompt (reads editor context from stdin when piped)
    ///
    /// Emits the shared provider-scoped status contract. Rolling
    /// `1d` / `7d` / `30d` windows. The `--format claude` surface is
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
    /// `budi cloud init` generates a commented `~/.config/budi/cloud.toml`
    /// template so the user can paste their API key without guessing the
    /// schema. `budi cloud sync` pushes queued local rollups and session
    /// summaries to the cloud now (same work the background worker runs
    /// on an interval). `budi cloud status` reports whether cloud sync
    /// is enabled, when it last succeeded, and how many records are
    /// queued locally.
    #[command(after_help = "\
Examples:
  budi cloud init                Generate ~/.config/budi/cloud.toml template
  budi cloud init --api-key KEY  Write the key and enable sync in one step
  budi cloud status              Show cloud sync readiness and last sync
  budi cloud sync                Push queued local data to the cloud now
  budi cloud sync --format json  JSON output (exit code 2 on failure)
  budi cloud sync --full         Drop watermarks then re-upload everything
  budi cloud sync --full --yes   Same, non-interactive (CI / scripts)
  budi cloud reset               Reset watermarks so next sync re-uploads all
  budi cloud reset --yes         Same, non-interactive (CI / scripts)")]
    Cloud {
        #[command(subcommand)]
        action: CloudAction,
    },
    /// Pricing manifest: view status, trigger a manual refresh
    #[command(after_help = "\
Examples:
  budi pricing status              Show current manifest layer, version, and unknown models
  budi pricing status --refresh    Fetch the latest LiteLLM manifest before showing status
  budi pricing status --format json  JSON output for scripting")]
    Pricing {
        #[command(subcommand)]
        action: PricingAction,
    },
}

#[derive(Debug, Subcommand)]
enum PricingAction {
    /// Show manifest layer, version, known model count, and unknown models
    Status {
        /// Output format: text (default) or json
        #[arg(short, long, value_enum, default_value_t = StatsFormat::Text)]
        format: StatsFormat,
        /// Trigger an immediate manifest refresh before showing status
        #[arg(long, default_value_t = false)]
        refresh: bool,
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
    /// Generate `~/.config/budi/cloud.toml` from a commented template
    ///
    /// Writes a starter config with every field commented so a fresh user
    /// never has to consult external docs to bootstrap cloud sync.
    /// Without flags it leaves `api_key = "PASTE_YOUR_KEY_HERE"` and
    /// `enabled = false`; `--api-key <K>` writes the real key and flips
    /// `enabled = true` in one shot.
    Init {
        /// Paste your API key directly and set `enabled = true` in the template.
        /// Without this flag the template still writes a stub that must be
        /// edited before `budi cloud sync` will do anything.
        #[arg(long, value_name = "KEY")]
        api_key: Option<String>,
        /// Overwrite an existing `~/.config/budi/cloud.toml`. Refuses to run
        /// without `--yes` when a real (non-stub) key is about to be replaced,
        /// to avoid silently clobbering a working config.
        #[arg(long, default_value_t = false)]
        force: bool,
        /// Skip the interactive "are you sure?" confirmation when `--force`
        /// would overwrite a non-stub config.
        #[arg(long, default_value_t = false)]
        yes: bool,
        /// Manually set `device_id` instead of auto-generating a UUID v4.
        /// Useful for multi-machine setups where you want a stable
        /// human-readable id, or offline installs where the auto-seed
        /// whoami call can't reach the cloud.
        #[arg(long, value_name = "ID")]
        device_id: Option<String>,
        /// Manually set `org_id` instead of fetching it via
        /// `GET /v1/whoami`. Useful for self-hosted endpoints that
        /// don't expose `/v1/whoami` yet, or offline installs.
        #[arg(long, value_name = "ID")]
        org_id: Option<String>,
    },
    /// Show cloud sync readiness and last-synced-at
    Status {
        /// Output format: text (default) or json
        #[arg(short, long, value_enum, default_value_t = StatsFormat::Text)]
        format: StatsFormat,
    },
    /// Push queued local data (daily rollups, session summaries) to the cloud now
    ///
    /// `--full` drops the local cloud-sync watermarks before the push so the
    /// next sync re-uploads every rollup + session summary. Equivalent to
    /// running `budi cloud reset && budi cloud sync` in one step (#583). The
    /// re-upload is safe — cloud-side dedup (ADR-0083 §6) collapses any
    /// records that overlap with what the cloud already has.
    Sync {
        /// Output format: text (default) or json
        #[arg(short, long, value_enum, default_value_t = StatsFormat::Text)]
        format: StatsFormat,
        /// Drop the cloud-sync watermarks before pushing so the next sync
        /// re-uploads everything. Equivalent to `cloud reset && cloud sync`.
        #[arg(long, default_value_t = false)]
        full: bool,
        /// Skip the interactive confirmation that `--full` would otherwise
        /// show. Required for non-TTY callers (CI, scripts) — otherwise the
        /// prompt aborts to avoid a silent re-upload on a stray invocation.
        /// Ignored unless `--full` is set.
        #[arg(long, default_value_t = false)]
        yes: bool,
    },
    /// Drop the cloud sync watermarks so the next sync re-uploads everything
    ///
    /// Useful after switching orgs, rotating an api_key, or recovering from a
    /// cloud-side data wipe — the daemon's local watermark is org-blind and
    /// keeps the cloud "ahead" of where it actually is until the watermark is
    /// reset (#564). Cloud-side dedup means the re-upload is safe even if some
    /// records overlap with rows the cloud already has.
    Reset {
        /// Skip the interactive confirmation. Required for non-TTY callers
        /// (CI, scripts) — otherwise the prompt aborts to avoid a silent
        /// re-upload on a stray invocation.
        #[arg(long, default_value_t = false)]
        yes: bool,
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
        /// Output format: text (default) or json. `json` prints a structured
        /// per-agent summary at the end (suitable for scripting) and
        /// suppresses the live per-agent progress feed so stdout stays
        /// parseable.
        #[arg(short, long, value_enum, default_value_t = StatsFormat::Text)]
        format: StatsFormat,
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

/// `--period` / `-p` argument for `budi stats` and `budi sessions`.
///
/// Two flavors are supported:
///
/// * **Named windows** (`today`, `week`, `month`, `all`). `today` is
///   anchored to the start of the current local calendar day.
///   `week` and `month` resolve to rolling 7 / 30 days ending now —
///   identical to `-p 7d` / `-p 30d` — matching the README's
///   "last 7 / 30 calendar days including today" contract. Before 8.3,
///   `week` was calendar-week-starting-Monday and `month` was
///   first-of-calendar-month, which collapsed to a single day of data
///   on Mondays and on the 1st of the month respectively.
/// * **Rolling windows** (`Nd`, `Nw`, `Nm` where `N` is a positive integer) —
///   e.g. `1d`, `7d`, `2w`, `3m`. `Nd` / `Nw` go back exactly that many
///   days / weeks from the local calendar day, and `Nm` uses calendar
///   months (same day-of-month N months ago, clamped to the end of the
///   target month). This matches the rolling `1d` / `7d` / `30d`
///   windows used by the statusline surface and the cloud dashboard.
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
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Err("period is empty; use today, week, month, all, or a relative window like 1d, 7d, 2w, 1m".to_string());
        }

        match trimmed.to_ascii_lowercase().as_str() {
            "today" => return Ok(StatsPeriod::Today),
            "week" => return Ok(StatsPeriod::Week),
            "month" => return Ok(StatsPeriod::Month),
            "all" => return Ok(StatsPeriod::All),
            _ => {}
        }

        // Relative window: split into digit prefix + unit suffix using
        // char boundaries so non-ASCII input cannot panic `split_at`.
        let mut chars = trimmed.chars();
        let unit = chars.next_back().ok_or_else(|| {
            format!(
                "invalid period '{}'; use today, week, month, all, or a relative window like 1d, 7d, 2w, 1m",
                s
            )
        })?;
        let num_str: String = chars.collect();
        if num_str.is_empty() {
            return Err(format!(
                "invalid period '{}'; relative windows need a count, e.g. 1d, 7d, 2w, 1m",
                s
            ));
        }

        let num: u32 = num_str.parse().map_err(|_| {
            format!(
                "invalid number in period '{}'; use a positive integer like 1d, 7d, 2w, 1m",
                s
            )
        })?;
        if num == 0 {
            return Err(format!(
                "invalid period '{}'; relative windows must be at least 1 (e.g. 1d, 1w, 1m)",
                s
            ));
        }

        match unit.to_ascii_lowercase() {
            'd' => Ok(StatsPeriod::Days(num)),
            'w' => Ok(StatsPeriod::Weeks(num)),
            'm' => Ok(StatsPeriod::Months(num)),
            _ => Err(format!(
                "invalid period unit in '{}'; use d (days), w (weeks), or m (months), e.g. 7d, 2w, 1m",
                s
            )),
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
    /// ANSI colors + OSC 8 hyperlinks (for Claude Code statusline).
    /// Accepts `text` as an alias so `--format text` matches the shared
    /// convention the other CLI surfaces use for their default human-
    /// readable render.
    // The `text` alias landed in 8.3.1 as a fresh-user friction fix.
    // Intentionally kept out of the clap `///` doc comment so the CI
    // help-cleanliness grep guard stays green.
    #[value(alias = "text")]
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
            no_integrations,
            no_daemon,
        } => commands::init::cmd_init(cleanup, yes, no_integrations, no_daemon),
        Commands::Doctor {
            deep,
            quiet,
            repo_root,
        } => commands::doctor::cmd_doctor(repo_root, deep, quiet),
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
            limit,
            label_width,
            include_pending,
            include_non_repo,
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
                limit,
                label_width,
                include_pending,
                include_non_repo,
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
            DbAction::Import { force, format } => {
                let json = matches!(format, StatsFormat::Json);
                commands::import::cmd_import(force, json)
            }
        },
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
            full_uuid,
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
                    full_uuid,
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
            CloudAction::Init {
                api_key,
                force,
                yes,
                device_id,
                org_id,
            } => commands::cloud::cmd_cloud_init(api_key, force, yes, device_id, org_id),
            CloudAction::Status { format } => commands::cloud::cmd_cloud_status(format),
            CloudAction::Sync { format, full, yes } => {
                commands::cloud::cmd_cloud_sync(format, full, yes)
            }
            CloudAction::Reset { yes } => commands::cloud::cmd_cloud_reset(yes),
        },
        Commands::Pricing { action } => match action {
            PricingAction::Status { format, refresh } => {
                commands::pricing::cmd_pricing_status(format, refresh)
            }
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
        // `budi db` is the canonical DB admin namespace. The pre-8.2.1
        // bare verbs (`budi migrate` / `budi repair` / `budi import`)
        // were removed in 8.3.0 (#428) and must not come back as a
        // top-level command.
        assert!(
            lower.contains("\n  db "),
            "top-level help should advertise the `budi db` namespace"
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
                action: DbAction::Import { force, format },
            } => {
                assert!(!force);
                assert_eq!(format, StatsFormat::Text);
            }
            _ => panic!("expected db import command"),
        }

        let cli = Cli::try_parse_from(["budi", "db", "import", "--force"])
            .expect("budi db import --force parses");
        match cli.command {
            Commands::Db {
                action: DbAction::Import { force, format },
            } => {
                assert!(force);
                assert_eq!(format, StatsFormat::Text);
            }
            _ => panic!("expected db import --force command"),
        }

        let cli = Cli::try_parse_from(["budi", "db", "import", "--format", "json"])
            .expect("budi db import --format json parses");
        match cli.command {
            Commands::Db {
                action: DbAction::Import { force, format },
            } => {
                assert!(!force);
                assert_eq!(format, StatsFormat::Json);
            }
            _ => panic!("expected db import --format json command"),
        }
    }

    #[test]
    fn cli_rejects_removed_db_bare_verbs() {
        // #428 — the pre-8.2.1 bare verbs `budi migrate` / `budi repair` /
        // `budi import` were removed in 8.3.0 after shipping in 8.2.x as
        // hidden deprecation aliases. Users must now reach these via the
        // `budi db` namespace. Regression guard so they don't quietly come
        // back.
        assert!(Cli::try_parse_from(["budi", "migrate"]).is_err());
        assert!(Cli::try_parse_from(["budi", "repair"]).is_err());
        assert!(Cli::try_parse_from(["budi", "import"]).is_err());
        assert!(Cli::try_parse_from(["budi", "import", "--force"]).is_err());
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
            Commands::Doctor { deep, quiet, .. } => {
                assert!(deep);
                assert!(!quiet, "--deep should not imply --quiet");
            }
            _ => panic!("expected doctor command"),
        }
    }

    /// #487 (U-4): `--quiet` suppresses individual PASS lines on a
    /// green run. The flag is parsed in isolation here; the rendering
    /// contract (`CheckResult::print_respecting`) has its own unit
    /// coverage in `commands::doctor::tests`.
    #[test]
    fn cli_parses_doctor_quiet_flag() {
        let cli =
            Cli::try_parse_from(["budi", "doctor", "--quiet"]).expect("doctor --quiet parses");
        match cli.command {
            Commands::Doctor { deep, quiet, .. } => {
                assert!(quiet);
                assert!(!deep, "--quiet should not imply --deep");
            }
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
                action: CloudAction::Sync { format, full, yes },
            } => {
                assert!(matches!(format, StatsFormat::Text));
                assert!(!full, "default invocation must not drop watermarks");
                assert!(!yes, "default invocation must be interactive");
            }
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
    fn cli_parses_cloud_sync_full() {
        // #583: `--full` folds the prior `cloud reset && cloud sync`
        // two-step into a single verb. `--yes` is the non-interactive
        // escape hatch CI / scripts need so the confirmation prompt
        // isn't a hard block.
        let cli = Cli::try_parse_from(["budi", "cloud", "sync", "--full"])
            .expect("budi cloud sync --full parses");
        match cli.command {
            Commands::Cloud {
                action: CloudAction::Sync { full, yes, .. },
            } => {
                assert!(full, "--full must request a watermark drop");
                assert!(!yes, "--full alone must keep the confirmation prompt");
            }
            _ => panic!("expected cloud sync --full command"),
        }

        let cli = Cli::try_parse_from(["budi", "cloud", "sync", "--full", "--yes"])
            .expect("budi cloud sync --full --yes parses");
        match cli.command {
            Commands::Cloud {
                action: CloudAction::Sync { full, yes, .. },
            } => {
                assert!(full);
                assert!(yes, "--yes must skip the confirmation");
            }
            _ => panic!("expected cloud sync --full --yes command"),
        }

        let cli = Cli::try_parse_from(["budi", "cloud", "sync", "--full", "--format", "json"])
            .expect("budi cloud sync --full --format json parses");
        match cli.command {
            Commands::Cloud {
                action:
                    CloudAction::Sync {
                        full,
                        format,
                        yes: _,
                    },
            } => {
                assert!(full);
                assert!(matches!(format, StatsFormat::Json));
            }
            _ => panic!("expected cloud sync --full --format json command"),
        }
    }

    #[test]
    fn cli_parses_cloud_reset() {
        // #564: bare `budi cloud reset` parses and defaults to
        // interactive (yes=false). `--yes` is the non-interactive
        // escape hatch CI / scripts need so the prompt isn't a
        // hard block.
        let cli = Cli::try_parse_from(["budi", "cloud", "reset"]).expect("budi cloud reset parses");
        match cli.command {
            Commands::Cloud {
                action: CloudAction::Reset { yes },
            } => assert!(!yes, "default invocation must be interactive"),
            _ => panic!("expected cloud reset command"),
        }

        let cli = Cli::try_parse_from(["budi", "cloud", "reset", "--yes"])
            .expect("budi cloud reset --yes parses");
        match cli.command {
            Commands::Cloud {
                action: CloudAction::Reset { yes },
            } => assert!(yes, "--yes must skip the confirmation"),
            _ => panic!("expected cloud reset --yes command"),
        }
    }

    #[test]
    fn cli_parses_cloud_init_bare() {
        let cli = Cli::try_parse_from(["budi", "cloud", "init"]).expect("budi cloud init parses");
        match cli.command {
            Commands::Cloud {
                action:
                    CloudAction::Init {
                        api_key,
                        force,
                        yes,
                        device_id,
                        org_id,
                    },
            } => {
                assert!(api_key.is_none());
                assert!(!force);
                assert!(!yes);
                assert!(device_id.is_none());
                assert!(org_id.is_none());
            }
            _ => panic!("expected cloud init command"),
        }
    }

    #[test]
    fn cli_parses_cloud_init_with_flags() {
        let cli = Cli::try_parse_from([
            "budi",
            "cloud",
            "init",
            "--api-key",
            "fake-test-key",
            "--force",
            "--yes",
        ])
        .expect("budi cloud init --api-key --force --yes parses");
        match cli.command {
            Commands::Cloud {
                action:
                    CloudAction::Init {
                        api_key,
                        force,
                        yes,
                        device_id,
                        org_id,
                    },
            } => {
                assert_eq!(api_key.as_deref(), Some("fake-test-key"));
                assert!(force);
                assert!(yes);
                assert!(device_id.is_none());
                assert!(org_id.is_none());
            }
            _ => panic!("expected cloud init command"),
        }
    }

    #[test]
    fn cli_parses_cloud_init_manual_ids() {
        // #541: the escape hatch for offline installs / self-hosted
        // endpoints without /v1/whoami. `--device-id` / `--org-id`
        // bypass the whoami fetch and write the provided values
        // verbatim into the template.
        let cli = Cli::try_parse_from([
            "budi",
            "cloud",
            "init",
            "--api-key",
            "fake-test-key",
            "--device-id",
            "my-laptop",
            "--org-id",
            "org_selfhost",
        ])
        .expect("budi cloud init with manual ids parses");
        match cli.command {
            Commands::Cloud {
                action:
                    CloudAction::Init {
                        api_key,
                        device_id,
                        org_id,
                        ..
                    },
            } => {
                assert_eq!(api_key.as_deref(), Some("fake-test-key"));
                assert_eq!(device_id.as_deref(), Some("my-laptop"));
                assert_eq!(org_id.as_deref(), Some("org_selfhost"));
            }
            _ => panic!("expected cloud init command"),
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
    fn stats_period_parses_calendar_windows() {
        use std::str::FromStr;
        assert_eq!(StatsPeriod::from_str("today").unwrap(), StatsPeriod::Today);
        assert_eq!(StatsPeriod::from_str("Today").unwrap(), StatsPeriod::Today);
        assert_eq!(StatsPeriod::from_str("week").unwrap(), StatsPeriod::Week);
        assert_eq!(StatsPeriod::from_str("month").unwrap(), StatsPeriod::Month);
        assert_eq!(StatsPeriod::from_str("all").unwrap(), StatsPeriod::All);
    }

    #[test]
    fn stats_period_parses_relative_windows() {
        use std::str::FromStr;
        assert_eq!(
            StatsPeriod::from_str("1d").unwrap(),
            StatsPeriod::Days(1),
            "1d should parse as a 1-day rolling window"
        );
        assert_eq!(StatsPeriod::from_str("7d").unwrap(), StatsPeriod::Days(7));
        assert_eq!(
            StatsPeriod::from_str("30D").unwrap(),
            StatsPeriod::Days(30),
            "unit suffix should be case-insensitive"
        );
        assert_eq!(StatsPeriod::from_str("1w").unwrap(), StatsPeriod::Weeks(1));
        assert_eq!(StatsPeriod::from_str("2w").unwrap(), StatsPeriod::Weeks(2));
        assert_eq!(StatsPeriod::from_str("1m").unwrap(), StatsPeriod::Months(1));
        assert_eq!(StatsPeriod::from_str("3m").unwrap(), StatsPeriod::Months(3));
        assert_eq!(
            StatsPeriod::from_str(" 7d ").unwrap(),
            StatsPeriod::Days(7),
            "whitespace should be trimmed"
        );
    }

    #[test]
    fn stats_period_rejects_invalid_input() {
        use std::str::FromStr;

        // Zero is rejected with a hint rather than silently collapsing
        // the window to "today" (0d) and producing confusing stats.
        assert!(StatsPeriod::from_str("0d").is_err());
        assert!(StatsPeriod::from_str("0w").is_err());
        assert!(StatsPeriod::from_str("0m").is_err());

        // Empty / whitespace / missing count.
        assert!(StatsPeriod::from_str("").is_err());
        assert!(StatsPeriod::from_str("   ").is_err());
        assert!(StatsPeriod::from_str("d").is_err());
        assert!(StatsPeriod::from_str("w").is_err());

        // Unknown unit.
        assert!(StatsPeriod::from_str("7y").is_err());
        assert!(StatsPeriod::from_str("7h").is_err());

        // Non-numeric count.
        assert!(StatsPeriod::from_str("abcd").is_err());
        assert!(StatsPeriod::from_str("-1d").is_err());

        // Multi-byte UTF-8 input must not panic (`split_at` byte
        // safety — regression guard for the pre-#404 implementation).
        assert!(StatsPeriod::from_str("1日").is_err());
        assert!(StatsPeriod::from_str("日").is_err());
    }

    #[test]
    fn cli_stats_parses_relative_period_flag() {
        let cli = Cli::try_parse_from(["budi", "stats", "-p", "7d"])
            .expect("budi stats -p 7d should parse");
        match cli.command {
            Commands::Stats { period, .. } => assert_eq!(period, StatsPeriod::Days(7)),
            _ => panic!("expected stats command"),
        }

        let cli = Cli::try_parse_from(["budi", "stats", "--period", "2w"])
            .expect("budi stats --period 2w should parse");
        match cli.command {
            Commands::Stats { period, .. } => assert_eq!(period, StatsPeriod::Weeks(2)),
            _ => panic!("expected stats command"),
        }

        let cli = Cli::try_parse_from(["budi", "stats", "-p", "1m"])
            .expect("budi stats -p 1m should parse");
        match cli.command {
            Commands::Stats { period, .. } => assert_eq!(period, StatsPeriod::Months(1)),
            _ => panic!("expected stats command"),
        }

        // Invalid relative period must be rejected by clap with a clear
        // message (the `FromStr::Err` string is surfaced by clap).
        assert!(Cli::try_parse_from(["budi", "stats", "-p", "0d"]).is_err());
        assert!(Cli::try_parse_from(["budi", "stats", "-p", "7y"]).is_err());
    }

    #[test]
    fn cli_sessions_parses_relative_period_flag() {
        let cli = Cli::try_parse_from(["budi", "sessions", "-p", "7d"])
            .expect("budi sessions -p 7d should parse");
        match cli.command {
            Commands::Sessions { period, .. } => assert_eq!(period, StatsPeriod::Days(7)),
            _ => panic!("expected sessions command"),
        }

        let cli = Cli::try_parse_from(["budi", "sessions", "--period", "2w"])
            .expect("budi sessions --period 2w should parse");
        match cli.command {
            Commands::Sessions { period, .. } => assert_eq!(period, StatsPeriod::Weeks(2)),
            _ => panic!("expected sessions command"),
        }
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

    /// #485: `budi statusline --format text` should parse as the
    /// default (Claude) render so a fresh user doesn't have to
    /// remember that statusline's format vocabulary differs from
    /// `budi stats` / `budi sessions`. Added via `#[value(alias = "text")]`
    /// on `StatuslineFormat::Claude`.
    #[test]
    fn cli_statusline_accepts_text_as_claude_alias() {
        let cli = Cli::try_parse_from(["budi", "statusline", "--format", "text"])
            .expect("budi statusline --format text should parse");
        match cli.command {
            Commands::Statusline { format, .. } => {
                assert_eq!(format, StatuslineFormat::Claude);
            }
            _ => panic!("expected statusline command"),
        }

        // Explicit `--format claude` still parses as Claude (no
        // regression from the alias addition).
        let cli = Cli::try_parse_from(["budi", "statusline", "--format", "claude"])
            .expect("budi statusline --format claude should parse");
        match cli.command {
            Commands::Statusline { format, .. } => {
                assert_eq!(format, StatuslineFormat::Claude);
            }
            _ => panic!("expected statusline command"),
        }

        // Other non-default formats still resolve to their own
        // variants — alias only applies to the default.
        let cli = Cli::try_parse_from(["budi", "statusline", "--format", "json"])
            .expect("budi statusline --format json should parse");
        match cli.command {
            Commands::Statusline { format, .. } => {
                assert_eq!(format, StatuslineFormat::Json);
            }
            _ => panic!("expected statusline command"),
        }
    }
}
