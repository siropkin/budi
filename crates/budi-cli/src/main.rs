use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use tracing_subscriber::EnvFilter;

mod client;
mod commands;
mod daemon;

use crate::commands::integrations::IntegrationComponent;

const HEALTH_TIMEOUT_SECS: u64 = 3;

#[derive(Debug, Parser)]
#[command(name = "budi")]
#[command(about = "budi — AI cost analytics. Know where your tokens and money go.")]
#[command(version)]
#[command(
    after_help = "Get started:\n  budi init\n\nCommon commands:\n  budi stats              Show today's cost summary\n  budi stats models       Cost breakdown by model\n  budi stats branches     Cost breakdown by branch\n  budi sessions           List recent sessions with cost and vitals\n  budi sessions <id>      Session detail: cost, models, vitals, tags\n  budi sessions latest    Detail + vitals for the most recent session\n  budi sessions current   Vitals for the active Claude Code session in this cwd (used by /budi)\n  budi status             Quick check: daemon and today's spend\n  budi doctor             Full diagnostic: daemon, tailer, schema, transcript visibility\n  budi cloud status       Cloud sync readiness and last-synced-at\n  budi cloud sync         Push queued local data to the cloud now\n  budi autostart status   Check daemon autostart service\n  budi db import          Import historical transcripts from disk\n  budi db import --force  Re-ingest all data from scratch (use after upgrades)\n  budi db check           Verify schema; report drift (read-only)\n  budi db check --fix     Verify + auto-repair drift and run migrations\n\nMore info: https://github.com/siropkin/budi"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Set up budi (daemon + autostart) and show detected agents.
    Init {
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
        /// Output format: text (default) or json
        #[arg(short, long, value_enum, default_value_t = StatsFormat::Text)]
        format: StatsFormat,
        #[arg(long, hide = true)]
        repo_root: Option<PathBuf>,
    },
    /// Show usage analytics. Bare `budi stats` prints today's summary; subcommands drill into specific views.
    #[command(after_help = "\
Examples:
  budi stats                       Today's cost summary (default)
  budi stats -p week               This week's summary
  budi stats projects -p all       All-time project costs
  budi stats branches              Branches ranked by cost (today)
  budi stats branch main           Cost details for a specific branch
  budi stats branch main --repo github.com/acme/app
  budi stats tickets               Tickets ranked by cost (today)
  budi stats ticket ENG-123        Cost details for a specific ticket
  budi stats activities            Activities ranked by cost (today)
  budi stats activity bugfix       Cost details for a specific activity
  budi stats files                 Files ranked by cost (today)
  budi stats file src/main.rs      Cost details for a specific file
  budi stats models                Model usage breakdown
  budi stats tag activity          Raw cost breakdown by the activity tag
  budi stats --provider cursor     Filter summary to Cursor only
  budi stats --format json         JSON output for scripting")]
    Stats(StatsArgs),
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
    /// Database admin commands (check schema, import historical transcripts)
    ///
    /// Groups check / import under a single namespace. The pre-8.2.1
    /// bare verbs (`budi migrate` / `budi repair` / `budi import`) were
    /// removed in 8.3.0; the 8.2.x `db migrate` / `db repair` verbs
    /// were collapsed into `db check [--fix]` in 8.3.14.
    #[command(after_help = "\
Examples:
  budi db check                  Verify schema; report drift (read-only)
  budi db check --fix            Verify + auto-repair drift and run migrations
  budi db import                 Import historical transcripts from disk
  budi db import --force         Re-ingest all data from scratch (use after upgrades)
  budi db import --format json   JSON output with per-agent breakdown (for scripting)")]
    Db {
        #[command(subcommand)]
        action: DbAction,
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
  budi sessions <session-id>       Show detail + vitals for a specific session
  budi sessions latest             Show detail + vitals for the most recent session
  budi sessions current            Show vitals for the active Claude Code session in the current cwd
  budi sessions --format json      JSON output for scripting")]
    Sessions {
        /// Session ID for detail view, or one of the literal tokens
        /// `latest` (newest session in the DB) / `current` (active
        /// Claude Code session for this cwd, used by the auto-installed
        /// `/budi` skill). Omit for the session list.
        #[arg()]
        session_id: Option<String>,
        /// Time period for session list (today, week, month, all, or relative like 1d, 7d, 1m)
        #[arg(long, short, default_value = "today")]
        period: StatsPeriod,
        /// Filter sessions by search term (model, repo, branch, provider)
        #[arg(long)]
        search: Option<String>,
        /// Filter sessions to a single provider (e.g. `copilot_chat`,
        /// `claude_code`). Unlike `--search`, this is an exact match on
        /// the provider field — `--search code` would also match
        /// `claude_code`/`vscode`/`codex`.
        #[arg(long, value_name = "NAME")]
        provider: Option<String>,
        /// Filter sessions by host environment (`vscode`, `cursor`,
        /// `jetbrains`, `terminal`, `unknown`). Repeat the flag or pass a
        /// CSV (`--surface vscode,cursor`) to combine. `provider` answers
        /// "which agent"; `--surface` answers "which host".
        #[arg(long, value_name = "NAME", value_delimiter = ',')]
        surface: Vec<String>,
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
    Status {
        /// Output format: text (default) or json
        #[arg(short, long, value_enum, default_value_t = StatsFormat::Text)]
        format: StatsFormat,
    },
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
  budi statusline --slots session,message      Override config slots from the command line
  budi statusline --install                    Install budi into the Claude Code status line")]
    Statusline {
        /// Install the status line in ~/.claude/settings.json
        #[arg(long, default_value_t = false)]
        install: bool,
        /// Output format: claude (ANSI+OSC8), starship (plain text), json, custom (uses config template)
        #[arg(long, value_enum, default_value_t = StatuslineFormat::Claude)]
        format: StatuslineFormat,
        /// Scope all costs to a single provider (claude_code, cursor, codex, copilot_cli, copilot_chat).
        /// Defaults to `claude_code` when `--format claude` is used so the
        /// Claude Code statusline never shows blended multi-provider totals.
        #[arg(long)]
        provider: Option<String>,
        /// Comma-separated slot list (e.g. `session,message`). Overrides
        /// `~/.config/budi/statusline.toml` slots/preset/format for this
        /// invocation. Known slots: 1d, 7d, 30d, session, message, branch,
        /// project, provider (legacy: today, week, month).
        #[arg(long, value_name = "SLOTS")]
        slots: Option<String>,
    },
    /// Manage optional integrations (install later, list current status)
    Integrations {
        #[command(subcommand)]
        action: IntegrationAction,
    },
    /// Manage the daemon autostart service (launchd / systemd / Task Scheduler)
    #[command(after_help = "\
Examples:
  budi autostart status              Check if autostart is installed and running
  budi autostart status --format json  JSON output for scripting
  budi autostart install             Install the autostart service
  budi autostart uninstall           Remove the autostart service")]
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
  budi cloud reset --yes         Same, non-interactive (CI / scripts)
  budi cloud reset --format json JSON output (requires --yes for non-TTY)")]
    Cloud {
        #[command(subcommand)]
        action: CloudAction,
    },
    /// Pricing manifest: view status (read-only) or sync from upstream (network)
    ///
    /// Bare `budi pricing` defaults to the read-only `pricing status` view.
    /// `pricing sync` is the network-touching verb that fetches the latest
    /// LiteLLM manifest, mirroring the `cloud sync` shape on the cloud
    /// namespace — both are direction-tagged data movements, both come
    /// with `--format json` for scripted use.
    #[command(after_help = "\
Examples:
  budi pricing                       Show current manifest layer, version, and unknown models (read-only)
  budi pricing status                Same as bare `budi pricing` (long form)
  budi pricing sync                  Fetch the latest LiteLLM manifest into the local cache
  budi pricing recompute             Re-poll the org price list and recompute effective costs
  budi pricing recompute --force     Run the recompute pass even if list_version is unchanged
  budi pricing --format json         JSON output for scripting
  budi pricing sync --format json    JSON output (exit code 2 on refresh failure)")]
    Pricing(PricingArgs),
}

/// Top-level args for `budi stats`. Bare invocation (no subcommand) renders
/// the default summary scoped by `--period` / `--provider` / `--format`.
/// Subcommands drill into a specific view; their flags are global so they
/// parse equivalently before or after the subcommand name.
#[derive(Debug, clap::Args)]
pub struct StatsArgs {
    #[command(subcommand)]
    pub view: Option<StatsView>,
    #[command(flatten)]
    pub opts: StatsOpts,
}

/// Shared output / filter knobs for every `budi stats` view. Every flag is
/// `global = true` so it parses at any depth (`budi stats -p week projects`
/// and `budi stats projects -p week` are equivalent).
#[derive(Debug, clap::Args)]
pub struct StatsOpts {
    /// Time period to show (today, week, month, all, or relative like 1d, 7d, 1m)
    #[arg(long, short, default_value = "today", global = true)]
    pub period: StatsPeriod,
    /// Optional repository filter for `branch`, `ticket`, `activity`, or
    /// `file` detail views (recommended when names repeat across repos).
    #[arg(long, global = true)]
    pub repo: Option<String>,
    /// Filter by provider (e.g. claude_code, cursor, codex, copilot_cli, copilot_chat, openai). Applies to the default summary view and every breakdown subcommand (`models`, `projects`, `branches`, `tickets`, `activities`, `files`).
    #[arg(long, global = true)]
    pub provider: Option<String>,
    /// Filter by host environment (`vscode`, `cursor`, `jetbrains`, `terminal`, `unknown`). Applies to the default summary view and every breakdown subcommand (`models`, `projects`, `branches`, `tickets`, `activities`, `files`, `surfaces`). Repeat or pass CSV to combine.
    #[arg(long, global = true, value_name = "NAME", value_delimiter = ',')]
    pub surface: Vec<String>,
    /// Maximum rows in breakdown views (`projects`, `branches`, `tickets`,
    /// `activities`, `files`, `models`, `tag`). `0` = no cap. Truncated
    /// rows collapse into an `(other N: $X)` aggregate so the Total
    /// footer always reconciles to the cent.
    #[arg(long, default_value_t = 30, global = true)]
    pub limit: usize,
    /// Maximum characters for labels and label-like extra columns in
    /// breakdown views. Values longer than this truncate with a middle
    /// ellipsis (`…`).
    #[arg(long, default_value_t = 40, global = true)]
    pub label_width: usize,
    /// Include zero-cost `(model not yet attributed)` rows in the
    /// `models` view. By default Cursor-lag transient rows are collapsed
    /// into a suppressed-count footnote.
    #[arg(long, default_value_t = false, global = true)]
    pub include_pending: bool,
    /// Break out the `(no repository)` bucket in `projects` into a
    /// per-folder breakdown keyed on the cwd basename.
    #[arg(long, default_value_t = false, global = true)]
    pub include_non_repo: bool,
    /// Output format: text (default) or json
    #[arg(short, long, value_enum, default_value_t = StatsFormat::Text, global = true)]
    pub format: StatsFormat,
}

/// Drill-in views for `budi stats`. Each variant maps to one of the
/// pre-8.3.14 mutually-exclusive view flags. Singular variants take an
/// argument (the specific entity to drill into); plural variants render
/// a ranked breakdown.
#[derive(Debug, Subcommand)]
pub enum StatsView {
    /// Repositories ranked by cost
    Projects,
    /// Branches ranked by cost
    Branches,
    /// Cost details for a specific branch
    Branch {
        /// Branch name (e.g. `main`)
        name: String,
    },
    /// Tickets ranked by cost
    Tickets,
    /// Cost details for a specific ticket
    Ticket {
        /// Ticket id (e.g. ENG-123)
        id: String,
    },
    /// Activities ranked by cost
    Activities,
    /// Cost details for a specific activity
    Activity {
        /// Activity name (e.g. `bugfix`, `refactor`)
        name: String,
    },
    /// Files ranked by cost
    Files,
    /// Cost details for a specific file
    File {
        /// Repo-relative file path (forward-slashed, inside the repo root)
        path: String,
    },
    /// Cost breakdown by model
    Models,
    /// Cost breakdown by host environment (vscode / cursor / jetbrains / terminal / unknown).
    /// Mirrors the per-provider Agents block but keyed on the surface axis.
    Surfaces,
    /// Raw cost breakdown by tag KEY (escape hatch for custom tag keys)
    Tag {
        /// Tag key (e.g. `ticket_id`, `activity`, or any custom key)
        key: String,
    },
}

/// Top-level args for `budi pricing`. Bare invocation (no subcommand) renders
/// the read-only manifest status — same output as the explicit `pricing status`.
/// `pricing sync` is the only network-touching verb in this namespace; it
/// replaces the pre-8.3.14 `pricing status --refresh` flag (the lone
/// side-effecting flag in the entire CLI that hid behind a read-only verb).
#[derive(Debug, clap::Args)]
pub struct PricingArgs {
    #[command(subcommand)]
    pub view: Option<PricingView>,
    /// Output format: text (default) or json
    #[arg(short, long, value_enum, default_value_t = StatsFormat::Text, global = true)]
    pub format: StatsFormat,
}

#[derive(Debug, Subcommand)]
pub enum PricingView {
    /// Read-only: show manifest layer, version, known model count, unknown models
    Status,
    /// Network: fetch the latest LiteLLM manifest into the local cache, then show state
    Sync,
    /// Network: re-poll the cloud price list and recompute effective costs
    Recompute {
        /// Re-run the recompute pass even when `list_version` is unchanged
        #[arg(long)]
        force: bool,
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
        /// Skip prompts and use defaults
        #[arg(long, default_value_t = false)]
        yes: bool,
    },
    /// Re-apply the user's enabled integrations + any newly-default components.
    ///
    /// #613: this is the post-install hook that `budi update` re-execs against
    /// the freshly-installed CLI so any new IntegrationComponents (e.g. the
    /// `/budi` skill added in #603) or seeded files (e.g. `statusline.toml`
    /// from #600) actually land for upgrading users. Idempotent and silent
    /// on already-installed surfaces.
    Refresh,
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
    /// running `budi cloud reset && budi cloud sync` in one step. The
    /// re-upload is safe — cloud-side dedup collapses any records that
    /// overlap with what the cloud already has.
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
        /// Output format: text (default) or json
        #[arg(short, long, value_enum, default_value_t = StatsFormat::Text)]
        format: StatsFormat,
    },
}

#[derive(Debug, Subcommand)]
enum DbAction {
    /// Verify schema and report drift; pass --fix to auto-repair
    Check {
        /// Apply migrations and additive repairs in addition to checking.
        /// Without this flag, `db check` is a read-only diagnostic that
        /// exits non-zero when drift or a pending migration is detected.
        #[arg(long, default_value_t = false)]
        fix: bool,
    },
    /// Import historical transcripts from Claude Code, Codex, Copilot CLI, and Cursor into the analytics database.
    ///
    /// Backfills pre-existing transcripts the daemon seeded as history on
    /// first boot. The live tailer skips bytes that pre-date budi
    /// installation so it never re-emits old content as new; `budi db
    /// import` is the documented way to bring that history into the
    /// analytics database. `budi doctor` surfaces a corresponding hint
    /// when there is something to backfill.
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
    Status {
        /// Output format: text (default) or json
        #[arg(short, long, value_enum, default_value_t = StatsFormat::Text)]
        format: StatsFormat,
    },
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
            no_integrations,
            no_daemon,
        } => commands::init::cmd_init(no_integrations, no_daemon),
        Commands::Doctor {
            deep,
            quiet,
            format,
            repo_root,
        } => commands::doctor::cmd_doctor(repo_root, deep, quiet, format),
        Commands::Stats(args) => {
            let StatsArgs { view, opts } = args;
            let StatsOpts {
                period,
                repo,
                provider,
                surface,
                limit,
                label_width,
                include_pending,
                include_non_repo,
                format,
            } = opts;
            let json_output = matches!(format, StatsFormat::Json);
            // Translate the new subcommand shape to the existing
            // `cmd_stats` dispatch — the helper still drives the
            // per-view rendering, we just project `view` back into the
            // legacy boolean / Option arguments it expects.
            let mut projects = false;
            let mut branches = false;
            let mut branch: Option<String> = None;
            let mut tickets = false;
            let mut ticket: Option<String> = None;
            let mut activities = false;
            let mut activity: Option<String> = None;
            let mut files = false;
            let mut file: Option<String> = None;
            let mut models = false;
            let mut surfaces = false;
            let mut tag: Option<String> = None;
            match view {
                None => {}
                Some(StatsView::Projects) => projects = true,
                Some(StatsView::Branches) => branches = true,
                Some(StatsView::Branch { name }) => branch = Some(name),
                Some(StatsView::Tickets) => tickets = true,
                Some(StatsView::Ticket { id }) => ticket = Some(id),
                Some(StatsView::Activities) => activities = true,
                Some(StatsView::Activity { name }) => activity = Some(name),
                Some(StatsView::Files) => files = true,
                Some(StatsView::File { path }) => file = Some(path),
                Some(StatsView::Models) => models = true,
                Some(StatsView::Surfaces) => surfaces = true,
                Some(StatsView::Tag { key }) => tag = Some(key),
            }
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
                surfaces,
                provider,
                surface,
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
            DbAction::Check { fix } => commands::db::cmd_db_check(fix),
            DbAction::Import { force, format } => {
                let json = matches!(format, StatsFormat::Json);
                commands::import::cmd_import(force, json)
            }
        },
        Commands::Statusline {
            install,
            format,
            provider,
            slots,
        } => {
            if install {
                commands::statusline::cmd_statusline_install()
            } else {
                commands::statusline::cmd_statusline(format, provider, slots)
            }
        }
        Commands::Sessions {
            session_id,
            period,
            search,
            provider,
            surface,
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
                let provider = provider
                    .map(|p| commands::normalize_provider(&p))
                    .transpose()?;
                let surfaces: Vec<String> = surface
                    .iter()
                    .map(|s| commands::normalize_surface(s))
                    .collect::<Result<_>>()?;
                commands::sessions::cmd_sessions(
                    period,
                    search.as_deref(),
                    provider.as_deref(),
                    &surfaces,
                    ticket.as_deref(),
                    activity.as_deref(),
                    limit,
                    full_uuid,
                    json_output,
                )
            }
        }
        Commands::Status { format } => commands::status::cmd_status(format),
        Commands::Integrations { action } => commands::integrations::cmd_integrations(action),
        Commands::Autostart { action } => match action {
            AutostartAction::Status { format } => commands::autostart::cmd_autostart_status(format),
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
            CloudAction::Reset { yes, format } => commands::cloud::cmd_cloud_reset(yes, format),
        },
        Commands::Pricing(args) => {
            let PricingArgs { view, format } = args;
            match view {
                None | Some(PricingView::Status) => commands::pricing::cmd_pricing_status(format),
                Some(PricingView::Sync) => commands::pricing::cmd_pricing_sync(format),
                Some(PricingView::Recompute { force }) => {
                    commands::pricing::cmd_pricing_recompute(format, force)
                }
            }
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "main/tests.rs"]
mod tests;
