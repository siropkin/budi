//! `budi cloud` subcommands — manual cloud sync and freshness reporting.
//!
//! 8.1 R2.1 (issue #225) introduces `budi cloud sync` as the user-facing way
//! to say "push my latest local data to cloud now" without waiting for the
//! background worker interval (ADR-0083 §9). `budi cloud status` shows the
//! same readiness snapshot the daemon exposes at `GET /cloud/status` so users
//! can answer "is cloud sync healthy?" without reading logs. 8.3 R F (issue
//! #446) adds `budi cloud init` so a fresh user never has to hand-write the
//! `cloud.toml` schema.
//!
//! Both commands follow the normalized CLI output contract shared with
//! `budi stats` / `budi sessions`:
//! - `--format text` is the default, human-readable with ✓/✗ status lines.
//! - `--format json` emits the daemon response body verbatim for scripting.

use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use budi_core::cloud_sync::{WhoamiOutcome, whoami};
use budi_core::config::{CLOUD_API_KEY_STUB, CloudConfig, cloud_config_path, load_cloud_config};
use serde_json::Value;

use crate::StatsFormat;
use crate::client::DaemonClient;

use super::ansi;

const DEFAULT_CLOUD_ENDPOINT: &str = "https://app.getbudi.dev";

/// `budi cloud init` — generate `~/.config/budi/cloud.toml` from the
/// documented template so a fresh user never has to hand-write the
/// schema from ADR-0083 §9.
///
/// 8.3.4 (#521) auto-seeds `device_id` on a subsequent `budi init`.
/// 8.3.5 (#541) goes further: when `--api-key KEY` is supplied and
/// `--org-id` isn't set manually, the CLI calls `GET /v1/whoami` to
/// resolve the `org_id` for that key and writes both identity fields
/// into the template inline. `--device-id` / `--org-id` are escape
/// hatches for offline installs or self-hosted clouds that don't
/// expose `/v1/whoami`. When `whoami` fails (401, 404, network
/// error), the CLI falls back to the pre-#541 commented-placeholder
/// template so a transient cloud outage never blocks a user from
/// writing a config file.
///
/// 8.3.9 (#559) makes the relink path (org switch / API key rotation)
/// seamless: when `--api-key KEY` is supplied interactively against an
/// existing `cloud.toml`, the CLI prompts to overwrite instead of
/// hard-erroring. Non-TTY callers still need `--force` as the explicit
/// escape hatch; the error copy now names the existing org so the user
/// recognizes what they're about to replace.
pub fn cmd_cloud_init(
    api_key: Option<String>,
    force: bool,
    yes: bool,
    device_id: Option<String>,
    org_id: Option<String>,
) -> Result<()> {
    let path = cloud_config_path().context("failed to resolve ~/.config/budi/cloud.toml path")?;

    let existed = path.exists();
    if existed {
        let existing = load_cloud_config();
        let has_real_key = existing
            .api_key
            .as_deref()
            .map(|k| !k.is_empty() && k != CLOUD_API_KEY_STUB)
            .unwrap_or(false);
        let user_supplied_new_key = api_key
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_some();

        if !force {
            // #559: `--api-key KEY` against an existing file reads as
            // "I'm relinking" (org switch / API key rotation). In a TTY
            // we prompt and let the user accept the overwrite without
            // having to learn about `--force`. Non-TTY callers (CI,
            // scripts) still hit the error path so an automated run can
            // never silently clobber a working config.
            if user_supplied_new_key && io::stdin().is_terminal() {
                if !confirm_relink(&path, &existing)? {
                    println!("Aborted. {} left unchanged.", path.display());
                    return Ok(());
                }
                // Confirmed — fall through to the write path.
            } else {
                return Err(rotation_aware_already_exists_error(&path, &existing));
            }
        } else if has_real_key && !yes && !confirm_overwrite(&path)? {
            // `--force` overwrite: if the current api_key looks real, require
            // --yes or an interactive confirmation so a stray invocation doesn't
            // silently clobber a working config. Stub keys / no keys overwrite
            // freely because the prior install was never going to sync.
            println!("Aborted. {} left unchanged.", path.display());
            return Ok(());
        }
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    // Normalize --api-key: empty or whitespace-only strings are treated as
    // "not provided", so `--api-key ""` behaves like a bare `budi cloud init`.
    let trimmed_key = api_key.as_deref().map(str::trim).filter(|s| !s.is_empty());

    let (key_value, enabled) = match trimmed_key {
        Some(k) => (k.to_string(), true),
        None => (CLOUD_API_KEY_STUB.to_string(), false),
    };

    // #541: when a real key is on-hand, try to auto-seed device_id +
    // org_id. Flags override the automatic fetch. The whoami outcome
    // lets us distinguish "key is bad, don't enable cloud" from "cloud
    // doesn't expose /v1/whoami, fall through".
    let seed = match trimmed_key {
        Some(k) => resolve_seed(k, device_id, org_id),
        None => SeedOutcome::Skipped,
    };

    // If whoami rejected the key outright, DON'T write `enabled = true`
    // — leave the template honest and tell the user.
    let seed_for_template = match &seed {
        SeedOutcome::KeyRejected => None,
        other => other.seeded_ids(),
    };
    let effective_enabled = enabled && !matches!(seed, SeedOutcome::KeyRejected);

    let rendered =
        render_cloud_toml_template(&key_value, effective_enabled, seed_for_template.as_ref());
    fs::write(&path, rendered).with_context(|| format!("failed to write {}", path.display()))?;

    render_init_text(&path, existed, effective_enabled, &seed);
    Ok(())
}

/// Identity pair seeded into the new `cloud.toml`. Each id is tagged
/// with its provenance so the init output can say
/// "device_id auto-generated" vs "device_id from --device-id flag".
#[derive(Debug, Clone)]
pub(crate) struct SeededIdentity {
    pub device_id: String,
    pub device_id_source: IdentitySource,
    pub org_id: Option<String>,
    pub org_id_source: Option<IdentitySource>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdentitySource {
    /// Generated via UUID v4 in this process.
    Generated,
    /// Pulled from `GET /v1/whoami`.
    Whoami,
    /// Passed explicitly via `--device-id` / `--org-id`.
    Flag,
}

/// Outcome of the #541 seeding flow, carried from `cmd_cloud_init` down
/// to the template renderer + the human-facing printout.
#[derive(Debug, Clone)]
pub(crate) enum SeedOutcome {
    /// No `--api-key` supplied — seeding not attempted (pre-#541 behavior).
    Skipped,
    /// Cloud auth rejected the key. Template reverts to disabled + stub.
    KeyRejected,
    /// Cloud returned 404/405 — endpoint not present on this deployment.
    EndpointAbsent {
        status: u16,
        device_id: String,
        device_id_source: IdentitySource,
    },
    /// Network / 5xx / parse failure talking to the cloud.
    TransientError {
        detail: String,
        device_id: String,
        device_id_source: IdentitySource,
    },
    /// Happy path: device_id + org_id both resolved.
    Seeded(SeededIdentity),
}

impl SeedOutcome {
    fn seeded_ids(&self) -> Option<SeededIdentity> {
        match self {
            SeedOutcome::Seeded(ids) => Some(ids.clone()),
            SeedOutcome::EndpointAbsent {
                device_id,
                device_id_source,
                ..
            }
            | SeedOutcome::TransientError {
                device_id,
                device_id_source,
                ..
            } => Some(SeededIdentity {
                device_id: device_id.clone(),
                device_id_source: *device_id_source,
                org_id: None,
                org_id_source: None,
            }),
            SeedOutcome::Skipped | SeedOutcome::KeyRejected => None,
        }
    }
}

/// #541: resolve the device_id + org_id for a fresh cloud.toml.
/// - `--device-id` overrides UUID v4 generation.
/// - `--org-id` overrides the whoami fetch entirely (skips the network
///   call — useful for offline installs).
/// - Otherwise we call `GET /v1/whoami` against the default endpoint.
pub(crate) fn resolve_seed(
    api_key: &str,
    device_id_flag: Option<String>,
    org_id_flag: Option<String>,
) -> SeedOutcome {
    let (device_id, device_id_source) = match device_id_flag {
        Some(id) => (id, IdentitySource::Flag),
        None => (uuid::Uuid::new_v4().to_string(), IdentitySource::Generated),
    };

    if let Some(org_id_override) = org_id_flag {
        return SeedOutcome::Seeded(SeededIdentity {
            device_id,
            device_id_source,
            org_id: Some(org_id_override),
            org_id_source: Some(IdentitySource::Flag),
        });
    }

    match whoami(DEFAULT_CLOUD_ENDPOINT, api_key) {
        WhoamiOutcome::Ok(resp) => SeedOutcome::Seeded(SeededIdentity {
            device_id,
            device_id_source,
            org_id: Some(resp.org_id),
            org_id_source: Some(IdentitySource::Whoami),
        }),
        WhoamiOutcome::Unauthorized => SeedOutcome::KeyRejected,
        WhoamiOutcome::EndpointAbsent(status) => SeedOutcome::EndpointAbsent {
            status,
            device_id,
            device_id_source,
        },
        WhoamiOutcome::TransientError(detail) => SeedOutcome::TransientError {
            detail,
            device_id,
            device_id_source,
        },
    }
}

/// Build the on-disk template text. Kept pure and public-to-crate so the
/// unit tests can pin the shape without going through the filesystem.
///
/// `seeded` is `Some` when `budi cloud init --api-key KEY` resolved the
/// device_id (+ optionally org_id via whoami) in-process. The two ids
/// then land as uncommented TOML lines instead of the commented
/// placeholders; if only device_id resolved (e.g. whoami fell through
/// to `EndpointAbsent` / `TransientError`), org_id stays commented.
pub(crate) fn render_cloud_toml_template(
    api_key: &str,
    enabled: bool,
    seeded: Option<&SeededIdentity>,
) -> String {
    // Heavily commented so a user reading the file understands every field
    // without cross-referencing ADR-0083. Keep lines <= 78 cols so a
    // standard terminal pager doesn't wrap.
    let enabled_line = if enabled { "true" } else { "false" };
    let today = chrono::Utc::now().format("%Y-%m-%d");

    let (device_id_line, org_id_line) = match seeded {
        Some(ids) => (
            format!("device_id = \"{}\"", ids.device_id),
            match ids.org_id.as_deref() {
                Some(v) => format!("org_id = \"{v}\""),
                None => "# org_id = \"your-org-id\"".to_string(),
            },
        ),
        None => (
            "# device_id = \"your-device-id\"".to_string(),
            "# org_id = \"your-org-id\"".to_string(),
        ),
    };

    format!(
        "\
# budi cloud sync configuration (ADR-0083 §9)
# Generated by `budi cloud init` on {today}.
# Cloud sync is off by default. See https://app.getbudi.dev for an account.

[cloud]
# Flip to `true` after pasting a real api_key below. `budi cloud sync` is
# a no-op while `enabled = false` or while api_key is the placeholder.
enabled = {enabled_line}

# Paste your API key from Settings → API keys at https://app.getbudi.dev.
# `budi cloud status` will report \"disabled (stub key)\" while this still
# reads `PASTE_YOUR_KEY_HERE`, so you can tell at a glance whether this
# step is done.
api_key = \"{api_key}\"

# Endpoint for the cloud API. Leave the default unless you are testing a
# self-hosted ingester.
endpoint = \"https://app.getbudi.dev\"

# These two fields identify which device and workspace the daily rollups
# belong to on the dashboard.
#
# When `budi cloud init --api-key KEY` is run with a real key, both are
# auto-seeded: `device_id` from a fresh UUID v4, `org_id` from the
# cloud's `/v1/whoami` endpoint. Pass `--device-id` / `--org-id` to
# override either lookup (useful on multi-machine setups or self-
# hosted clouds that don't expose `/v1/whoami`). `budi cloud sync`
# refuses to run until both fields are present (see `budi cloud status`).
{device_id_line}
{org_id_line}

# Optional human-friendly label shown on the cloud Devices page (#552).
# When commented out, budi defaults to this machine's OS hostname.
# Uncomment and set a custom string to override. Set `label = \"\"` if you
# want to opt out of sharing a readable label entirely.
# label = \"ivan-mbp\"

[cloud.sync]
# Background sync interval in seconds. Defaults to 300 (5 minutes).
interval_seconds = 300
# Upper bound on retry backoff in seconds. Defaults to 300.
retry_max_seconds = 300
"
    )
}

fn confirm_overwrite(path: &Path) -> Result<bool> {
    // We print to stdout rather than stderr so piped consumers that
    // dropped stdin still see the prompt line alongside the aborted
    // message. If stdin is not a TTY, treat the answer as "no" so an
    // automated invocation without `--yes` never silently destroys a
    // working config.
    if !io::stdin().is_terminal() {
        return Ok(false);
    }
    print!(
        "{} already exists with a non-stub api_key. Overwrite? [y/N] ",
        path.display()
    );
    io::stdout().flush().ok();
    let mut answer = String::new();
    if io::stdin().read_line(&mut answer).is_err() {
        return Ok(false);
    }
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// #559: TTY prompt fired when `budi cloud init --api-key KEY` runs
/// against an existing `cloud.toml`. Names the org currently linked so
/// the user can sanity-check what they're about to replace before
/// confirming. Non-TTY callers never reach this — they take the
/// rotation-aware error path and have to opt in via `--force`.
fn confirm_relink(path: &Path, existing: &CloudConfig) -> Result<bool> {
    if !io::stdin().is_terminal() {
        return Ok(false);
    }
    print!(
        "{} {}. Replace with the key you just supplied? [y/N] ",
        path.display(),
        describe_existing_link(existing),
    );
    io::stdout().flush().ok();
    let mut answer = String::new();
    if io::stdin().read_line(&mut answer).is_err() {
        return Ok(false);
    }
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// #559: rotation-aware replacement for the bare "file exists" error.
/// Names the existing org and points at `--force` as the right escape
/// hatch for the org-switch / key-rotation case. Used when the user
/// hits the non-interactive path (no `--api-key`, or stdin not a TTY).
fn rotation_aware_already_exists_error(path: &Path, existing: &CloudConfig) -> anyhow::Error {
    anyhow!(
        "{} {}. If you're switching orgs or rotating your API key, re-run with --force to replace it with the key you just supplied (existing settings will be overwritten).",
        path.display(),
        describe_existing_link(existing),
    )
}

/// One-liner describing what `cloud.toml` is currently linked to,
/// suitable for embedding in a prompt or error message. Falls back to
/// a generic phrase when the file is malformed or `org_id` is absent
/// (e.g. a partially edited template) so we never print a quoted empty
/// string.
fn describe_existing_link(existing: &CloudConfig) -> String {
    match existing.org_id.as_deref() {
        Some(id) if !id.is_empty() => format!("already points to org \"{id}\""),
        _ => "already exists".to_string(),
    }
}

fn render_init_text(path: &std::path::Path, existed: bool, enabled: bool, seed: &SeedOutcome) {
    let green = ansi("\x1b[32m");
    let yellow = ansi("\x1b[33m");
    let red = ansi("\x1b[31m");
    let dim = ansi("\x1b[90m");
    let bold = ansi("\x1b[1m");
    let reset = ansi("\x1b[0m");

    let verb = if existed { "Overwrote" } else { "Created" };
    println!();
    println!(
        "  {green}✓{reset} {bold}{verb} cloud config template:{reset} {}",
        path.display()
    );

    // #541: surface what the seeding flow did. Each SeedOutcome variant
    // maps to a single line explaining "what's in the file and why".
    // `Skipped` stays quiet (matches pre-#541 render shape for the
    // bare-run path) — the trailing "Next steps" block already covers it.
    match seed {
        SeedOutcome::Skipped => {}
        SeedOutcome::Seeded(ids) => {
            println!();
            println!("  {green}✓{reset} {bold}Seeded cloud identity:{reset}",);
            println!(
                "    {dim}device_id:{reset} {}  {dim}({}){reset}",
                ids.device_id,
                describe_source(ids.device_id_source),
            );
            if let (Some(org), Some(src)) = (ids.org_id.as_deref(), ids.org_id_source) {
                println!(
                    "    {dim}org_id:{reset}    {org}  {dim}({}){reset}",
                    describe_source(src),
                );
            }
        }
        SeedOutcome::KeyRejected => {
            println!();
            println!(
                "  {red}✗{reset} {bold}API key rejected by cloud.{reset} Template was written with {bold}enabled = false{reset} and the placeholder key; fix the key and re-run `budi cloud init --api-key KEY --force`."
            );
        }
        SeedOutcome::EndpointAbsent {
            status, device_id, ..
        } => {
            println!();
            println!(
                "  {yellow}!{reset} {bold}Cloud endpoint returned {status}{reset} for `/v1/whoami` — auto-seeding `org_id` wasn't available. device_id was still generated ({}). Set `org_id` manually in {} or re-run with `--org-id <ID>`.",
                device_id,
                path.display(),
            );
        }
        SeedOutcome::TransientError {
            detail, device_id, ..
        } => {
            println!();
            println!(
                "  {yellow}!{reset} {bold}Couldn't reach cloud to auto-seed org_id:{reset} {detail}. device_id was still generated ({}). Set `org_id` manually in {} or re-run with `--org-id <ID>`.",
                device_id,
                path.display(),
            );
        }
    }

    println!();
    if enabled {
        println!("  {bold}Next steps{reset}");
        println!("    1. {dim}Restart the daemon:{reset} budi init");
        println!(
            "    2. {dim}Confirm:{reset}            budi cloud status   (expect 'state: ready')"
        );
        println!("    3. {dim}Push now:{reset}           budi cloud sync");
    } else {
        println!("  {bold}Next steps{reset}");
        println!("    1. {dim}Sign up:{reset}  open https://app.getbudi.dev");
        println!(
            "    2. {dim}Paste key:{reset} replace `PASTE_YOUR_KEY_HERE` in {}",
            path.display()
        );
        println!("    3. {dim}Enable:{reset}   set `enabled = true` in the same file");
        println!("    4. {dim}Restart:{reset}  budi init");
        println!("    5. {dim}Confirm:{reset}  budi cloud status");
        println!("    6. {dim}Push now:{reset} budi cloud sync");
    }
    println!();
}

fn describe_source(src: IdentitySource) -> &'static str {
    match src {
        IdentitySource::Generated => "generated",
        IdentitySource::Whoami => "from /v1/whoami",
        IdentitySource::Flag => "from flag",
    }
}

/// `budi cloud sync` — flush the pending cloud queue now.
pub fn cmd_cloud_sync(format: StatsFormat) -> Result<()> {
    let client = DaemonClient::connect()?;
    let body = client.cloud_sync()?;

    if matches!(format, StatsFormat::Json) {
        super::print_json(&body)?;
        // Exit non-zero on non-ok result so scripts can branch on status.
        if body.get("ok").and_then(Value::as_bool) != Some(true) {
            std::process::exit(2);
        }
        return Ok(());
    }

    render_sync_text(&body);
    if body.get("ok").and_then(Value::as_bool) != Some(true) {
        std::process::exit(2);
    }
    Ok(())
}

/// `budi cloud status` — report cloud sync readiness and last-synced-at.
pub fn cmd_cloud_status(format: StatsFormat) -> Result<()> {
    let client = DaemonClient::connect()?;
    let body = client.cloud_status()?;

    if matches!(format, StatsFormat::Json) {
        super::print_json(&body)?;
        return Ok(());
    }

    render_status_text(&body);
    Ok(())
}

/// #564: `budi cloud reset` — drop the local cloud-sync watermarks so the
/// next sync re-uploads every rollup + session summary in
/// `message_rollups_daily` / `sessions`. The user-visible escape hatch
/// after the cloud loses historical rows (org switch, device_id rotation,
/// cloud-side wipe). Cloud-side dedup (ADR-0083 §6) keeps the re-upload
/// safe even when records overlap with what the cloud already has.
///
/// Routes through the daemon's `POST /cloud/reset` so SQLite stays
/// daemon-owned and the reset takes the same `cloud_syncing` busy flag a
/// background tick would — that way a manual reset can never race a
/// concurrent envelope build that already read the about-to-be-deleted
/// watermark.
pub fn cmd_cloud_reset(yes: bool) -> Result<()> {
    let cfg = load_cloud_config();
    if !confirm_reset(&cfg, yes)? {
        println!("Aborted. Watermarks left unchanged.");
        return Ok(());
    }

    let client = DaemonClient::connect()?;
    let body = client
        .cloud_reset()
        .context("failed to reset cloud sync watermarks via the daemon")?;

    let removed = body.get("removed").and_then(Value::as_u64).unwrap_or(0) as usize;
    render_reset_text(&cfg, removed);
    Ok(())
}

fn confirm_reset(cfg: &CloudConfig, yes: bool) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if !io::stdin().is_terminal() {
        return Err(anyhow!(
            "`budi cloud reset` requires confirmation. Re-run with --yes to skip the prompt (required for non-interactive shells)."
        ));
    }
    print!(
        "This will reset the cloud sync watermarks for {}.\nThe next `budi cloud sync` will re-upload all local rollups and session summaries to the cloud.\nContinue? [y/N] ",
        describe_reset_target(cfg),
    );
    io::stdout().flush().ok();
    let mut answer = String::new();
    if io::stdin().read_line(&mut answer).is_err() {
        return Ok(false);
    }
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Phrase the prompt + post-reset line so the user sees which org they
/// are about to re-upload to. Falls back to a generic phrase when
/// `cloud.toml` is missing or partial — `budi cloud reset` still works
/// in those cases (the watermarks live in SQLite, independent of the
/// TOML), but we don't want to print `org ""`.
fn describe_reset_target(cfg: &CloudConfig) -> String {
    match cfg.org_id.as_deref() {
        Some(id) if !id.is_empty() => format!("org \"{id}\""),
        _ => "the configured cloud endpoint".to_string(),
    }
}

fn render_reset_text(cfg: &CloudConfig, removed: usize) {
    let green = ansi("\x1b[32m");
    let yellow = ansi("\x1b[33m");
    let dim = ansi("\x1b[90m");
    let bold = ansi("\x1b[1m");
    let reset = ansi("\x1b[0m");

    println!();
    if removed == 0 {
        println!(
            "  {yellow}!{reset} {bold}No cloud sync watermarks were set.{reset} The next `budi cloud sync` already starts from scratch."
        );
    } else {
        println!(
            "  {green}✓{reset} {bold}Cloud sync watermarks reset for {}.{reset}",
            describe_reset_target(cfg),
        );
        println!(
            "    {dim}removed{reset}    {removed} sentinel row{}",
            if removed == 1 { "" } else { "s" },
        );
    }
    println!(
        "    {dim}next step{reset}  run {bold}budi cloud sync{reset} to push everything now, or wait for the next interval tick"
    );
    println!();
}

fn render_sync_text(body: &Value) {
    let green = ansi("\x1b[32m");
    let red = ansi("\x1b[31m");
    let yellow = ansi("\x1b[33m");
    let dim = ansi("\x1b[90m");
    let bold = ansi("\x1b[1m");
    let reset = ansi("\x1b[0m");

    let ok = body.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let result = body
        .get("result")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let message = body.get("message").and_then(Value::as_str).unwrap_or("");
    let endpoint = body.get("endpoint").and_then(Value::as_str).unwrap_or("");
    let records = body
        .get("records_upserted")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let rollups = body
        .get("rollups_attempted")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let sessions = body
        .get("sessions_attempted")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let watermark = body.get("watermark").and_then(Value::as_str);

    println!();
    let (icon, color, headline) = match (ok, result) {
        (true, "success") => (
            "✓",
            green,
            format!("Cloud sync complete ({records} records pushed)"),
        ),
        (true, "empty_payload") => ("✓", green, "Nothing new to sync".to_string()),
        (_, "disabled") => ("-", dim, "Cloud sync is disabled".to_string()),
        (_, "not_configured") => ("!", yellow, "Cloud sync is not configured".to_string()),
        (_, "auth_failure") => ("✗", red, "Cloud sync failed: authentication".to_string()),
        (_, "schema_mismatch") => ("✗", red, "Cloud sync failed: schema mismatch".to_string()),
        (_, "transient_error") => ("✗", red, "Cloud sync failed: transient error".to_string()),
        _ => ("✗", red, format!("Cloud sync result: {result}")),
    };

    println!("  {color}{icon}{reset} {bold}{headline}{reset}");
    if !message.is_empty() {
        println!("    {dim}{message}{reset}");
    }
    if !endpoint.is_empty() {
        println!("    {dim}endpoint{reset}   {endpoint}");
    }
    if rollups > 0 || sessions > 0 {
        println!("    {dim}attempted{reset}  {rollups} rollups, {sessions} sessions");
    }
    if let Some(wm) = watermark {
        println!("    {dim}watermark{reset}  {wm}");
    }
    println!();
}

fn render_status_text(body: &Value) {
    let green = ansi("\x1b[32m");
    let red = ansi("\x1b[31m");
    let yellow = ansi("\x1b[33m");
    let dim = ansi("\x1b[90m");
    let bold_cyan = ansi("\x1b[1;36m");
    let bold = ansi("\x1b[1m");
    let reset = ansi("\x1b[0m");

    let enabled = body
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let configured = body
        .get("configured")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let ready = body.get("ready").and_then(Value::as_bool).unwrap_or(false);
    // Older daemons don't emit these fields; treat them as false so the
    // pre-#446 "disabled" branch still fires instead of the new "no config"
    // branch when a new CLI talks to an old daemon.
    let config_exists = body
        .get("config_exists")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let api_key_stub = body
        .get("api_key_stub")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let endpoint = body.get("endpoint").and_then(Value::as_str).unwrap_or("");
    let last_synced_at = body.get("last_synced_at").and_then(Value::as_str);
    let watermark = body.get("rollup_watermark").and_then(Value::as_str);
    let pending_rollups = body
        .get("pending_rollups")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let pending_sessions = body
        .get("pending_sessions")
        .and_then(Value::as_i64)
        .unwrap_or(0);

    println!();
    println!("  {bold_cyan} budi cloud status{reset}");
    println!("  {dim}{}{reset}", "─".repeat(40));

    // Order matters: "ready" dominates; otherwise we pick the most specific
    // not-ready label so the next-step hint under the panel can point at the
    // exact thing that is missing (#446).
    let (state_icon, state_color, state_label) = if ready {
        ("✓", green, "ready")
    } else if !config_exists {
        ("-", dim, "disabled (no config)")
    } else if api_key_stub {
        ("!", yellow, "disabled (stub key)")
    } else if enabled && !configured {
        ("!", yellow, "enabled but missing api_key")
    } else if enabled {
        ("!", yellow, "enabled but not fully configured")
    } else {
        ("-", dim, "disabled")
    };
    println!(
        "  {state_color}{state_icon}{reset} {bold}State{reset}      {state_color}{state_label}{reset}"
    );

    if !endpoint.is_empty() {
        println!("    {dim}endpoint{reset}   {endpoint}");
    }

    match last_synced_at {
        Some(ts) => println!("    {dim}last sync{reset}  {ts}"),
        None => println!("    {dim}last sync{reset}  {red}never{reset}"),
    }
    if let Some(wm) = watermark {
        println!("    {dim}watermark{reset}  {wm}");
    }
    if pending_rollups > 0 || pending_sessions > 0 {
        println!(
            "    {dim}pending{reset}    {yellow}{pending_rollups} rollups, {pending_sessions} sessions{reset}  (run `budi cloud sync`)"
        );
    }

    if !ready {
        println!();
        if !config_exists {
            println!(
                "  {dim}No ~/.config/budi/cloud.toml yet. Run{reset} {bold}budi cloud init{reset} {dim}to generate a template.{reset}"
            );
        } else if api_key_stub {
            println!(
                "  {yellow}API key not set —{reset} paste your key from https://app.getbudi.dev into ~/.config/budi/cloud.toml, then set `enabled = true`."
            );
        } else if !enabled {
            println!(
                "  {dim}Cloud sync is off. Enable it by setting `enabled = true` in{reset} ~/.config/budi/cloud.toml"
            );
        } else if !configured {
            println!(
                "  {yellow}Cloud sync is enabled but missing credentials.{reset} See ~/.config/budi/cloud.toml"
            );
        }
    }
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use budi_core::config::CloudConfig;

    #[test]
    fn template_stub_variant_is_disabled_and_carries_placeholder() {
        let out = render_cloud_toml_template(CLOUD_API_KEY_STUB, false, None);
        assert!(out.contains("enabled = false"));
        assert!(out.contains("PASTE_YOUR_KEY_HERE"));
        assert!(
            out.contains("[cloud]"),
            "top-level section must match ADR-0083 §9 wire shape"
        );
        assert!(
            out.contains("[cloud.sync]"),
            "sync tuning block must be present so interval/retry are discoverable"
        );
    }

    #[test]
    fn template_with_real_key_is_enabled() {
        let out = render_cloud_toml_template("fake-test-key", true, None);
        assert!(out.contains("enabled = true"));
        assert!(out.contains("api_key = \"fake-test-key\""));
        // The comment above `api_key = ...` legitimately references the
        // placeholder string so users recognize the "stub key" status line —
        // assert on the actual assignment instead of a global search so we
        // pin the behaviour that matters (no live key written with the stub
        // value) without being precious about the inline docstring.
        assert!(
            !out.contains("api_key = \"PASTE_YOUR_KEY_HERE\""),
            "real-key path must not emit an api_key assignment of the stub value"
        );
    }

    #[test]
    fn template_is_valid_toml_and_round_trips_through_cloud_config() {
        let out = render_cloud_toml_template("fake-test-key", true, None);
        // The generated file uses a top-level [cloud] section, so parse it
        // via a small wrapper that mirrors `load_cloud_config`'s contract.
        #[derive(serde::Deserialize)]
        struct Wrapper {
            cloud: CloudConfig,
        }
        let w: Wrapper = toml::from_str(&out).expect("generated template must be valid TOML");
        assert!(w.cloud.enabled);
        assert_eq!(w.cloud.api_key.as_deref(), Some("fake-test-key"));
        assert_eq!(w.cloud.endpoint, "https://app.getbudi.dev");
        assert_eq!(w.cloud.sync.interval_seconds, 300);
    }

    #[test]
    fn template_stub_round_trips_as_stub() {
        let out = render_cloud_toml_template(CLOUD_API_KEY_STUB, false, None);
        #[derive(serde::Deserialize)]
        struct Wrapper {
            cloud: CloudConfig,
        }
        let w: Wrapper = toml::from_str(&out).unwrap();
        assert!(!w.cloud.enabled);
        assert!(w.cloud.is_api_key_stub());
    }

    #[test]
    fn template_with_full_seed_writes_uncommented_identity_lines() {
        // #541 happy path: device_id + org_id both land as real TOML
        // assignments, not commented placeholders.
        let seed = SeededIdentity {
            device_id: "7b322df1-3bcd-4e72-9e2a-0b2f3c4d5e6f".to_string(),
            device_id_source: IdentitySource::Generated,
            org_id: Some("org_xEvtA".to_string()),
            org_id_source: Some(IdentitySource::Whoami),
        };
        let out = render_cloud_toml_template("budi_realkey", true, Some(&seed));
        assert!(out.contains("device_id = \"7b322df1-3bcd-4e72-9e2a-0b2f3c4d5e6f\""));
        assert!(out.contains("org_id = \"org_xEvtA\""));
        // No commented placeholder lines should leak through.
        assert!(!out.contains("# device_id = \"your-device-id\""));
        assert!(!out.contains("# org_id = \"your-org-id\""));

        // Round-trip through CloudConfig to make sure the uncommented
        // lines parse and are picked up by the loader.
        #[derive(serde::Deserialize)]
        struct Wrapper {
            cloud: CloudConfig,
        }
        let w: Wrapper = toml::from_str(&out).expect("seeded template parses");
        assert_eq!(
            w.cloud.device_id.as_deref(),
            Some("7b322df1-3bcd-4e72-9e2a-0b2f3c4d5e6f"),
        );
        assert_eq!(w.cloud.org_id.as_deref(), Some("org_xEvtA"));
        assert!(
            w.cloud.is_ready(),
            "full seed should produce a ready config"
        );
    }

    #[test]
    fn template_with_device_id_only_leaves_org_id_commented() {
        // #541 fallback: whoami failed (EndpointAbsent / TransientError)
        // → device_id still seeded, org_id stays commented so the user
        // has to set it manually.
        let seed = SeededIdentity {
            device_id: "7b322df1-3bcd-4e72-9e2a-0b2f3c4d5e6f".to_string(),
            device_id_source: IdentitySource::Generated,
            org_id: None,
            org_id_source: None,
        };
        let out = render_cloud_toml_template("budi_realkey", true, Some(&seed));
        assert!(out.contains("device_id = \"7b322df1-3bcd-4e72-9e2a-0b2f3c4d5e6f\""));
        assert!(out.contains("# org_id = \"your-org-id\""));
        // Round-trip: not is_ready because org_id is None.
        #[derive(serde::Deserialize)]
        struct Wrapper {
            cloud: CloudConfig,
        }
        let w: Wrapper = toml::from_str(&out).unwrap();
        assert!(!w.cloud.is_ready());
        assert_eq!(
            w.cloud.disabled_reason(),
            Some("missing org_id"),
            "fallback config must surface the precise missing field",
        );
    }

    #[test]
    fn seed_outcome_seeded_ids_mirrors_the_variant() {
        // #541: `SeedOutcome::seeded_ids` is the helper that feeds the
        // template renderer. KeyRejected / Skipped must yield None; the
        // remaining variants must surface device_id so the template at
        // least records what was generated even when cloud is down.
        assert!(SeedOutcome::Skipped.seeded_ids().is_none());
        assert!(SeedOutcome::KeyRejected.seeded_ids().is_none());

        let absent = SeedOutcome::EndpointAbsent {
            status: 404,
            device_id: "dev-1".to_string(),
            device_id_source: IdentitySource::Generated,
        };
        let got = absent.seeded_ids().unwrap();
        assert_eq!(got.device_id, "dev-1");
        assert!(got.org_id.is_none());

        let transient = SeedOutcome::TransientError {
            detail: "boom".to_string(),
            device_id: "dev-2".to_string(),
            device_id_source: IdentitySource::Flag,
        };
        let got = transient.seeded_ids().unwrap();
        assert_eq!(got.device_id, "dev-2");
        assert_eq!(got.device_id_source, IdentitySource::Flag);
        assert!(got.org_id.is_none());

        let seeded = SeedOutcome::Seeded(SeededIdentity {
            device_id: "dev-3".to_string(),
            device_id_source: IdentitySource::Generated,
            org_id: Some("org_x".to_string()),
            org_id_source: Some(IdentitySource::Whoami),
        });
        let got = seeded.seeded_ids().unwrap();
        assert_eq!(got.org_id.as_deref(), Some("org_x"));
        assert_eq!(got.org_id_source, Some(IdentitySource::Whoami));
    }

    fn config_with_org(org_id: Option<&str>) -> CloudConfig {
        CloudConfig {
            org_id: org_id.map(|s| s.to_string()),
            ..CloudConfig::default()
        }
    }

    #[test]
    fn describe_existing_link_names_org_when_present() {
        // #559: prompts and error messages should name the linked org so
        // the user can sanity-check what they're about to overwrite.
        let cfg = config_with_org(Some("org_xEvtA"));
        assert_eq!(
            describe_existing_link(&cfg),
            "already points to org \"org_xEvtA\"",
        );
    }

    #[test]
    fn describe_existing_link_falls_back_when_org_id_missing() {
        // #559: a partially edited template (no org_id) shouldn't print
        // a quoted empty string. Fall back to the generic phrase.
        let cfg = config_with_org(None);
        assert_eq!(describe_existing_link(&cfg), "already exists");

        let empty = config_with_org(Some(""));
        assert_eq!(
            describe_existing_link(&empty),
            "already exists",
            "empty org_id should not produce \"\" in the message",
        );
    }

    #[test]
    fn rotation_aware_error_names_org_and_points_at_force() {
        // #559: bare "already exists" was confusing in the rotation
        // path. The new copy should name the org and explain when
        // --force is the right escape hatch.
        let path = std::path::PathBuf::from("/tmp/cloud.toml");
        let cfg = config_with_org(Some("org_old"));
        let err = rotation_aware_already_exists_error(&path, &cfg).to_string();
        assert!(
            err.contains("org \"org_old\""),
            "error must name the existing org: {err}",
        );
        assert!(
            err.contains("--force"),
            "error must point at --force as the escape hatch: {err}",
        );
        assert!(
            err.contains("switching orgs") || err.contains("rotating"),
            "error must call out the rotation/switch case: {err}",
        );
    }

    #[test]
    fn rotation_aware_error_falls_back_when_org_id_missing() {
        // Partial config (no org_id) still gets a useful error — just
        // without the org name.
        let path = std::path::PathBuf::from("/tmp/cloud.toml");
        let cfg = config_with_org(None);
        let err = rotation_aware_already_exists_error(&path, &cfg).to_string();
        assert!(err.contains("--force"));
        assert!(!err.contains("org \"\""));
    }

    #[test]
    fn describe_reset_target_names_org_when_present() {
        // #564: the reset prompt and post-reset line both name the
        // currently-linked org so the user can sanity-check what
        // they are about to re-upload to.
        let cfg = config_with_org(Some("org_xEvtA"));
        assert_eq!(describe_reset_target(&cfg), "org \"org_xEvtA\"");
    }

    #[test]
    fn describe_reset_target_falls_back_when_org_id_missing() {
        // #564: a partial / malformed `cloud.toml` shouldn't print a
        // quoted empty string. The reset path still works (watermarks
        // live in SQLite, independent of the TOML), so we just print a
        // generic phrase.
        let cfg = config_with_org(None);
        assert_eq!(describe_reset_target(&cfg), "the configured cloud endpoint",);

        let empty = config_with_org(Some(""));
        assert_eq!(
            describe_reset_target(&empty),
            "the configured cloud endpoint",
            "empty org_id should not produce \"\" in the message",
        );
    }
}
