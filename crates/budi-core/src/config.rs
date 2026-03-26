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

/// Cross-platform home directory detection.
/// Uses HOME on Unix, USERPROFILE (then HOMEPATH) on Windows.
pub fn home_dir() -> Result<PathBuf> {
    if let Ok(home) = env::var("HOME") {
        return Ok(PathBuf::from(home));
    }
    #[cfg(windows)]
    {
        if let Ok(profile) = env::var("USERPROFILE") {
            return Ok(PathBuf::from(profile));
        }
        if let (Ok(drive), Ok(path)) = (env::var("HOMEDRIVE"), env::var("HOMEPATH")) {
            return Ok(PathBuf::from(format!("{drive}{path}")));
        }
    }
    anyhow::bail!("Could not determine home directory (HOME not set)")
}

pub const DEFAULT_DAEMON_HOST: &str = "127.0.0.1";
pub const DEFAULT_DAEMON_PORT: u16 = 7878;

/// Known statusline slot names.
pub const STATUSLINE_SLOTS: &[&str] = &[
    "today", "week", "month", "session", "branch", "project", "provider",
];

/// User-configurable statusline layout.
///
/// Loaded from `~/.config/budi/statusline.toml`.
/// Example:
/// ```toml
/// slots = ["today", "week", "month", "branch"]
/// # format = "{today} | {week} | {month}"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StatuslineConfig {
    /// Ordered list of data slots to display. Default: ["today", "week", "month"].
    pub slots: Vec<String>,
    /// Optional custom format template. Overrides `slots` when set.
    /// Placeholders: {today}, {week}, {month}, {session}, {branch}, {project}, {provider}
    pub format: Option<String>,
}

impl Default for StatuslineConfig {
    fn default() -> Self {
        Self {
            slots: vec!["today".to_string(), "week".to_string(), "month".to_string()],
            format: None,
        }
    }
}

impl StatuslineConfig {
    /// Resolve which slots are needed (from format template or explicit slots list).
    pub fn required_slots(&self) -> Vec<String> {
        if let Some(ref fmt) = self.format {
            // Extract {placeholder} names from the format string
            let mut slots = Vec::new();
            let mut rest = fmt.as_str();
            while let Some(start) = rest.find('{') {
                if let Some(end) = rest[start..].find('}') {
                    let name = &rest[start + 1..start + end];
                    if STATUSLINE_SLOTS.contains(&name) && !slots.contains(&name.to_string()) {
                        slots.push(name.to_string());
                    }
                    rest = &rest[start + end + 1..];
                } else {
                    break;
                }
            }
            slots
        } else {
            self.slots.clone()
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
pub fn load_tags_config() -> Option<TagsConfig> {
    let path = tags_config_path().ok()?;
    if !path.exists() {
        return None;
    }
    let raw = fs::read_to_string(&path).ok()?;
    toml::from_str(&raw).ok()
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
    if let Ok(override_dir) = env::var(BUDI_HOME_ENV) {
        return Ok(PathBuf::from(override_dir));
    }
    Ok(home_dir()?.join(BUDI_HOME_DEFAULT_REL))
}

/// Returns `~/.config/budi/` — the config directory for statusline.toml, tags.toml, etc.
pub fn budi_config_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".config/budi"))
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

pub fn ensure_repo_layout(repo_root: &Path) -> Result<()> {
    let paths = repo_paths(repo_root)?;
    fs::create_dir_all(&paths.data_dir)
        .with_context(|| format!("Failed to create {}", paths.data_dir.display()))?;
    fs::create_dir_all(&paths.log_dir)
        .with_context(|| format!("Failed to create {}", paths.log_dir.display()))?;
    fs::create_dir_all(repo_root.join(".claude"))
        .with_context(|| "Failed to create .claude".to_string())?;
    let canonical_repo_root =
        fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    let marker_path = repo_root_marker_path(&paths.data_dir);
    fs::write(&marker_path, canonical_repo_root.display().to_string())
        .with_context(|| format!("Failed writing {}", marker_path.display()))?;
    Ok(())
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
    fn statusline_config_default_slots() {
        let config = StatuslineConfig::default();
        assert_eq!(config.slots, vec!["today", "week", "month"]);
        assert!(config.format.is_none());
    }

    #[test]
    fn statusline_config_required_slots_from_slots() {
        let config = StatuslineConfig {
            slots: vec!["today".to_string(), "branch".to_string()],
            format: None,
        };
        assert_eq!(config.required_slots(), vec!["today", "branch"]);
    }

    #[test]
    fn statusline_config_required_slots_from_format() {
        let config = StatuslineConfig {
            slots: vec![],
            format: Some("{today} | {branch} | {provider}".to_string()),
        };
        let required = config.required_slots();
        assert_eq!(required, vec!["today", "branch", "provider"]);
    }

    #[test]
    fn statusline_config_required_slots_ignores_unknown() {
        let config = StatuslineConfig {
            slots: vec![],
            format: Some("{today} | {unknown} | {week}".to_string()),
        };
        let required = config.required_slots();
        assert_eq!(required, vec!["today", "week"]);
    }

    #[test]
    fn statusline_config_parse_toml() {
        let toml_str = r#"
slots = ["today", "week", "branch"]
format = "{today} | {week} | {branch}"
"#;
        let config: StatuslineConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.slots, vec!["today", "week", "branch"]);
        assert_eq!(config.format.unwrap(), "{today} | {week} | {branch}");
    }

    #[test]
    fn statusline_config_parse_minimal_toml() {
        let toml_str = r#"slots = ["month", "project"]"#;
        let config: StatuslineConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.slots, vec!["month", "project"]);
        assert!(config.format.is_none());
    }

    #[test]
    fn statusline_config_empty_toml_uses_defaults() {
        let config: StatuslineConfig = toml::from_str("").unwrap();
        assert_eq!(config.slots, vec!["today", "week", "month"]);
    }
}
