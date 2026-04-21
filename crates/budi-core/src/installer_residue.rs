//! Detects and removes residue the standalone shell / PowerShell installers
//! wrote outside Budi's own data/config directories.
//!
//! The standalone installers (`scripts/install-standalone.sh`,
//! `scripts/install.sh`) append a two-line block to the user's shell profile
//! when `BIN_DIR` is not already on `$PATH`:
//!
//! ```text
//! # Added by budi installer
//! export PATH="$BIN_DIR:$PATH"
//! ```
//!
//! (or `fish_add_path $BIN_DIR` for fish). `budi uninstall` used to leave
//! those lines behind, which permanently polluted `$PATH` after the user
//! believed they had uninstalled. This module scans the candidate shell
//! profiles, finds the `# Added by budi installer` marker + the matching
//! PATH modification on the next line, and produces a cleaned text suitable
//! for `apply_cleanup()`.
//!
//! Only the marker + one immediately following PATH-modification line are
//! removed. Everything else (including fuzzy PATH edits the user made by
//! hand) is left untouched — consent-first cleanup per ADR-0081.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub const INSTALLER_MARKER: &str = "# Added by budi installer";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallerPlatform {
    Macos,
    Linux,
    Windows,
}

fn current_platform() -> InstallerPlatform {
    #[cfg(target_os = "macos")]
    {
        InstallerPlatform::Macos
    }
    #[cfg(target_os = "windows")]
    {
        return InstallerPlatform::Windows;
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        InstallerPlatform::Linux
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovedLine {
    pub line_number: usize,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellProfileResidue {
    pub path: PathBuf,
    pub original_text: String,
    pub cleaned_text: String,
    pub removed_lines: Vec<RemovedLine>,
}

impl ShellProfileResidue {
    pub fn apply_cleanup(&self) -> Result<bool> {
        if self.removed_lines.is_empty() || self.cleaned_text == self.original_text {
            return Ok(false);
        }
        fs::write(&self.path, &self.cleaned_text)
            .with_context(|| format!("Failed writing {}", self.path.display()))?;
        Ok(true)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstallerResidueScan {
    pub files: Vec<ShellProfileResidue>,
}

impl InstallerResidueScan {
    pub fn has_residue(&self) -> bool {
        !self.files.is_empty()
    }
}

pub fn scan() -> Result<InstallerResidueScan> {
    let home = crate::config::home_dir()?;
    let shell = std::env::var("SHELL").ok();
    Ok(scan_with_env(&home, current_platform(), shell.as_deref()))
}

pub fn scan_with_env(
    home: &Path,
    platform: InstallerPlatform,
    shell: Option<&str>,
) -> InstallerResidueScan {
    let mut files = Vec::new();
    for path in shell_profile_candidates(home, platform, shell) {
        if let Some(residue) = scan_shell_profile(&path) {
            files.push(residue);
        }
    }
    InstallerResidueScan { files }
}

fn scan_shell_profile(path: &Path) -> Option<ShellProfileResidue> {
    if !path.exists() {
        return None;
    }
    let original_text = fs::read_to_string(path).ok()?;
    let (cleaned_text, removed_lines) = strip_installer_blocks(&original_text);
    if removed_lines.is_empty() {
        return None;
    }
    Some(ShellProfileResidue {
        path: path.to_path_buf(),
        original_text,
        cleaned_text,
        removed_lines,
    })
}

/// Remove every `# Added by budi installer` block from `raw`.
///
/// A block is the marker line + the immediately following line when that
/// line looks like a PATH modification (`export PATH=...`, `PATH=...`, or
/// `fish_add_path ...`). We also eat a single blank line immediately
/// preceding the marker so the installer's `printf '\n# Added...'` produces
/// a byte-identical round-trip when the surrounding file does not end with
/// a blank line already.
fn strip_installer_blocks(raw: &str) -> (String, Vec<RemovedLine>) {
    let lines: Vec<&str> = raw.lines().collect();
    let mut skip: HashSet<usize> = HashSet::new();
    let mut removed = Vec::new();

    for (idx, line) in lines.iter().enumerate() {
        if line.trim() != INSTALLER_MARKER {
            continue;
        }
        let next_idx = idx + 1;
        let Some(next_line) = lines.get(next_idx) else {
            continue;
        };
        if !looks_like_path_modification(next_line) {
            continue;
        }

        skip.insert(idx);
        skip.insert(next_idx);
        removed.push(RemovedLine {
            line_number: idx + 1,
            content: line.to_string(),
        });
        removed.push(RemovedLine {
            line_number: next_idx + 1,
            content: next_line.to_string(),
        });

        if idx > 0 && lines[idx - 1].trim().is_empty() && !skip.contains(&(idx - 1)) {
            skip.insert(idx - 1);
        }
    }

    let kept: Vec<&str> = lines
        .iter()
        .enumerate()
        .filter_map(|(i, l)| (!skip.contains(&i)).then_some(*l))
        .collect();

    (rebuild_text(&kept, raw), removed)
}

fn looks_like_path_modification(line: &str) -> bool {
    let trimmed = line.trim_start();
    let without_export = trimmed.strip_prefix("export ").unwrap_or(trimmed);
    let without_export = without_export.trim_start();
    if let Some(rest) = without_export.strip_prefix("PATH") {
        return rest.trim_start().starts_with('=');
    }
    if let Some(rest) = trimmed.strip_prefix("fish_add_path") {
        return rest.starts_with(char::is_whitespace);
    }
    false
}

fn rebuild_text(kept: &[&str], original: &str) -> String {
    if kept.is_empty() && !original.is_empty() {
        // Fully-stripped file: return empty string (no newline).
        return String::new();
    }
    let newline = if original.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut out = kept.join(newline);
    if original.ends_with("\r\n") {
        out.push_str("\r\n");
    } else if original.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn shell_profile_candidates(
    home: &Path,
    platform: InstallerPlatform,
    shell: Option<&str>,
) -> Vec<PathBuf> {
    if platform == InstallerPlatform::Windows {
        return Vec::new();
    }

    let zsh = home.join(".zshrc");
    let bashrc = home.join(".bashrc");
    let bash_profile = home.join(".bash_profile");
    let profile = home.join(".profile");
    let fish = home.join(".config/fish/config.fish");

    let mut candidates = Vec::new();
    if let Some(shell) = shell {
        let lower = shell.to_ascii_lowercase();
        if lower.contains("zsh") {
            candidates.push(zsh.clone());
        }
        if lower.contains("bash") {
            candidates.push(bashrc.clone());
            candidates.push(bash_profile.clone());
            candidates.push(profile.clone());
        }
        if lower.contains("fish") {
            candidates.push(fish.clone());
        }
    }
    candidates.push(zsh);
    candidates.push(bashrc);
    candidates.push(bash_profile);
    candidates.push(profile);
    candidates.push(fish);
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
            "budi-installer-residue-{name}-{}-{stamp}",
            std::process::id()
        ))
    }

    #[test]
    fn strip_removes_marker_plus_path_line_plus_leading_blank() {
        let raw =
            "alpha\nbeta\n\n# Added by budi installer\nexport PATH=\"/home/u/.local/bin:$PATH\"\n";
        let (cleaned, removed) = strip_installer_blocks(raw);
        assert_eq!(cleaned, "alpha\nbeta\n");
        assert_eq!(removed.len(), 2);
        assert_eq!(removed[0].content, "# Added by budi installer");
    }

    #[test]
    fn strip_handles_fish_add_path() {
        let raw =
            "set -x EDITOR vim\n\n# Added by budi installer\nfish_add_path /home/u/.local/bin\n";
        let (cleaned, removed) = strip_installer_blocks(raw);
        assert_eq!(cleaned, "set -x EDITOR vim\n");
        assert_eq!(removed.len(), 2);
    }

    #[test]
    fn strip_leaves_unrelated_path_edits_alone() {
        let raw = "# My own PATH\nexport PATH=\"/opt/tools:$PATH\"\n";
        let (cleaned, removed) = strip_installer_blocks(raw);
        assert_eq!(cleaned, raw);
        assert!(removed.is_empty());
    }

    #[test]
    fn strip_requires_path_modification_on_next_line() {
        // Marker without a matching PATH line is left untouched so we never
        // accidentally eat a comment the user happened to copy-paste.
        let raw = "# Added by budi installer\nunset EDITOR\n";
        let (cleaned, removed) = strip_installer_blocks(raw);
        assert_eq!(cleaned, raw);
        assert!(removed.is_empty());
    }

    #[test]
    fn strip_preserves_crlf_line_endings() {
        let raw = "alpha\r\n\r\n# Added by budi installer\r\nexport PATH=\"/home/u/.local/bin:$PATH\"\r\n";
        let (cleaned, _) = strip_installer_blocks(raw);
        assert_eq!(cleaned, "alpha\r\n");
    }

    #[test]
    fn strip_handles_file_without_trailing_newline() {
        let raw = "alpha\n\n# Added by budi installer\nexport PATH=\"/home/u/.local/bin:$PATH\"";
        let (cleaned, _) = strip_installer_blocks(raw);
        assert_eq!(cleaned, "alpha");
    }

    #[test]
    fn looks_like_path_modification_variants() {
        assert!(looks_like_path_modification(
            "export PATH=\"/home/u/.local/bin:$PATH\""
        ));
        assert!(looks_like_path_modification(
            "  export PATH=\"/home/u/.local/bin:$PATH\""
        ));
        assert!(looks_like_path_modification("PATH=/usr/bin:/bin"));
        assert!(looks_like_path_modification("fish_add_path /opt/budi/bin"));
        assert!(!looks_like_path_modification("echo hello"));
        assert!(!looks_like_path_modification("# PATH= in comment"));
        assert!(!looks_like_path_modification("fish_add_paths bogus"));
    }

    #[test]
    fn apply_cleanup_is_noop_when_file_unchanged() {
        let dir = unique_temp_dir("noop");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".zshrc");
        let text = "alpha\nbeta\n";
        fs::write(&path, text).unwrap();

        let residue = ShellProfileResidue {
            path: path.clone(),
            original_text: text.to_string(),
            cleaned_text: text.to_string(),
            removed_lines: Vec::new(),
        };
        let changed = residue.apply_cleanup().unwrap();
        assert!(!changed);
        assert_eq!(fs::read_to_string(&path).unwrap(), text);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn apply_cleanup_roundtrip_preserves_bytes_when_block_absent() {
        let dir = unique_temp_dir("roundtrip");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".zshrc");
        let text = "alias ll='ls -la'\nexport EDITOR=vim\n";
        fs::write(&path, text).unwrap();

        let scan = scan_with_env(&dir, InstallerPlatform::Linux, Some("/bin/zsh"));
        assert!(scan.files.is_empty(), "no residue should be detected");
        assert_eq!(fs::read_to_string(&path).unwrap(), text);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_finds_and_apply_removes_installer_block() {
        let dir = unique_temp_dir("scan-and-strip");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".zshrc");
        let pre = "alias ll='ls -la'\nexport EDITOR=vim\n";
        let residue_block =
            "\n# Added by budi installer\nexport PATH=\"/home/u/.local/bin:$PATH\"\n";
        let installed = format!("{pre}{residue_block}");
        fs::write(&path, &installed).unwrap();

        let scan = scan_with_env(&dir, InstallerPlatform::Linux, Some("/bin/zsh"));
        assert_eq!(scan.files.len(), 1);
        assert_eq!(scan.files[0].removed_lines.len(), 2);

        assert!(scan.files[0].apply_cleanup().unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), pre);

        // Idempotent — second scan finds nothing.
        let second = scan_with_env(&dir, InstallerPlatform::Linux, Some("/bin/zsh"));
        assert!(second.files.is_empty());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_detects_multiple_blocks_from_repeated_installs() {
        let dir = unique_temp_dir("multi");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".zshrc");
        // A previous smoke test left a stale block pointing at /tmp; the
        // current install added another. Both must be removed.
        let raw = "alpha\n\n# Added by budi installer\nexport PATH=\"/tmp/budi-smoke/fake-prefix:$PATH\"\n\n# Added by budi installer\nexport PATH=\"/home/u/.local/bin:$PATH\"\n";
        fs::write(&path, raw).unwrap();

        let scan = scan_with_env(&dir, InstallerPlatform::Linux, Some("/bin/zsh"));
        assert_eq!(scan.files.len(), 1);
        assert_eq!(scan.files[0].removed_lines.len(), 4);
        assert!(scan.files[0].apply_cleanup().unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), "alpha\n");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn shell_profile_candidates_respects_shell_hint_but_includes_fallbacks() {
        let home = Path::new("/tmp/home");
        let zsh = shell_profile_candidates(home, InstallerPlatform::Macos, Some("/bin/zsh"));
        assert_eq!(zsh[0], home.join(".zshrc"));
        assert!(zsh.contains(&home.join(".bashrc")));
        assert!(zsh.contains(&home.join(".config/fish/config.fish")));

        let fish =
            shell_profile_candidates(home, InstallerPlatform::Linux, Some("/usr/local/bin/fish"));
        assert_eq!(fish[0], home.join(".config/fish/config.fish"));
    }

    #[test]
    fn shell_profile_candidates_empty_on_windows() {
        let home = Path::new("C:/Users/test");
        assert!(shell_profile_candidates(home, InstallerPlatform::Windows, None).is_empty());
    }
}
