use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config;

pub const SHELL_BLOCK_START: &str = "# >>> budi >>>";
pub const SHELL_BLOCK_END: &str = "# <<< budi <<<";
pub const CURSOR_BLOCK_START: &str = "// >>> budi >>>";
pub const CURSOR_BLOCK_END: &str = "// <<< budi <<<";

const CURSOR_OPENAI_BASE_URL_KEY: &str = "openai.baseUrl";
const UPGRADE_NOTICE_DIR: &str = "upgrade-flags";
const UPGRADE_NOTICE_FILE: &str = "legacy-proxy-cleanup-v8.2.flag";
const PROXY_ENV_VARS: [&str; 5] = [
    "ANTHROPIC_BASE_URL",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
    "OPENAI_BASE_URL",
    "COPILOT_PROVIDER_BASE_URL",
    "COPILOT_PROVIDER_TYPE",
];
const EXPORTED_PROXY_URL_ENV_VARS: [&str; 3] = [
    "ANTHROPIC_BASE_URL",
    "OPENAI_BASE_URL",
    "COPILOT_PROVIDER_BASE_URL",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyProxySurface {
    ShellProfile,
    CursorSettings,
    CodexConfig,
}

impl LegacyProxySurface {
    pub fn display_name(self) -> &'static str {
        match self {
            Self::ShellProfile => "shell profile",
            Self::CursorSettings => "Cursor settings",
            Self::CodexConfig => "Codex config",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedBlock {
    pub start_line: usize,
    pub end_line: Option<usize>,
    pub lines: Vec<String>,
}

impl ManagedBlock {
    pub fn is_corrupted(&self) -> bool {
        self.end_line.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuzzyFinding {
    pub line_number: usize,
    pub label: String,
    pub snippet: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportedEnvVar {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyProxyFile {
    pub surface: LegacyProxySurface,
    pub path: PathBuf,
    pub original_text: String,
    pub cleaned_text: String,
    pub managed_blocks: Vec<ManagedBlock>,
    pub fuzzy_findings: Vec<FuzzyFinding>,
}

impl LegacyProxyFile {
    pub fn has_managed_blocks(&self) -> bool {
        !self.managed_blocks.is_empty()
    }

    pub fn has_fuzzy_findings(&self) -> bool {
        !self.fuzzy_findings.is_empty()
    }

    pub fn apply_cleanup(&self) -> Result<bool> {
        if !self.has_managed_blocks() || self.cleaned_text == self.original_text {
            return Ok(false);
        }
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }
        fs::write(&self.path, &self.cleaned_text)
            .with_context(|| format!("Failed writing {}", self.path.display()))?;
        Ok(true)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyProxyScan {
    pub files: Vec<LegacyProxyFile>,
    pub exported_env_vars: Vec<ExportedEnvVar>,
}

impl LegacyProxyScan {
    pub fn has_any_residue(&self) -> bool {
        !self.files.is_empty() || !self.exported_env_vars.is_empty()
    }

    pub fn has_managed_blocks(&self) -> bool {
        self.files.iter().any(LegacyProxyFile::has_managed_blocks)
    }

    pub fn managed_file_count(&self) -> usize {
        self.files
            .iter()
            .filter(|file| file.has_managed_blocks())
            .count()
    }

    pub fn fuzzy_file_count(&self) -> usize {
        self.files
            .iter()
            .filter(|file| file.has_fuzzy_findings())
            .count()
    }

    pub fn total_fuzzy_findings(&self) -> usize {
        self.files
            .iter()
            .map(|file| file.fuzzy_findings.len())
            .sum()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpgradeNoticeOutcome {
    NotNeeded,
    AlreadyLogged,
    LoggedNow,
}

#[derive(Debug, Clone)]
struct EnvContext {
    shell: Option<String>,
    appdata: Option<String>,
    codex_home: Option<String>,
    exported_env_vars: Vec<ExportedEnvVar>,
}

impl EnvContext {
    fn capture() -> Self {
        Self {
            shell: std::env::var("SHELL").ok(),
            appdata: std::env::var("APPDATA").ok(),
            codex_home: std::env::var("CODEX_HOME").ok(),
            exported_env_vars: current_process_proxy_url_env_vars(),
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlatformFamily {
    Macos,
    Linux,
    Windows,
}

#[derive(Debug, Clone)]
struct StripResult {
    cleaned_text: String,
    managed_blocks: Vec<ManagedBlock>,
}

pub fn scan() -> Result<LegacyProxyScan> {
    let home = config::home_dir()?;
    scan_with_env(&home, current_platform(), &EnvContext::capture())
}

pub fn emit_upgrade_notice_once() -> Result<UpgradeNoticeOutcome> {
    let home = config::home_dir()?;
    let scan = scan_with_env(&home, current_platform(), &EnvContext::capture())?;
    let flag_path = upgrade_notice_flag_path()?;
    emit_upgrade_notice_once_for_scan(&scan, &flag_path)
}

fn emit_upgrade_notice_once_for_scan(
    scan: &LegacyProxyScan,
    flag_path: &Path,
) -> Result<UpgradeNoticeOutcome> {
    if !scan.has_managed_blocks() {
        return Ok(UpgradeNoticeOutcome::NotNeeded);
    }
    if flag_path.exists() {
        return Ok(UpgradeNoticeOutcome::AlreadyLogged);
    }
    if let Some(parent) = flag_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }

    let managed_paths = scan
        .files
        .iter()
        .filter(|file| file.has_managed_blocks())
        .map(|file| file.path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");

    tracing::warn!(
        managed_files = scan.managed_file_count(),
        paths = %managed_paths,
        "Detected legacy 8.0/8.1 proxy config residue managed by Budi. Run `budi init --cleanup` to review and remove it before those stale localhost routes break your agents on 8.2."
    );

    fs::write(flag_path, "logged\n")
        .with_context(|| format!("Failed writing {}", flag_path.display()))?;
    Ok(UpgradeNoticeOutcome::LoggedNow)
}

fn scan_with_env(
    home: &Path,
    platform: PlatformFamily,
    env: &EnvContext,
) -> Result<LegacyProxyScan> {
    let mut files = Vec::new();

    for path in shell_profile_candidates(home, platform, env.shell.as_deref()) {
        if let Some(file) = scan_shell_profile(&path)? {
            files.push(file);
        }
    }

    for path in cursor_settings_candidates(home, platform, env.appdata.as_deref()) {
        if let Some(file) = scan_cursor_settings(&path)? {
            files.push(file);
        }
    }

    for path in codex_config_candidates(home, env.codex_home.as_deref()) {
        if let Some(file) = scan_codex_config(&path)? {
            files.push(file);
        }
    }

    Ok(LegacyProxyScan {
        files,
        exported_env_vars: env.exported_env_vars.clone(),
    })
}

fn scan_shell_profile(path: &Path) -> Result<Option<LegacyProxyFile>> {
    scan_text_file(
        path,
        LegacyProxySurface::ShellProfile,
        shell_marker_variants(),
        scan_shell_env_findings,
    )
}

fn scan_cursor_settings(path: &Path) -> Result<Option<LegacyProxyFile>> {
    scan_text_file(
        path,
        LegacyProxySurface::CursorSettings,
        cursor_marker_variants(),
        scan_cursor_settings_findings,
    )
}

fn scan_codex_config(path: &Path) -> Result<Option<LegacyProxyFile>> {
    scan_text_file(
        path,
        LegacyProxySurface::CodexConfig,
        shell_marker_variants(),
        |_| Vec::new(),
    )
}

fn scan_text_file<F>(
    path: &Path,
    surface: LegacyProxySurface,
    marker_variants: &[(&str, &str)],
    fuzzy_scan: F,
) -> Result<Option<LegacyProxyFile>>
where
    F: Fn(&str) -> Vec<FuzzyFinding>,
{
    if !path.exists() {
        return Ok(None);
    }
    let original_text =
        fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;

    let stripped = strip_managed_blocks(&original_text, marker_variants);
    let cleaned_text = if surface == LegacyProxySurface::CursorSettings {
        trim_trailing_comma_before_closing_brace(&stripped.cleaned_text)
    } else {
        stripped.cleaned_text
    };
    let fuzzy_findings = fuzzy_scan(&cleaned_text);

    if stripped.managed_blocks.is_empty() && fuzzy_findings.is_empty() {
        return Ok(None);
    }

    Ok(Some(LegacyProxyFile {
        surface,
        path: path.to_path_buf(),
        original_text,
        cleaned_text,
        managed_blocks: stripped.managed_blocks,
        fuzzy_findings,
    }))
}

fn shell_marker_variants() -> &'static [(&'static str, &'static str)] {
    &[(SHELL_BLOCK_START, SHELL_BLOCK_END)]
}

fn cursor_marker_variants() -> &'static [(&'static str, &'static str)] {
    &[(CURSOR_BLOCK_START, CURSOR_BLOCK_END)]
}

fn strip_managed_blocks(raw: &str, marker_variants: &[(&str, &str)]) -> StripResult {
    let mut kept_lines = Vec::new();
    let mut managed_blocks = Vec::new();
    let mut in_block = false;
    let mut current_end = "";
    let mut current_start_line = 0usize;
    let mut current_lines: Vec<String> = Vec::new();

    for (index, line) in raw.lines().enumerate() {
        let line_number = index + 1;
        let trimmed = line.trim();

        if !in_block {
            if let Some((_, end)) = marker_variants.iter().find(|(start, _)| trimmed == *start) {
                in_block = true;
                current_end = end;
                current_start_line = line_number;
                current_lines.push(line.to_string());
                continue;
            }

            kept_lines.push(line.to_string());
            continue;
        }

        current_lines.push(line.to_string());
        if trimmed == current_end {
            managed_blocks.push(ManagedBlock {
                start_line: current_start_line,
                end_line: Some(line_number),
                lines: std::mem::take(&mut current_lines),
            });
            in_block = false;
            current_end = "";
            current_start_line = 0;
        }
    }

    if in_block {
        managed_blocks.push(ManagedBlock {
            start_line: current_start_line,
            end_line: None,
            lines: current_lines,
        });
    }

    StripResult {
        cleaned_text: rebuild_text(&kept_lines, raw),
        managed_blocks,
    }
}

fn rebuild_text(lines: &[String], original: &str) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let newline = newline_style(original);
    let mut rebuilt = lines.join(newline);
    if original.ends_with("\r\n") {
        rebuilt.push_str("\r\n");
    } else if original.ends_with('\n') {
        rebuilt.push('\n');
    }
    rebuilt
}

fn newline_style(raw: &str) -> &'static str {
    if raw.contains("\r\n") { "\r\n" } else { "\n" }
}

fn trim_trailing_comma_before_closing_brace(raw: &str) -> String {
    let Some(close_idx) = raw.rfind('}') else {
        return raw.to_string();
    };

    let mut idx = close_idx;
    let bytes = raw.as_bytes();
    while idx > 0 && bytes[idx - 1].is_ascii_whitespace() {
        idx -= 1;
    }

    if idx > 0 && bytes[idx - 1] == b',' {
        let mut out = String::new();
        out.push_str(&raw[..idx - 1]);
        out.push_str(&raw[idx..]);
        out
    } else {
        raw.to_string()
    }
}

fn scan_shell_env_findings(raw: &str) -> Vec<FuzzyFinding> {
    let mut findings = Vec::new();
    for (index, line) in raw.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') || trimmed.starts_with("//") {
            continue;
        }
        for key in PROXY_ENV_VARS {
            if looks_like_env_assignment(trimmed, key) {
                findings.push(FuzzyFinding {
                    line_number: index + 1,
                    label: key.to_string(),
                    snippet: line.trim().to_string(),
                });
            }
        }
    }
    findings
}

fn scan_cursor_settings_findings(raw: &str) -> Vec<FuzzyFinding> {
    let mut findings = Vec::new();
    for (index, line) in raw.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        if lower.contains(CURSOR_OPENAI_BASE_URL_KEY.to_ascii_lowercase().as_str())
            && looks_like_legacy_proxy_url(&lower)
        {
            findings.push(FuzzyFinding {
                line_number: index + 1,
                label: CURSOR_OPENAI_BASE_URL_KEY.to_string(),
                snippet: line.trim().to_string(),
            });
        }
    }
    findings
}

fn looks_like_env_assignment(line: &str, key: &str) -> bool {
    let without_export = line.strip_prefix("export ").unwrap_or(line).trim_start();
    let Some(rest) = without_export.strip_prefix(key) else {
        return false;
    };
    rest.trim_start().starts_with('=')
}

fn current_process_proxy_url_env_vars() -> Vec<ExportedEnvVar> {
    EXPORTED_PROXY_URL_ENV_VARS
        .into_iter()
        .filter_map(|key| {
            let value = std::env::var(key).ok()?;
            if looks_like_legacy_proxy_url(&value) {
                Some(ExportedEnvVar {
                    key: key.to_string(),
                    value,
                })
            } else {
                None
            }
        })
        .collect()
}

fn looks_like_legacy_proxy_url(value: &str) -> bool {
    let lower = value.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return false;
    }
    lower.contains("localhost:9878")
        || lower.contains("127.0.0.1:9878")
        || lower.contains("[::1]:9878")
}

fn current_platform() -> PlatformFamily {
    #[cfg(target_os = "macos")]
    {
        PlatformFamily::Macos
    }
    #[cfg(target_os = "windows")]
    {
        return PlatformFamily::Windows;
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        PlatformFamily::Linux
    }
}

fn shell_profile_candidates(
    home: &Path,
    platform: PlatformFamily,
    shell: Option<&str>,
) -> Vec<PathBuf> {
    if platform == PlatformFamily::Windows {
        return Vec::new();
    }

    let zsh = home.join(".zshrc");
    let bashrc = home.join(".bashrc");
    let bash_profile = home.join(".bash_profile");

    let mut candidates = Vec::new();
    if let Some(shell) = shell {
        let lower = shell.to_ascii_lowercase();
        if lower.contains("zsh") {
            candidates.push(zsh.clone());
        }
        if lower.contains("bash") {
            candidates.push(bashrc.clone());
            candidates.push(bash_profile.clone());
        }
    }
    candidates.push(zsh);
    candidates.push(bashrc);
    candidates.push(bash_profile);
    dedup_paths(candidates)
}

fn cursor_settings_candidates(
    home: &Path,
    platform: PlatformFamily,
    appdata: Option<&str>,
) -> Vec<PathBuf> {
    let candidates = match platform {
        PlatformFamily::Macos => vec![
            home.join("Library/Application Support/Cursor/User/settings.json"),
            home.join(".cursor/settings.json"),
        ],
        PlatformFamily::Linux => vec![
            home.join(".config/Cursor/User/settings.json"),
            home.join(".cursor/settings.json"),
        ],
        PlatformFamily::Windows => {
            let mut candidates = Vec::new();
            if let Some(appdata) = appdata {
                let trimmed = appdata.trim();
                if !trimmed.is_empty() {
                    candidates.push(PathBuf::from(trimmed).join("Cursor/User/settings.json"));
                }
            }
            candidates.push(home.join("AppData/Roaming/Cursor/User/settings.json"));
            candidates.push(home.join(".cursor/settings.json"));
            candidates
        }
    };
    dedup_paths(candidates)
}

fn codex_config_candidates(home: &Path, codex_home: Option<&str>) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(raw) = codex_home {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            candidates.push(PathBuf::from(trimmed).join("config.toml"));
        }
    }
    candidates.push(home.join(".codex/config.toml"));
    dedup_paths(candidates)
}

fn dedup_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut deduped = Vec::new();
    for path in paths {
        if seen.insert(path.clone()) {
            deduped.push(path);
        }
    }
    deduped
}

fn upgrade_notice_flag_path() -> Result<PathBuf> {
    Ok(config::budi_home_dir()?
        .join(UPGRADE_NOTICE_DIR)
        .join(UPGRADE_NOTICE_FILE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_dir(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "budi-legacy-proxy-{name}-{}-{stamp}",
            std::process::id()
        ))
    }

    #[test]
    fn strip_managed_blocks_removes_complete_block() {
        let raw = "alpha\n# >>> budi >>>\nexport X=1\n# <<< budi <<<\nomega\n";
        let stripped = strip_managed_blocks(raw, shell_marker_variants());
        assert_eq!(stripped.cleaned_text, "alpha\nomega\n");
        assert_eq!(stripped.managed_blocks.len(), 1);
        assert_eq!(stripped.managed_blocks[0].start_line, 2);
        assert_eq!(stripped.managed_blocks[0].end_line, Some(4));
    }

    #[test]
    fn strip_managed_blocks_marks_unterminated_block_as_corrupted() {
        let raw = "alpha\n# >>> budi >>>\nexport OPENAI_BASE_URL=http://localhost:9878\n";
        let stripped = strip_managed_blocks(raw, shell_marker_variants());
        assert_eq!(stripped.cleaned_text, "alpha\n");
        assert_eq!(stripped.managed_blocks.len(), 1);
        assert!(stripped.managed_blocks[0].is_corrupted());
    }

    #[test]
    fn scan_shell_env_findings_ignores_commented_lines() {
        let findings = scan_shell_env_findings(
            "# export OPENAI_BASE_URL=http://localhost:9878\nexport OPENAI_BASE_URL=http://localhost:9878\n",
        );
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].label, "OPENAI_BASE_URL");
    }

    #[test]
    fn scan_cursor_settings_findings_only_flags_localhost_values() {
        let findings = scan_cursor_settings_findings(
            "{\n  \"openai.baseUrl\": \"http://127.0.0.1:9878\"\n}\n",
        );
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].label, "openai.baseUrl");

        let clean = scan_cursor_settings_findings(
            "{\n  \"openai.baseUrl\": \"https://api.openai.com\"\n}\n",
        );
        assert!(clean.is_empty());
    }

    #[test]
    fn shell_profile_candidates_cover_mac_and_linux_shapes() {
        let home = Path::new("/tmp/home");
        assert_eq!(
            shell_profile_candidates(home, PlatformFamily::Macos, Some("/bin/zsh")),
            vec![
                home.join(".zshrc"),
                home.join(".bashrc"),
                home.join(".bash_profile"),
            ]
        );
        assert_eq!(
            shell_profile_candidates(home, PlatformFamily::Linux, Some("/bin/bash")),
            vec![
                home.join(".bashrc"),
                home.join(".bash_profile"),
                home.join(".zshrc"),
            ]
        );
    }

    #[test]
    fn cursor_settings_candidates_cover_windows_and_fallbacks() {
        let home = Path::new("/tmp/home");
        assert_eq!(
            cursor_settings_candidates(home, PlatformFamily::Macos, None),
            vec![
                home.join("Library/Application Support/Cursor/User/settings.json"),
                home.join(".cursor/settings.json"),
            ]
        );
        assert_eq!(
            cursor_settings_candidates(
                home,
                PlatformFamily::Windows,
                Some("C:/Users/test/AppData/Roaming"),
            ),
            vec![
                PathBuf::from("C:/Users/test/AppData/Roaming/Cursor/User/settings.json"),
                home.join("AppData/Roaming/Cursor/User/settings.json"),
                home.join(".cursor/settings.json"),
            ]
        );
    }

    #[test]
    fn codex_config_candidates_include_codex_home_override() {
        let home = Path::new("/tmp/home");
        assert_eq!(
            codex_config_candidates(home, Some("/tmp/codex-home")),
            vec![
                PathBuf::from("/tmp/codex-home/config.toml"),
                home.join(".codex/config.toml"),
            ]
        );
    }

    #[test]
    fn upgrade_notice_outcome_only_logs_once_when_managed_block_exists() {
        let home = unique_temp_dir("notice");
        let flag_path = unique_temp_dir("notice-flag").join("legacy.flag");
        fs::create_dir_all(&home).unwrap();
        fs::write(
            home.join(".zshrc"),
            "# >>> budi >>>\nexport OPENAI_BASE_URL=http://localhost:9878\n# <<< budi <<<\n",
        )
        .unwrap();

        let env = EnvContext {
            shell: Some("/bin/zsh".to_string()),
            appdata: None,
            codex_home: None,
            exported_env_vars: Vec::new(),
        };
        let scan = scan_with_env(&home, PlatformFamily::Macos, &env).unwrap();

        let first = emit_upgrade_notice_once_for_scan(&scan, &flag_path).unwrap();
        let second = emit_upgrade_notice_once_for_scan(&scan, &flag_path).unwrap();

        assert_eq!(first, UpgradeNoticeOutcome::LoggedNow);
        assert_eq!(second, UpgradeNoticeOutcome::AlreadyLogged);

        let _ = fs::remove_dir_all(&home);
        if let Some(parent) = flag_path.parent() {
            let _ = fs::remove_dir_all(parent);
        }
    }
}
