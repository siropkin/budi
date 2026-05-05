use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const BUDI_HOME_ENV: &str = "BUDI_HOME";
pub const BUDI_HOME_DEFAULT_REL: &str = ".local/share/budi";
pub(crate) const BUDI_REPOS_DIR: &str = "repos";
pub(crate) const BUDI_CONFIG_FILE_NAME: &str = "config.toml";
pub(crate) const BUDI_REPO_ROOT_MARKER_FILE_NAME: &str = "repo-root.txt";
pub(crate) const BUDI_LOG_DIR_NAME: &str = "logs";

fn parse_env_path(raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed))
}

/// Cross-platform home directory detection.
/// Uses HOME on Unix, USERPROFILE (then HOMEPATH) on Windows.
pub fn home_dir() -> Result<PathBuf> {
    if let Ok(home) = env::var("HOME")
        && let Some(path) = parse_env_path(&home)
    {
        return Ok(path);
    }
    #[cfg(windows)]
    {
        if let Ok(profile) = env::var("USERPROFILE") {
            if let Some(path) = parse_env_path(&profile) {
                return Ok(path);
            }
        }
        if let (Ok(drive), Ok(path)) = (env::var("HOMEDRIVE"), env::var("HOMEPATH")) {
            if let Some(path) = parse_env_path(&format!("{drive}{path}")) {
                return Ok(path);
            }
        }
    }
    anyhow::bail!("Could not determine home directory (HOME not set)")
}

pub const DEFAULT_DAEMON_HOST: &str = "127.0.0.1";
pub const DEFAULT_DAEMON_PORT: u16 = 7878;

/// Known agent identifiers used in `agents.toml`.
pub const KNOWN_AGENTS: &[&str] = &["claude-code", "codex-cli", "cursor", "copilot-cli"];

/// Per-agent enablement entry.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentEntry {
    pub enabled: bool,
}

/// Per-agent enablement config loaded from `~/.config/budi/agents.toml`.
///
/// When the file is absent (legacy install), callers should treat all
/// available agents as enabled for backward compatibility.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AgentsConfig {
    #[serde(rename = "claude-code")]
    pub claude_code: AgentEntry,
    #[serde(rename = "codex-cli")]
    pub codex_cli: AgentEntry,
    pub cursor: AgentEntry,
    #[serde(rename = "copilot-cli")]
    pub copilot_cli: AgentEntry,
}

impl AgentsConfig {
    pub fn is_agent_enabled(&self, provider_name: &str) -> bool {
        match provider_name {
            "claude_code" => self.claude_code.enabled,
            "codex" | "codex_cli" => self.codex_cli.enabled,
            "cursor" => self.cursor.enabled,
            "copilot_cli" => self.copilot_cli.enabled,
            _ => false,
        }
    }

    /// Returns a config with all known agents enabled.
    pub fn all_enabled() -> Self {
        Self {
            claude_code: AgentEntry { enabled: true },
            codex_cli: AgentEntry { enabled: true },
            cursor: AgentEntry { enabled: true },
            copilot_cli: AgentEntry { enabled: true },
        }
    }

    /// Human-readable display name for an agent identifier.
    pub fn display_name(agent: &str) -> &'static str {
        match agent {
            "claude-code" => "Claude Code",
            "codex-cli" => "Codex CLI",
            "cursor" => "Cursor",
            "copilot-cli" => "Copilot CLI",
            _ => "Unknown",
        }
    }
}

/// Path to the global agents config file.
pub fn agents_config_path() -> Result<PathBuf> {
    Ok(budi_config_dir()?.join("agents.toml"))
}

/// Load agents config. Returns `None` if the file does not exist (legacy install)
/// or if the file is effectively empty (no explicit agent sections).
/// Callers should treat `None` as "all available agents enabled" for backward compatibility.
pub fn load_agents_config() -> Option<AgentsConfig> {
    let path = agents_config_path().ok()?;
    if !path.exists() {
        return None;
    }
    let raw = fs::read_to_string(&path).ok()?;

    // An empty or whitespace-only file should be treated the same as a missing
    // file (all-enabled fallback) rather than silently disabling every agent.
    let has_explicit_sections = KNOWN_AGENTS
        .iter()
        .any(|agent| raw.contains(&format!("[{agent}]")));
    if !has_explicit_sections {
        if !raw.trim().is_empty() {
            tracing::warn!(
                "{}: no recognized agent sections found; treating as absent",
                path.display()
            );
        }
        return None;
    }

    let parsed: toml::Value = match toml::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("Failed to parse {}: {e}", path.display());
            return None;
        }
    };

    // Warn about unknown top-level keys so typos don't silently disable an agent.
    if let Some(table) = parsed.as_table() {
        for key in table.keys() {
            if !KNOWN_AGENTS.contains(&key.as_str()) {
                tracing::warn!(
                    "{}: unknown agent key '{}'; known agents: {}",
                    path.display(),
                    key,
                    KNOWN_AGENTS.join(", ")
                );
            }
        }
    }

    match toml::from_str(&raw) {
        Ok(config) => Some(config),
        Err(e) => {
            tracing::warn!("Failed to parse {}: {e}", path.display());
            None
        }
    }
}

/// Known statusline slot names.
///
/// `1d` / `7d` / `30d` are the canonical window slot names for the default
/// quiet statusline (ADR-0088 §4, #224). `today` / `week` / `month` are kept
/// as backward-compatible aliases — they render the same rolling-window
/// values so existing `~/.config/budi/statusline.toml` files keep working.
pub const STATUSLINE_SLOTS: &[&str] = &[
    "1d", "7d", "30d", "today", "week", "month", "session", "message", "branch", "project",
    "provider", "health",
];

/// Named presets for common statusline layouts.
///
/// Default is `cost` (rolling `1d` / `7d` / `30d`). `coach` and `full` are
/// advanced variants documented in the README; they are not in the default
/// install path per ADR-0088 §4.
pub const STATUSLINE_PRESETS: &[(&str, &[&str])] = &[
    ("cost", &["1d", "7d", "30d"]),
    ("coach", &["session", "health"]),
    ("full", &["session", "health", "1d"]),
];

/// Normalize a legacy slot name to its canonical form.
/// Maps calendar-window aliases (`today` / `week` / `month`) to their rolling
/// equivalents (`1d` / `7d` / `30d`). Returns the input unchanged for all
/// other slot names.
pub fn normalize_statusline_slot(slot: &str) -> &str {
    match slot {
        "today" => "1d",
        "week" => "7d",
        "month" => "30d",
        other => other,
    }
}

/// User-configurable statusline layout.
///
/// Loaded from `~/.config/budi/statusline.toml`.
/// Example:
/// ```toml
/// preset = "coach"
/// # Or customize directly:
/// # slots = ["today", "week", "month", "branch"]
/// # format = "{today} | {week} | {month}"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StatuslineConfig {
    /// Named preset: "cost" (today/week/month), "coach" (session+health), "full" (session+health+today).
    /// When set, overrides `slots`. Ignored if `format` is set.
    pub preset: Option<String>,
    /// Ordered list of data slots to display. Default: ["today", "week", "month"].
    pub slots: Vec<String>,
    /// Optional custom format template. Overrides `slots` and `preset` when set.
    /// Placeholders: {today}, {week}, {month}, {session}, {message}, {branch}, {project}, {provider}, {health}
    pub format: Option<String>,
}

impl Default for StatuslineConfig {
    fn default() -> Self {
        Self {
            preset: None,
            slots: vec!["1d".to_string(), "7d".to_string(), "30d".to_string()],
            format: None,
        }
    }
}

impl StatuslineConfig {
    /// Resolve the effective slots list, considering preset → slots → format priority.
    /// Legacy slot aliases (`today` / `week` / `month`) are normalized to
    /// their canonical rolling-window names (`1d` / `7d` / `30d`).
    pub fn effective_slots(&self) -> Vec<String> {
        let raw = if let Some(ref preset_name) = self.preset
            && let Some((_, preset_slots)) = STATUSLINE_PRESETS
                .iter()
                .find(|(name, _)| *name == preset_name.as_str())
        {
            preset_slots.iter().map(|s| s.to_string()).collect()
        } else {
            self.slots.clone()
        };
        raw.into_iter()
            .map(|s| normalize_statusline_slot(&s).to_string())
            .collect()
    }

    /// Resolve which slots are needed (from format template, preset, or explicit slots list).
    /// Legacy slot aliases (`today` / `week` / `month`) are normalized to
    /// their canonical rolling-window names (`1d` / `7d` / `30d`).
    pub fn required_slots(&self) -> Vec<String> {
        if let Some(ref fmt) = self.format {
            let mut slots = Vec::new();
            let mut rest = fmt.as_str();
            while let Some(start) = rest.find('{') {
                if let Some(end) = rest[start..].find('}') {
                    let name = &rest[start + 1..start + end];
                    if STATUSLINE_SLOTS.contains(&name) {
                        let canonical = normalize_statusline_slot(name).to_string();
                        if !slots.iter().any(|s: &String| s == &canonical) {
                            slots.push(canonical);
                        }
                    }
                    rest = &rest[start + end + 1..];
                } else {
                    break;
                }
            }
            slots
        } else {
            self.effective_slots()
        }
    }
}

/// Path to the global statusline config file.
pub fn statusline_config_path() -> Result<PathBuf> {
    let home = home_dir()?;
    Ok(home.join(".config/budi/statusline.toml"))
}

/// Load statusline config, falling back to defaults if the file doesn't exist.
pub fn load_statusline_config() -> StatuslineConfig {
    let path = match statusline_config_path() {
        Ok(p) => p,
        Err(_) => return StatuslineConfig::default(),
    };
    if !path.exists() {
        return StatuslineConfig::default();
    }
    let raw = match fs::read_to_string(&path) {
        Ok(r) => r,
        Err(_) => return StatuslineConfig::default(),
    };
    toml::from_str(&raw).unwrap_or_default()
}

/// Outcome of a call to [`seed_statusline_config_if_needed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeedStatuslineOutcome {
    /// Wrote a fresh `statusline.toml` with the quiet `cost` preset and
    /// commented examples for `coach` / `full` / custom formats.
    Generated,
    /// File already exists — no-op so user edits aren't clobbered.
    AlreadySet,
}

/// Default seeded contents for `~/.config/budi/statusline.toml`.
///
/// Mirrors the default `StatuslineConfig` (`slots = ["1d", "7d", "30d"]`)
/// so a `cat ~/.config/budi/statusline.toml` after `budi init` shows
/// users *exactly* what's running, plus the discoverability comments
/// for the `coach` / `full` presets and the custom-format escape hatch.
pub const STATUSLINE_TOML_TEMPLATE: &str = "\
# budi statusline configuration.
# Active layout: rolling 1d / 7d / 30d cost (the quiet default).
slots = [\"1d\", \"7d\", \"30d\"]

# Try a different preset:
# preset = \"coach\"  # session cost + health vitals + tip
# preset = \"full\"   # session + health + 1d
#
# Or build a custom format:
# format = \"{health} {project} | {session} | {1d} 1d | {7d} 7d\"
#
# Available slots: 1d, 7d, 30d, session, message, branch, project, provider, health
# Docs: https://github.com/siropkin/budi#status-line
";

/// Idempotently seed `~/.config/budi/statusline.toml` with the default
/// `cost` preset and commented examples for the other presets.
///
/// `budi init` calls this after installing the Claude Code statusline so
/// users have a real file to edit (#600). Without it, the README told
/// users to customize via `~/.config/budi/statusline.toml` but the file
/// only existed once they passed `--statusline-preset`, leaving fresh
/// installs with nothing to discover.
///
/// Idempotent: returns `AlreadySet` on every call after the first
/// without touching the file (preserves user edits). Caller is
/// responsible for the `~/.claude` / `--no-integrations` install gates;
/// we only own the create-if-missing rule for the file itself.
pub fn seed_statusline_config_if_needed() -> Result<SeedStatuslineOutcome> {
    let path = statusline_config_path()?;
    if path.exists() {
        return Ok(SeedStatuslineOutcome::AlreadySet);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    fs::write(&path, STATUSLINE_TOML_TEMPLATE)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(SeedStatuslineOutcome::Generated)
}

/// A single tag rule from `~/.config/budi/tags.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagRule {
    pub key: String,
    pub value: String,
    pub match_repo: Option<String>,
}

/// Tags configuration loaded from `~/.config/budi/tags.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TagsConfig {
    #[serde(default)]
    pub rules: Vec<TagRule>,
}

/// Path to the global tags config file.
pub fn tags_config_path() -> Result<PathBuf> {
    let home = home_dir()?;
    Ok(home.join(".config/budi/tags.toml"))
}

/// Load tags config, returning None if the file doesn't exist.
/// Logs a warning if the file exists but cannot be read or parsed.
pub fn load_tags_config() -> Option<TagsConfig> {
    let path = tags_config_path().ok()?;
    if !path.exists() {
        return None;
    }
    let raw = match fs::read_to_string(&path) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("Failed to read {}: {e}", path.display());
            return None;
        }
    };
    match toml::from_str(&raw) {
        Ok(config) => Some(config),
        Err(e) => {
            tracing::warn!("Failed to parse {}: {e}", path.display());
            None
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BudiConfig {
    /// Host the daemon listens on. Default: "127.0.0.1".
    pub daemon_host: String,
    /// Port the daemon listens on. Default: 7878.
    pub daemon_port: u16,
}

impl Default for BudiConfig {
    fn default() -> Self {
        Self {
            daemon_host: DEFAULT_DAEMON_HOST.to_string(),
            daemon_port: DEFAULT_DAEMON_PORT,
        }
    }
}

impl BudiConfig {
    pub fn daemon_base_url(&self) -> String {
        format!("http://{}:{}", self.daemon_host, self.daemon_port)
    }
}

#[derive(Debug, Clone)]
pub struct RepoPaths {
    pub data_dir: PathBuf,
    pub config_file: PathBuf,
    pub log_dir: PathBuf,
}

pub fn find_repo_root(start: &Path) -> Result<PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        if current.join(".git").exists() {
            return Ok(current);
        }
        if !current.pop() {
            anyhow::bail!(
                "Not a git repository (or any parent up to /): {}\n\
                 Run `git init` first, or use --repo-root to specify the repo path.",
                start.display()
            );
        }
    }
}

/// For git worktrees, resolve to the main repo root for shared storage.
pub fn resolve_storage_root(repo_root: &Path) -> PathBuf {
    let git_path = repo_root.join(".git");
    if git_path.is_file()
        && let Some(main_root) = resolve_worktree_main_root(&git_path)
    {
        return main_root;
    }
    repo_root.to_path_buf()
}

fn resolve_worktree_main_root(git_file: &Path) -> Option<PathBuf> {
    let content = std::fs::read_to_string(git_file).ok()?;
    let gitdir = content.strip_prefix("gitdir: ")?.trim();
    let gitdir_path = if Path::new(gitdir).is_absolute() {
        PathBuf::from(gitdir)
    } else {
        git_file.parent()?.join(gitdir)
    };
    let mut candidate = gitdir_path;
    loop {
        if candidate.file_name().map(|n| n == ".git").unwrap_or(false) && candidate.is_dir() {
            return candidate.parent().map(|p| p.to_path_buf());
        }
        if !candidate.pop() {
            return None;
        }
    }
}

pub fn budi_home_dir() -> Result<PathBuf> {
    if let Ok(override_dir) = env::var(BUDI_HOME_ENV)
        && let Some(path) = parse_env_path(&override_dir)
    {
        return Ok(path);
    }
    #[cfg(windows)]
    {
        if let Ok(local_app_data) = env::var("LOCALAPPDATA") {
            return Ok(PathBuf::from(local_app_data).join("budi"));
        }
    }
    Ok(home_dir()?.join(BUDI_HOME_DEFAULT_REL))
}

/// Returns `~/.config/budi/` — the config directory for statusline.toml, tags.toml, etc.
pub fn budi_config_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".config/budi"))
}

// ---------------------------------------------------------------------------
// Cloud config — loaded from `~/.config/budi/cloud.toml` (ADR-0083 §9)
// ---------------------------------------------------------------------------

pub const DEFAULT_CLOUD_ENDPOINT: &str = "https://app.getbudi.dev";
pub const DEFAULT_CLOUD_SYNC_INTERVAL_SECONDS: u64 = 300;
pub const DEFAULT_CLOUD_SYNC_RETRY_MAX_SECONDS: u64 = 300;

/// Placeholder api_key string written by `budi cloud init` into a freshly
/// generated `cloud.toml`. `budi cloud status` surfaces this as
/// "disabled (stub key)" so the user sees a distinct next-step hint instead
/// of the generic "disabled" line they see when no config exists at all.
pub const CLOUD_API_KEY_STUB: &str = "PASTE_YOUR_KEY_HERE";

/// Cloud sync configuration loaded from `~/.config/budi/cloud.toml`.
/// Created by editing `~/.config/budi/cloud.toml` (see README § Cloud sync).
/// Cloud sync is **disabled by default** — requires explicit opt-in.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CloudConfig {
    pub enabled: bool,
    pub api_key: Option<String>,
    pub device_id: Option<String>,
    pub org_id: Option<String>,
    pub endpoint: String,
    pub sync: CloudSyncConfig,
    /// Human-friendly device label included in every ingest envelope.
    /// Rendered on the cloud dashboard's Devices page instead of the
    /// truncated `dev_<id>` we ship by default.
    ///
    /// - `None` (TOML key absent) → default to the local OS hostname
    ///   when the envelope is built.
    /// - `Some("")` (TOML `label = ""`) → explicit opt-out; the cloud
    ///   receives an empty label and falls back to whatever it renders
    ///   for a missing value. Raw hostnames can be PII, so the opt-out
    ///   path is deliberately surface-level simple (edit one line).
    /// - `Some("ivan-mbp")` → sent verbatim on every ingest, so a
    ///   rename propagates without re-linking the device.
    ///
    /// See [#552](https://github.com/siropkin/budi/issues/552) for the
    /// full UX decision and the paired cloud-side persistence ticket.
    pub label: Option<String>,
}

impl Default for CloudConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_key: None,
            device_id: None,
            org_id: None,
            endpoint: DEFAULT_CLOUD_ENDPOINT.to_string(),
            sync: CloudSyncConfig::default(),
            label: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CloudSyncConfig {
    pub interval_seconds: u64,
    pub retry_max_seconds: u64,
}

impl Default for CloudSyncConfig {
    fn default() -> Self {
        Self {
            interval_seconds: DEFAULT_CLOUD_SYNC_INTERVAL_SECONDS,
            retry_max_seconds: DEFAULT_CLOUD_SYNC_RETRY_MAX_SECONDS,
        }
    }
}

impl CloudConfig {
    /// Whether cloud sync should run, respecting `BUDI_CLOUD_ENABLED` env override.
    pub fn effective_enabled(&self) -> bool {
        if let Ok(val) = env::var("BUDI_CLOUD_ENABLED") {
            return val.trim().eq_ignore_ascii_case("true") || val.trim() == "1";
        }
        self.enabled
    }

    /// Effective API key, respecting `BUDI_CLOUD_API_KEY` env override.
    pub fn effective_api_key(&self) -> Option<String> {
        if let Ok(val) = env::var("BUDI_CLOUD_API_KEY") {
            let trimmed = val.trim().to_string();
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }
        self.api_key.clone()
    }

    /// Effective endpoint, respecting `BUDI_CLOUD_ENDPOINT` env override.
    pub fn effective_endpoint(&self) -> String {
        if let Ok(val) = env::var("BUDI_CLOUD_ENDPOINT") {
            let trimmed = val.trim().to_string();
            if !trimmed.is_empty() {
                return trimmed;
            }
        }
        self.endpoint.clone()
    }

    /// Resolve the device label sent with each ingest envelope (#552).
    ///
    /// Precedence:
    /// 1. `label` key present in `cloud.toml` → returned verbatim,
    ///    including `""` (explicit opt-out — the user chose to share
    ///    nothing human-readable).
    /// 2. `label` key absent → the local OS hostname from
    ///    [`crate::pipeline::enrichers::get_hostname`].
    ///
    /// An empty hostname still produces an empty string rather than a
    /// panic; the cloud dashboard treats empty labels as "fall back to
    /// whatever default label the server would render otherwise".
    pub fn effective_label(&self) -> String {
        if let Some(explicit) = self.label.as_ref() {
            return explicit.clone();
        }
        crate::pipeline::enrichers::get_hostname()
    }

    /// Returns true only if cloud sync is configured enough to run:
    /// enabled, has api_key, has device_id, has org_id.
    pub fn is_ready(&self) -> bool {
        self.effective_enabled()
            && self.effective_api_key().is_some()
            && self.device_id.is_some()
            && self.org_id.is_some()
    }

    /// Returns true when the `api_key` in the loaded config is exactly the
    /// placeholder string written by `budi cloud init`. Used by `budi cloud
    /// status` to surface "disabled (stub key)" separately from
    /// "disabled (no config)".
    pub fn is_api_key_stub(&self) -> bool {
        self.api_key.as_deref() == Some(CLOUD_API_KEY_STUB)
    }

    /// #540: short, stable snake_case reason tag for the "uploader
    /// disabled" daemon startup log line. Returns `None` when the
    /// config IS ready to run (caller should log the "configured" line
    /// instead). Precedence mirrors `budi cloud status`: the
    /// `enabled` flag is the coarsest gate, then api_key presence /
    /// staleness, then the identity pair. This string is logged
    /// verbatim as a structured field — do not change wording without
    /// updating any external log grep consumers.
    pub fn disabled_reason(&self) -> Option<&'static str> {
        if !self.effective_enabled() {
            return Some("cloud.enabled=false");
        }
        if self.effective_api_key().is_none() {
            return Some("missing api_key");
        }
        if self.is_api_key_stub() {
            return Some("api_key is placeholder");
        }
        if self.device_id.is_none() {
            return Some("missing device_id");
        }
        if self.org_id.is_none() {
            return Some("missing org_id");
        }
        None
    }
}

/// Path to the cloud config file.
pub fn cloud_config_path() -> Result<PathBuf> {
    Ok(budi_config_dir()?.join("cloud.toml"))
}

/// Returns true when `~/.config/budi/cloud.toml` exists on disk.
/// Swallows errors on path resolution so the callers (CLI render, daemon
/// status endpoint) treat an unreadable home as "no config" rather than
/// failing the surrounding command.
pub fn cloud_config_exists() -> bool {
    cloud_config_path()
        .ok()
        .map(|p| p.exists())
        .unwrap_or(false)
}

/// Outcome of a call to [`seed_cloud_device_id_if_needed`]. Surfaced to
/// the CLI so `budi init` can log exactly what happened (generated /
/// already set / skipped because cloud is disabled).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeedDeviceIdOutcome {
    /// `cloud.toml` didn't exist, cloud was disabled, or the api_key is
    /// still the stub. No seeding attempted. The CLI shouldn't nag the
    /// user — they haven't opted into cloud yet.
    Skipped,
    /// `device_id` was already present in the config file. No-op.
    AlreadySet,
    /// Generated a fresh UUID v4 and wrote it into the commented
    /// `# device_id = ...` slot. Returns the generated id so the CLI
    /// can log it for operator visibility.
    Generated(String),
}

/// If the on-disk cloud config has `enabled = true` + a non-stub
/// `api_key` + no `device_id`, generate a fresh UUID v4 and write it
/// into the `device_id` line. Preserves all other content (comments,
/// formatting, other fields) by doing a targeted string edit rather
/// than a serde round-trip that would drop comments.
///
/// Idempotent: returns `AlreadySet` on the second call without
/// touching the file. Returns `Skipped` whenever cloud sync isn't
/// configured enough to matter — fresh installs that never opted
/// into cloud never see a `cloud.toml` modification.
///
/// `org_id` is NOT auto-generated; it has to come from the dashboard
/// Settings page. The CLI render step nudges users to that value
/// separately.
///
/// See #521 for context.
pub fn seed_cloud_device_id_if_needed() -> Result<SeedDeviceIdOutcome> {
    let path = cloud_config_path()?;
    if !path.exists() {
        return Ok(SeedDeviceIdOutcome::Skipped);
    }

    // Re-read the raw file for string-level editing (parser drops comments).
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("read cloud config at {}", path.display()))?;

    // Parse the struct to check gating conditions (enabled + real api_key).
    let config = load_cloud_config();
    if !config.effective_enabled() {
        return Ok(SeedDeviceIdOutcome::Skipped);
    }
    let api_key = match config.effective_api_key() {
        Some(k) if !k.is_empty() && k != CLOUD_API_KEY_STUB => k,
        _ => return Ok(SeedDeviceIdOutcome::Skipped),
    };
    let _ = api_key; // used only for the gate

    if config.device_id.is_some() {
        return Ok(SeedDeviceIdOutcome::AlreadySet);
    }

    // Generate a fresh UUID v4. `uuid` is already a workspace dep.
    let new_device_id = uuid::Uuid::new_v4().to_string();

    // Surgical string edit: replace the commented template line. The
    // template writes exactly `# device_id = "your-device-id"`; match
    // that verbatim, fall back to appending a fresh `device_id` line
    // under `[cloud]` when the commented template line is absent (e.g.
    // the user hand-removed it).
    let commented = "# device_id = \"your-device-id\"";
    let uncommented = format!("device_id = \"{new_device_id}\"");

    let new_raw = if raw.contains(commented) {
        raw.replacen(commented, &uncommented, 1)
    } else {
        // Fallback: append the field under the `[cloud]` section. Walk
        // the raw lines to find the first `[cloud]` header line and
        // insert after the `api_key` line (so the field lives inside
        // the `[cloud]` table, not accidentally in `[cloud.sync]`).
        match find_insertion_line_for_device_id(&raw) {
            Some(idx) => {
                let mut lines: Vec<String> = raw.lines().map(str::to_string).collect();
                lines.insert(idx, uncommented.clone());
                // Preserve the trailing newline of the original file.
                let mut joined = lines.join("\n");
                if raw.ends_with('\n') {
                    joined.push('\n');
                }
                joined
            }
            None => {
                // Truly foreign structure — don't rewrite; let the
                // user hand-edit. Surface as skipped so the CLI
                // doesn't claim a generation that didn't happen.
                return Ok(SeedDeviceIdOutcome::Skipped);
            }
        }
    };

    fs::write(&path, new_raw)
        .with_context(|| format!("write cloud config at {}", path.display()))?;

    Ok(SeedDeviceIdOutcome::Generated(new_device_id))
}

/// Locate the line index in `raw` where a fresh `device_id = "..."`
/// line should be inserted when the commented template slot is
/// missing. Returns the line immediately after the last `api_key = ...`
/// line inside the `[cloud]` section (before any `[cloud.sync]` or
/// other table), or `None` if no such anchor is found.
fn find_insertion_line_for_device_id(raw: &str) -> Option<usize> {
    let mut in_cloud_section = false;
    let mut last_api_key_line: Option<usize> = None;
    for (i, line) in raw.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_cloud_section = trimmed == "[cloud]";
            continue;
        }
        if in_cloud_section && trimmed.starts_with("api_key") {
            last_api_key_line = Some(i + 1);
        }
    }
    last_api_key_line
}

/// Load cloud config. Returns default (disabled) if the file does not exist.
pub fn load_cloud_config() -> CloudConfig {
    let path = match cloud_config_path() {
        Ok(p) => p,
        Err(_) => return CloudConfig::default(),
    };
    if !path.exists() {
        return CloudConfig::default();
    }
    let raw = match fs::read_to_string(&path) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("Failed to read {}: {e}", path.display());
            return CloudConfig::default();
        }
    };
    // The TOML file uses a top-level [cloud] section per ADR-0083 §9.
    // We parse into a wrapper that extracts the [cloud] table.
    #[derive(Deserialize)]
    struct Wrapper {
        #[serde(default)]
        cloud: CloudConfig,
    }
    match toml::from_str::<Wrapper>(&raw) {
        Ok(w) => w.cloud,
        Err(e) => {
            tracing::warn!("Failed to parse {}: {e}", path.display());
            CloudConfig::default()
        }
    }
}

pub fn repo_paths(repo_root: &Path) -> Result<RepoPaths> {
    let repos_root = repos_root_dir()?;
    let repo_id = repo_storage_id(repo_root);
    let data_dir = repos_root.join(repo_id);
    let log_dir = data_dir.join(BUDI_LOG_DIR_NAME);
    Ok(RepoPaths {
        config_file: data_dir.join(BUDI_CONFIG_FILE_NAME),
        data_dir,
        log_dir,
    })
}

pub fn repos_root_dir() -> Result<PathBuf> {
    Ok(budi_home_dir()?.join(BUDI_REPOS_DIR))
}

pub fn config_path(repo_root: &Path) -> Result<PathBuf> {
    Ok(repo_paths(repo_root)?.config_file)
}

pub fn daemon_log_path(repo_root: &Path) -> Result<PathBuf> {
    Ok(repo_paths(repo_root)?.log_dir.join("daemon.log"))
}

pub fn repo_root_marker_path(data_dir: &Path) -> PathBuf {
    data_dir.join(BUDI_REPO_ROOT_MARKER_FILE_NAME)
}

pub fn load_or_default(repo_root: &Path) -> Result<BudiConfig> {
    let config_path = config_path(repo_root)?;
    if !config_path.exists() {
        return Ok(BudiConfig::default());
    }
    let raw = fs::read_to_string(&config_path)
        .with_context(|| format!("Failed reading {}", config_path.display()))?;
    let config: BudiConfig =
        toml::from_str(&raw).with_context(|| "Invalid budi config TOML".to_string())?;
    Ok(config)
}

pub fn save(repo_root: &Path, config: &BudiConfig) -> Result<()> {
    let config_path = config_path(repo_root)?;
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed creating {}", parent.display()))?;
    }
    let raw = toml::to_string_pretty(config)?;
    fs::write(&config_path, raw)
        .with_context(|| format!("Failed writing {}", config_path.display()))?;
    Ok(())
}

fn repo_storage_id(repo_root: &Path) -> String {
    let storage_root = resolve_storage_root(repo_root);
    let canonical = fs::canonicalize(&storage_root).unwrap_or_else(|_| storage_root.to_path_buf());
    let normalized = canonical.to_string_lossy().replace('\\', "/");
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    let digest = hasher.finalize();
    let hash_hex = digest
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>();

    let mut slug = storage_root
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or("repo")
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        slug = "repo".to_string();
    }
    if slug.len() > 32 {
        slug.truncate(32);
    }
    format!("{slug}-{}", &hash_hex[..12])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_storage_id_uses_slug_plus_short_hash() {
        let id = repo_storage_id(Path::new("/tmp/My Repo"));
        assert!(id.starts_with("my-repo-"));
        let hash_part = id.rsplit('-').next().unwrap_or_default();
        assert_eq!(hash_part.len(), 12);
        assert!(hash_part.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn resolve_storage_root_returns_self_for_normal_repo() {
        let repo = Path::new("/tmp/normal-repo");
        assert_eq!(resolve_storage_root(repo), repo);
    }

    #[test]
    fn resolve_worktree_main_root_parses_gitdir() {
        let tmp = std::env::temp_dir().join("budi-worktree-test");
        let main_root = tmp.join("main-repo");
        let main_git = main_root.join(".git");
        let wt_dir = main_git.join("worktrees").join("feature-branch");
        std::fs::create_dir_all(&wt_dir).unwrap();

        let wt_root = tmp.join("feature-branch");
        std::fs::create_dir_all(&wt_root).unwrap();
        let wt_git_file = wt_root.join(".git");
        std::fs::write(&wt_git_file, format!("gitdir: {}", wt_dir.display())).unwrap();

        let resolved = resolve_storage_root(&wt_root);
        assert_eq!(
            std::fs::canonicalize(&resolved).unwrap(),
            std::fs::canonicalize(&main_root).unwrap(),
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn repo_root_marker_round_trip() {
        let data_dir = PathBuf::from("/tmp/budi-marker-test");
        let marker_path = repo_root_marker_path(&data_dir);
        assert!(marker_path.ends_with(BUDI_REPO_ROOT_MARKER_FILE_NAME));
    }

    #[test]
    fn parse_env_path_rejects_blank_values() {
        assert!(parse_env_path("").is_none());
        assert!(parse_env_path("   ").is_none());
        assert!(parse_env_path("\n\t ").is_none());
    }

    #[test]
    fn parse_env_path_trims_whitespace() {
        let parsed = parse_env_path("  /tmp/budi-home  ").expect("path should parse");
        assert_eq!(parsed, PathBuf::from("/tmp/budi-home"));
    }

    #[test]
    fn statusline_config_default_slots() {
        let config = StatuslineConfig::default();
        assert_eq!(config.slots, vec!["1d", "7d", "30d"]);
        assert!(config.format.is_none());
    }

    #[test]
    fn statusline_config_required_slots_from_slots() {
        let config = StatuslineConfig {
            preset: None,
            slots: vec!["1d".to_string(), "branch".to_string()],
            format: None,
        };
        assert_eq!(config.required_slots(), vec!["1d", "branch"]);
    }

    #[test]
    fn statusline_config_required_slots_from_format() {
        let config = StatuslineConfig {
            preset: None,
            slots: vec![],
            format: Some("{1d} | {branch} | {provider}".to_string()),
        };
        let required = config.required_slots();
        assert_eq!(required, vec!["1d", "branch", "provider"]);
    }

    #[test]
    fn statusline_config_required_slots_ignores_unknown() {
        let config = StatuslineConfig {
            preset: None,
            slots: vec![],
            format: Some("{1d} | {unknown} | {7d}".to_string()),
        };
        let required = config.required_slots();
        assert_eq!(required, vec!["1d", "7d"]);
    }

    #[test]
    fn statusline_preset_overrides_slots() {
        let config = StatuslineConfig {
            preset: Some("coach".to_string()),
            slots: vec!["1d".to_string()],
            format: None,
        };
        assert_eq!(config.effective_slots(), vec!["session", "health"]);
        assert_eq!(config.required_slots(), vec!["session", "health"]);
    }

    #[test]
    fn statusline_format_overrides_preset() {
        let config = StatuslineConfig {
            preset: Some("coach".to_string()),
            slots: vec![],
            format: Some("{1d} | {7d}".to_string()),
        };
        assert_eq!(config.required_slots(), vec!["1d", "7d"]);
    }

    #[test]
    fn statusline_config_parse_toml() {
        let toml_str = r#"
slots = ["1d", "7d", "branch"]
format = "{1d} | {7d} | {branch}"
"#;
        let config: StatuslineConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.slots, vec!["1d", "7d", "branch"]);
        assert_eq!(config.format.unwrap(), "{1d} | {7d} | {branch}");
    }

    #[test]
    fn statusline_config_parse_minimal_toml() {
        let toml_str = r#"slots = ["30d", "project"]"#;
        let config: StatuslineConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.slots, vec!["30d", "project"]);
        assert!(config.format.is_none());
    }

    #[test]
    fn statusline_config_empty_toml_uses_defaults() {
        let config: StatuslineConfig = toml::from_str("").unwrap();
        assert_eq!(config.slots, vec!["1d", "7d", "30d"]);
    }

    #[test]
    fn statusline_config_legacy_slot_aliases_normalize() {
        // Existing configs that use the old calendar slot names still work —
        // today/week/month render the same rolling-window values as 1d/7d/30d.
        let config = StatuslineConfig {
            preset: None,
            slots: vec!["today".to_string(), "week".to_string(), "month".to_string()],
            format: None,
        };
        assert_eq!(config.effective_slots(), vec!["1d", "7d", "30d"]);
        assert_eq!(config.required_slots(), vec!["1d", "7d", "30d"]);
    }

    #[test]
    fn statusline_cost_preset_uses_rolling_windows() {
        let config = StatuslineConfig {
            preset: Some("cost".to_string()),
            slots: vec![],
            format: None,
        };
        assert_eq!(config.effective_slots(), vec!["1d", "7d", "30d"]);
    }

    #[test]
    fn agents_config_default_disables_all() {
        let config = AgentsConfig::default();
        assert!(!config.claude_code.enabled);
        assert!(!config.codex_cli.enabled);
        assert!(!config.cursor.enabled);
        assert!(!config.copilot_cli.enabled);
        assert!(!config.is_agent_enabled("claude_code"));
        assert!(!config.is_agent_enabled("codex"));
        assert!(!config.is_agent_enabled("codex_cli"));
        assert!(!config.is_agent_enabled("cursor"));
        assert!(!config.is_agent_enabled("copilot_cli"));
    }

    #[test]
    fn agents_config_all_enabled() {
        let config = AgentsConfig::all_enabled();
        assert!(config.is_agent_enabled("claude_code"));
        assert!(config.is_agent_enabled("codex"));
        assert!(config.is_agent_enabled("codex_cli"));
        assert!(config.is_agent_enabled("cursor"));
        assert!(config.is_agent_enabled("copilot_cli"));
    }

    #[test]
    fn agents_config_unknown_provider_disabled() {
        let config = AgentsConfig::all_enabled();
        assert!(!config.is_agent_enabled("gemini"));
    }

    #[test]
    fn agents_config_round_trips_toml() {
        let config = AgentsConfig {
            claude_code: AgentEntry { enabled: true },
            codex_cli: AgentEntry { enabled: true },
            cursor: AgentEntry { enabled: false },
            copilot_cli: AgentEntry { enabled: false },
        };
        let raw = toml::to_string_pretty(&config).unwrap();
        assert!(raw.contains("[claude-code]"));
        assert!(raw.contains("[codex-cli]"));
        assert!(raw.contains("enabled = true"));
        let parsed: AgentsConfig = toml::from_str(&raw).unwrap();
        assert!(parsed.claude_code.enabled);
        assert!(parsed.codex_cli.enabled);
        assert!(!parsed.cursor.enabled);
        assert!(!parsed.copilot_cli.enabled);
    }

    #[test]
    fn agents_config_parses_partial_toml() {
        let toml_str = r#"
[claude-code]
enabled = true
"#;
        let config: AgentsConfig = toml::from_str(toml_str).unwrap();
        assert!(config.claude_code.enabled);
        assert!(!config.cursor.enabled);
    }

    // --- Cloud config tests ---

    #[test]
    fn cloud_config_defaults() {
        let config = CloudConfig::default();
        assert!(!config.enabled);
        assert!(config.api_key.is_none());
        assert!(config.device_id.is_none());
        assert!(config.org_id.is_none());
        assert_eq!(config.endpoint, "https://app.getbudi.dev");
        assert_eq!(config.sync.interval_seconds, 300);
        assert_eq!(config.sync.retry_max_seconds, 300);
    }

    #[test]
    fn cloud_config_parses_full_toml() {
        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default)]
            cloud: CloudConfig,
        }
        let toml_str = r#"
[cloud]
enabled = true
api_key = "budi_abc123"
device_id = "dev_xyz"
org_id = "org_test"
endpoint = "https://custom.example.com"

[cloud.sync]
interval_seconds = 60
retry_max_seconds = 120
"#;
        let w: Wrapper = toml::from_str(toml_str).unwrap();
        let config = w.cloud;
        assert!(config.enabled);
        assert_eq!(config.api_key.as_deref(), Some("budi_abc123"));
        assert_eq!(config.device_id.as_deref(), Some("dev_xyz"));
        assert_eq!(config.org_id.as_deref(), Some("org_test"));
        assert_eq!(config.endpoint, "https://custom.example.com");
        assert_eq!(config.sync.interval_seconds, 60);
        assert_eq!(config.sync.retry_max_seconds, 120);
    }

    #[test]
    fn cloud_config_is_ready_requires_all_fields() {
        let mut config = CloudConfig::default();
        assert!(!config.is_ready());

        config.enabled = true;
        assert!(!config.is_ready());

        config.api_key = Some("budi_test".into());
        assert!(!config.is_ready());

        config.device_id = Some("dev_test".into());
        assert!(!config.is_ready());

        config.org_id = Some("org_test".into());
        assert!(config.is_ready());
    }

    #[test]
    fn cloud_config_is_api_key_stub_only_for_placeholder() {
        let mut config = CloudConfig::default();
        assert!(!config.is_api_key_stub());

        config.api_key = Some(CLOUD_API_KEY_STUB.to_string());
        assert!(config.is_api_key_stub());

        config.api_key = Some("budi_real_key".to_string());
        assert!(!config.is_api_key_stub());

        config.api_key = Some(format!("  {CLOUD_API_KEY_STUB}  "));
        assert!(
            !config.is_api_key_stub(),
            "stub detection is exact-match so accidental padding surfaces as a real (broken) key"
        );
    }

    #[test]
    fn cloud_config_disabled_reason_walks_precedence() {
        // #540: reason tag taxonomy — same order `budi cloud status`
        // surfaces missing fields, so the daemon startup log and the
        // CLI status command agree on "what's blocking cloud sync".
        let mut config = CloudConfig::default();
        // Default (enabled=false, everything None): coarsest gate fires first.
        assert_eq!(config.disabled_reason(), Some("cloud.enabled=false"));

        config.enabled = true;
        // Enabled but no api_key at all.
        assert_eq!(config.disabled_reason(), Some("missing api_key"));

        config.api_key = Some(CLOUD_API_KEY_STUB.to_string());
        // Placeholder distinguished from truly-missing.
        assert_eq!(config.disabled_reason(), Some("api_key is placeholder"));

        config.api_key = Some("budi_real".to_string());
        // Real key, but no device_id.
        assert_eq!(config.disabled_reason(), Some("missing device_id"));

        config.device_id = Some("dev_test".to_string());
        // device_id set, but no org_id.
        assert_eq!(config.disabled_reason(), Some("missing org_id"));

        config.org_id = Some("org_test".to_string());
        // Everything populated: no disabled reason — caller should log "configured".
        assert_eq!(config.disabled_reason(), None);
        assert!(config.is_ready());
    }

    #[test]
    fn cloud_config_partial_toml_uses_defaults() {
        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default)]
            cloud: CloudConfig,
        }
        let toml_str = r#"
[cloud]
enabled = true
api_key = "budi_test"
"#;
        let w: Wrapper = toml::from_str(toml_str).unwrap();
        let config = w.cloud;
        assert!(config.enabled);
        assert_eq!(config.api_key.as_deref(), Some("budi_test"));
        assert_eq!(config.endpoint, "https://app.getbudi.dev");
        assert_eq!(config.sync.interval_seconds, 300);
    }

    /// #521: locate the insertion line for a fresh `device_id` when the
    /// commented-template slot has been removed by a hand-edit. The
    /// helper is a pure string pass — no file I/O — so we can unit-
    /// test every branch without touching `~/.config`.
    #[test]
    fn find_insertion_line_for_device_id_anchors_after_api_key() {
        let raw = r#"# comment
[cloud]
enabled = true
api_key = "budi_real"
endpoint = "https://app.getbudi.dev"

[cloud.sync]
interval_seconds = 300
"#;
        let idx = find_insertion_line_for_device_id(raw).expect("should find an anchor");
        // The line after `api_key = "budi_real"` is line index 3 (0-based),
        // so insertion index is 4.
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines[idx - 1], "api_key = \"budi_real\"");
    }

    #[test]
    fn find_insertion_line_for_device_id_returns_none_without_cloud_section() {
        let raw = r#"# something else
[unrelated]
foo = "bar"
"#;
        assert!(find_insertion_line_for_device_id(raw).is_none());
    }

    #[test]
    fn find_insertion_line_for_device_id_skips_cloud_sync_section() {
        // `api_key` inside `[cloud.sync]` (unlikely but possible) must
        // NOT be treated as the anchor — we want the `[cloud]` table's
        // api_key.
        let raw = r#"[cloud]
enabled = true

[cloud.sync]
api_key = "wrong_table"
"#;
        // No `api_key` exists in the `[cloud]` section itself, so the
        // helper returns None rather than anchoring inside `[cloud.sync]`.
        assert!(find_insertion_line_for_device_id(raw).is_none());
    }
}
