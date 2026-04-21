use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use budi_core::config;
use reqwest::blocking::Client;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::daemon::{ensure_daemon_running, ensure_daemon_running_with_binary};

const RELEASES_LATEST_URL: &str = "https://api.github.com/repos/siropkin/budi/releases/latest";
const RELEASE_DOWNLOAD_BASE: &str = "https://github.com/siropkin/budi/releases/download";

struct PreparedStandaloneUpdate {
    temp_dir: PathBuf,
    budi_src: PathBuf,
    daemon_src: PathBuf,
    budi_dst: PathBuf,
    daemon_dst: PathBuf,
}

impl Drop for PreparedStandaloneUpdate {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.temp_dir);
    }
}

struct BackupEntry {
    dst: PathBuf,
    backup: Option<PathBuf>,
}

pub fn cmd_update(yes: bool, version: Option<String>) -> Result<()> {
    let is_brew = is_homebrew_install();

    let current = env!("CARGO_PKG_VERSION");
    println!("Current version: v{}", current);

    let green = super::ansi("\x1b[32m");
    let bold = super::ansi("\x1b[1m");
    let bold_green = super::ansi("\x1b[1;32m");
    let yellow = super::ansi("\x1b[33m");
    let dim = super::ansi("\x1b[90m");
    let reset = super::ansi("\x1b[0m");

    // --version with Homebrew: fall through to standalone assets since
    // brew doesn't support installing arbitrary versions.
    let use_brew = is_brew && version.is_none();

    // Resolve target version — either from --version flag or GitHub API.
    let (latest_tag, latest) = if let Some(ref v) = version {
        let tag = budi_core::update::normalize_release_tag(v)?;
        let ver = budi_core::update::version_from_tag(&tag);
        println!("Target version: v{}", ver);
        (tag, ver)
    } else {
        println!("Checking for updates...");
        let tag = fetch_latest_release_tag()?;
        let ver = budi_core::update::version_from_tag(&tag);
        (tag, ver)
    };

    if latest == current && version.is_none() {
        println!("{green}✓{reset} Already up to date (v{}).", current);
        return Ok(());
    }

    if version.is_some() && latest == current {
        println!("Reinstalling v{}...", current);
    } else {
        println!(
            "New version available: {bold}v{}{reset} → {bold_green}v{}{reset}",
            current, latest
        );
    }

    if !yes {
        let method = if use_brew {
            "Homebrew"
        } else {
            "GitHub release assets"
        };
        println!("This will update budi via {}.", method);
        if std::io::stdin().is_terminal() {
            eprint!("Continue? [y/N] ");
            let mut answer = String::new();
            std::io::stdin()
                .read_line(&mut answer)
                .context("Failed to read stdin")?;
            if !matches!(answer.trim(), "y" | "Y") {
                println!("Aborted.");
                return Ok(());
            }
        } else {
            anyhow::bail!(
                "Non-interactive terminal. Use `budi update --yes` to skip confirmation."
            );
        }
    }

    let prepared = if use_brew {
        None
    } else {
        println!("Preparing update package...");
        Some(prepare_standalone_update(&latest_tag)?)
    };

    let (repo_root, config) = resolve_current_config();

    // Stop daemon right before install, after preflight work succeeds.
    // Required on Windows where running executables cannot be overwritten.
    println!("Stopping daemon...");
    stop_all_daemons();
    thread::sleep(Duration::from_millis(500));

    println!("Updating...");

    let install_result = if use_brew {
        run_brew_upgrade()
    } else {
        let package = prepared
            .as_ref()
            .context("internal error: standalone package was not prepared")?;
        install_standalone_package(package)
    };

    if let Err(e) = install_result {
        eprintln!("Update failed. Attempting to restart daemon with current binaries...");
        let _ = ensure_daemon_running(repo_root.as_deref(), &config);
        return Err(e);
    }

    // Clean up legacy hooks from settings.json
    crate::commands::statusline::remove_legacy_hooks();

    // 8.3.0 (#428) removed the bare-verb aliases `budi migrate` /
    // `budi repair` / `budi import`; sweep the 8.2.x deprecation-nudge
    // marker file so upgrading users don't keep a stale date marker
    // under `$BUDI_HOME`. Best-effort; failures are silent.
    crate::commands::db::remove_db_alias_nudge_marker();

    // Remove stale binaries from the other install source (Homebrew vs standalone)
    crate::commands::init::clean_duplicate_binaries();

    // Run database migration before restarting daemon — migration in a
    // standalone process is fast vs slow inside the daemon's Tokio runtime.
    println!("Running database migration...");
    if let Ok(db_path) = budi_core::analytics::db_path() {
        if db_path.exists() && budi_core::migration::needs_migration_at(&db_path) {
            match budi_core::analytics::open_db_with_migration(&db_path) {
                Ok(_) => println!("{green}✓{reset} Database migrated."),
                Err(e) => println!("{yellow}!{reset} Migration warning: {}", e),
            }
        } else {
            println!("{green}✓{reset} Database up to date.");
        }
    }

    // Refresh opted-in integrations (or currently detected integrations for older installs).
    let (repo_root, config) = resolve_current_config();
    let report = crate::commands::integrations::refresh_enabled_integrations(&config);
    if !report.warnings.is_empty() {
        eprintln!("Integration refresh warnings:");
        for warning in report.warnings {
            eprintln!("  - {warning}");
        }
    }

    // Restart daemon with new version.
    let daemon_override = if is_brew && version.is_some() {
        prepared.as_ref().map(|p| p.daemon_dst.as_path())
    } else {
        None
    };
    println!("Restarting daemon...");
    let _ = ensure_daemon_running_with_binary(repo_root.as_deref(), &config, daemon_override);

    // Verify installed version.
    let verification_bin = prepared.as_ref().map(|p| p.budi_dst.as_path());
    verify_installed_version(verification_bin, &latest, green, yellow, reset);

    println!();
    println!(
        "{dim}Release notes: https://github.com/siropkin/budi/releases/tag/{}{reset}",
        latest_tag
    );
    println!("{dim}Restart Claude Code and Cursor to pick up any changes.{reset}");

    Ok(())
}

fn fetch_latest_release_tag() -> Result<String> {
    let client = Client::builder().timeout(Duration::from_secs(10)).build()?;
    let mut req = client
        .get(RELEASES_LATEST_URL)
        .header("User-Agent", "budi-cli");
    if let Some(token) = github_token() {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let resp = req.send().context("Failed to check for updates")?;

    if !resp.status().is_success() {
        let status = resp.status();
        if status.as_u16() == 403 || status.as_u16() == 429 {
            anyhow::bail!(
                "GitHub API rate limit exceeded ({}). Try again later, or specify a version: budi update --version <tag>",
                status
            );
        }
        anyhow::bail!("GitHub API returned {}", status);
    }

    let release: Value = resp.json()?;
    budi_core::update::parse_and_normalize_release_tag(&release)
}

fn run_brew_upgrade() -> Result<()> {
    let status = Command::new("brew")
        .args(["upgrade", "budi"])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("Failed to run brew upgrade")?;

    if !status.success() {
        anyhow::bail!("brew upgrade exited with {}", status);
    }
    Ok(())
}

fn prepare_standalone_update(tag: &str) -> Result<PreparedStandaloneUpdate> {
    #[cfg(target_os = "linux")]
    {
        if linux_uses_musl() {
            anyhow::bail!(
                "musl libc detected. Prebuilt binaries require glibc. Install from source instead: https://github.com/siropkin/budi#install"
            );
        }
    }

    let target = detect_target_triple()?;
    let is_windows = cfg!(target_os = "windows");
    let archive_ext = if is_windows { "zip" } else { "tar.gz" };
    let asset_name = format!("budi-{tag}-{target}.{archive_ext}");

    let temp_dir = create_temp_dir("budi-update")?;
    let archive_path = temp_dir.join(&asset_name);
    let sums_path = temp_dir.join("SHA256SUMS");

    let client = Client::builder().timeout(Duration::from_secs(20)).build()?;

    let mut asset_req = client
        .get(format!("{RELEASE_DOWNLOAD_BASE}/{tag}/{asset_name}"))
        .header("User-Agent", "budi-cli");
    let mut sums_req = client
        .get(format!("{RELEASE_DOWNLOAD_BASE}/{tag}/SHA256SUMS"))
        .header("User-Agent", "budi-cli");
    if let Some(token) = github_token() {
        asset_req = asset_req.header("Authorization", format!("Bearer {token}"));
        sums_req = sums_req.header("Authorization", format!("Bearer {token}"));
    }

    let archive_bytes = asset_req
        .send()
        .with_context(|| format!("Failed to download release asset {asset_name}"))?
        .error_for_status()
        .with_context(|| {
            format!("Download failed — check that a release asset exists for target {target}")
        })?
        .bytes()
        .context("Failed reading release asset bytes")?;
    fs::write(&archive_path, &archive_bytes)
        .with_context(|| format!("Failed to write {}", archive_path.display()))?;

    let sums_text = sums_req
        .send()
        .context("Failed to download SHA256SUMS")?
        .error_for_status()
        .context("SHA256SUMS is required for secure update verification")?
        .text()
        .context("Failed reading SHA256SUMS")?;
    fs::write(&sums_path, &sums_text)
        .with_context(|| format!("Failed to write {}", sums_path.display()))?;

    let expected = parse_checksum_for_asset(&sums_text, &asset_name)
        .with_context(|| format!("Checksum for {asset_name} not found in SHA256SUMS"))?;
    let actual = sha256_of_file(&archive_path)?;
    if expected != actual {
        anyhow::bail!("Checksum mismatch for {asset_name}");
    }

    let extract_dir = temp_dir.join("extracted");
    fs::create_dir_all(&extract_dir)
        .with_context(|| format!("Failed to create {}", extract_dir.display()))?;

    if is_windows {
        extract_zip_windows(&archive_path, &extract_dir)?;
    } else {
        extract_tar_gz(&archive_path, &extract_dir)?;
    }

    let package_dir = extract_dir.join(format!("budi-{tag}-{target}"));
    if !package_dir.is_dir() {
        anyhow::bail!(
            "Unexpected archive layout in {}",
            archive_path
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or("archive")
        );
    }

    let bin_dir = standalone_bin_dir()?;
    let (budi_name, daemon_name) = if is_windows {
        ("budi.exe", "budi-daemon.exe")
    } else {
        ("budi", "budi-daemon")
    };

    let budi_src = package_dir.join(budi_name);
    let daemon_src = package_dir.join(daemon_name);
    if !budi_src.is_file() {
        anyhow::bail!("Missing binary in release archive: {budi_name}");
    }
    if !daemon_src.is_file() {
        anyhow::bail!("Missing binary in release archive: {daemon_name}");
    }

    Ok(PreparedStandaloneUpdate {
        temp_dir,
        budi_src,
        daemon_src,
        budi_dst: bin_dir.join(budi_name),
        daemon_dst: bin_dir.join(daemon_name),
    })
}

fn install_standalone_package(package: &PreparedStandaloneUpdate) -> Result<()> {
    if let Some(parent) = package.budi_dst.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }

    let targets = [
        (&package.budi_src, &package.budi_dst),
        (&package.daemon_src, &package.daemon_dst),
    ];

    let backup_dir = package.temp_dir.join("backup");
    fs::create_dir_all(&backup_dir)
        .with_context(|| format!("Failed to create {}", backup_dir.display()))?;

    let mut backups: Vec<BackupEntry> = Vec::with_capacity(targets.len());
    for (_, dst) in &targets {
        if dst.exists() {
            let backup = backup_dir.join(
                dst.file_name()
                    .and_then(|f| f.to_str())
                    .map(|name| format!("{name}.bak"))
                    .unwrap_or_else(|| "binary.bak".to_string()),
            );
            fs::copy(dst, &backup)
                .with_context(|| format!("Failed to backup {}", dst.display()))?;
            backups.push(BackupEntry {
                dst: (*dst).to_path_buf(),
                backup: Some(backup),
            });
        } else {
            backups.push(BackupEntry {
                dst: (*dst).to_path_buf(),
                backup: None,
            });
        }
    }

    let pid = std::process::id();
    let mut staged: Vec<(PathBuf, PathBuf)> = Vec::with_capacity(targets.len());
    for (src, dst) in &targets {
        let staged_path = dst.with_file_name(format!(
            ".{}.new.{pid}",
            dst.file_name()
                .and_then(|f| f.to_str())
                .unwrap_or("budi-bin")
        ));
        if let Err(err) = fs::copy(src, &staged_path).with_context(|| {
            format!(
                "Failed to stage {} -> {}",
                src.display(),
                staged_path.display()
            )
        }) {
            cleanup_staged_binaries(&staged);
            return Err(err);
        }
        if let Err(err) = ensure_executable_permissions(&staged_path) {
            let _ = fs::remove_file(&staged_path);
            cleanup_staged_binaries(&staged);
            return Err(err);
        }
        staged.push((staged_path, (*dst).to_path_buf()));
    }

    let apply_result = (|| -> Result<()> {
        for (staged_path, dst) in &staged {
            if dst.exists() {
                fs::remove_file(dst)
                    .with_context(|| format!("Failed to replace {}", dst.display()))?;
            }
            fs::rename(staged_path, dst).with_context(|| {
                format!(
                    "Failed to move staged binary {} -> {}",
                    staged_path.display(),
                    dst.display()
                )
            })?;
        }

        // Verify both binaries after install.
        run_binary_version_check(&package.budi_dst)?;
        run_binary_version_check(&package.daemon_dst)?;
        Ok(())
    })();

    if let Err(err) = apply_result {
        cleanup_staged_binaries(&staged);
        rollback_installed_binaries(&backups);
        return Err(err);
    }

    Ok(())
}

fn cleanup_staged_binaries(staged: &[(PathBuf, PathBuf)]) {
    for (staged_path, _) in staged {
        if staged_path.exists() {
            let _ = fs::remove_file(staged_path);
        }
    }
}

fn rollback_installed_binaries(backups: &[BackupEntry]) {
    for entry in backups {
        if entry.dst.exists() {
            let _ = fs::remove_file(&entry.dst);
        }
        if let Some(ref backup_path) = entry.backup {
            let _ = fs::copy(backup_path, &entry.dst);
            let _ = ensure_executable_permissions(&entry.dst);
        }
    }
}

fn verify_installed_version(
    preferred_binary: Option<&Path>,
    expected: &str,
    green: &str,
    yellow: &str,
    reset: &str,
) {
    let output = match preferred_binary {
        Some(bin) => Command::new(bin).arg("--version").output(),
        None => Command::new("budi").arg("--version").output(),
    };

    match output {
        Ok(output) if output.status.success() => {
            let installed = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let installed_ver = installed.strip_prefix("budi ").unwrap_or(&installed);
            if installed_ver == expected || installed_ver == format!("v{expected}") {
                println!("{green}✓{reset} Updated to v{}.", expected);
            } else {
                println!(
                    "{yellow}!{reset} Expected v{}, but detected version is: {}",
                    expected, installed
                );
            }
        }
        _ => {
            println!(
                "{green}✓{reset} Updated to v{} (could not verify installed version).",
                expected
            );
        }
    }
}

fn resolve_current_config() -> (Option<PathBuf>, config::BudiConfig) {
    let repo_root = std::env::current_dir()
        .ok()
        .and_then(|cwd| config::find_repo_root(&cwd).ok());
    let cfg = match &repo_root {
        Some(root) => config::load_or_default(root).unwrap_or_default(),
        None => config::BudiConfig::default(),
    };
    (repo_root, cfg)
}

fn create_temp_dir(prefix: &str) -> Result<PathBuf> {
    let base = std::env::temp_dir();
    for _ in 0..16 {
        let stamp = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default();
        let dir = base.join(format!("{prefix}-{stamp}"));
        match fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(e).with_context(|| format!("Failed to create {}", dir.display()));
            }
        }
    }
    anyhow::bail!("Failed to allocate a temporary directory for update");
}

fn parse_checksum_for_asset(sums: &str, asset_name: &str) -> Result<String> {
    for line in sums.lines() {
        let mut parts = line.split_whitespace();
        let Some(hash) = parts.next() else {
            continue;
        };
        let Some(file) = parts.next() else {
            continue;
        };
        let file = file.trim_start_matches('*');
        if file == asset_name {
            return Ok(hash.to_ascii_lowercase());
        }
    }
    anyhow::bail!("Checksum entry not found")
}

fn sha256_of_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("Failed to read {}", path.display()))?;
    let digest = Sha256::digest(bytes);
    Ok(format!("{digest:x}"))
}

fn extract_tar_gz(archive_path: &Path, extract_dir: &Path) -> Result<()> {
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(archive_path)
        .arg("-C")
        .arg(extract_dir)
        .status()
        .context("Failed to run tar for archive extraction")?;
    if !status.success() {
        anyhow::bail!("tar extraction failed with {}", status);
    }
    Ok(())
}

fn extract_zip_windows(archive_path: &Path, extract_dir: &Path) -> Result<()> {
    let status = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Expand-Archive -LiteralPath $args[0] -DestinationPath $args[1] -Force",
        ])
        .arg(archive_path)
        .arg(extract_dir)
        .status()
        .context("Failed to run PowerShell archive extraction")?;
    if !status.success() {
        anyhow::bail!("Archive extraction failed with {}", status);
    }
    Ok(())
}

fn run_binary_version_check(bin: &Path) -> Result<()> {
    let output = Command::new(bin)
        .arg("--version")
        .output()
        .with_context(|| format!("Failed to run {} --version", bin.display()))?;
    if !output.status.success() {
        anyhow::bail!("Installed binary failed to run: {}", bin.display());
    }
    Ok(())
}

fn ensure_executable_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms)?;
    }
    Ok(())
}

fn detect_target_triple() -> Result<String> {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => anyhow::bail!("Unsupported architecture: {other}"),
    };

    let target = match std::env::consts::OS {
        "linux" => format!("{arch}-unknown-linux-gnu"),
        "macos" => format!("{arch}-apple-darwin"),
        "windows" => format!("{arch}-pc-windows-msvc"),
        other => anyhow::bail!("Unsupported OS: {other}"),
    };

    Ok(target)
}

#[cfg(target_os = "linux")]
fn linux_uses_musl() -> bool {
    if Path::new("/etc/alpine-release").exists() {
        return true;
    }

    let Ok(output) = Command::new("ldd").arg("--version").output() else {
        return false;
    };
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    text.to_ascii_lowercase().contains("musl")
}

fn standalone_bin_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("BIN_DIR") {
        let trimmed = dir.trim();
        if !trimmed.is_empty() {
            return Ok(PathBuf::from(trimmed));
        }
    }

    #[cfg(target_os = "windows")]
    {
        if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
            return Ok(PathBuf::from(local_app_data).join("budi").join("bin"));
        }
        anyhow::bail!("LOCALAPPDATA is not set; cannot determine install directory");
    }

    #[cfg(not(target_os = "windows"))]
    {
        let home = config::home_dir()?;
        Ok(home.join(".local").join("bin"))
    }
}

/// Try to find a GitHub token from env vars or `gh auth token`.
fn github_token() -> Option<String> {
    std::env::var("GITHUB_TOKEN")
        .or_else(|_| std::env::var("GH_TOKEN"))
        .ok()
        .or_else(|| {
            Command::new("gh")
                .args(["auth", "token"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|t| !t.is_empty())
        })
}

/// Check if budi was installed via Homebrew by examining the executable path.
fn is_homebrew_install() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok())
        .map(|p| {
            let s = p.to_string_lossy().to_lowercase();
            s.contains("/cellar/") || s.contains("/homebrew/")
        })
        .unwrap_or(false)
}

/// Stop all budi-daemon processes using platform-appropriate methods.
fn stop_all_daemons() {
    if cfg!(target_os = "windows") {
        let _ = Command::new("taskkill")
            .args(["/F", "/IM", "budi-daemon.exe"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    } else {
        let _ = Command::new("pkill").args(["-f", "budi-daemon"]).status();
    }
}

#[cfg(test)]
mod tests {
    use super::parse_checksum_for_asset;

    #[test]
    fn parse_checksum_finds_plain_entry() {
        let sums = "abc123  budi-v1.2.3-x86_64-unknown-linux-gnu.tar.gz\n";
        let checksum =
            parse_checksum_for_asset(sums, "budi-v1.2.3-x86_64-unknown-linux-gnu.tar.gz")
                .expect("checksum");
        assert_eq!(checksum, "abc123");
    }

    #[test]
    fn parse_checksum_finds_star_entry() {
        let sums = "def456 *budi-v1.2.3-x86_64-unknown-linux-gnu.tar.gz\n";
        let checksum =
            parse_checksum_for_asset(sums, "budi-v1.2.3-x86_64-unknown-linux-gnu.tar.gz")
                .expect("checksum");
        assert_eq!(checksum, "def456");
    }

    #[test]
    fn parse_checksum_returns_error_when_missing() {
        let sums = "abc123  other-file.tar.gz\n";
        assert!(
            parse_checksum_for_asset(sums, "budi-v1.2.3-x86_64-unknown-linux-gnu.tar.gz").is_err()
        );
    }
}
