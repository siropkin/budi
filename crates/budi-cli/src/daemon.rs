use std::fs::{self, OpenOptions};
use std::io;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use budi_core::config::{self, BudiConfig};
use reqwest::blocking::Client;

use crate::HEALTH_TIMEOUT_SECS;

pub fn daemon_client_with_timeout(timeout: Duration) -> Result<Client> {
    Client::builder()
        .timeout(timeout)
        .build()
        .context("Failed to construct HTTP client")
}

pub fn daemon_health(config: &BudiConfig) -> bool {
    daemon_health_with_timeout(config, Duration::from_secs(HEALTH_TIMEOUT_SECS))
}

pub fn daemon_health_with_timeout(config: &BudiConfig, timeout: Duration) -> bool {
    let Ok(client) = daemon_client_with_timeout(timeout) else {
        return false;
    };
    let url = format!("{}/health", config.daemon_base_url());
    client
        .get(url)
        .send()
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

/// Read the startup timeout from BUDI_STARTUP_TIMEOUT_SECS env var, default to 52s.
/// This controls how long we wait for the daemon to become healthy after spawning.
fn startup_timeout_retries() -> usize {
    std::env::var("BUDI_STARTUP_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(|secs| {
            // Each retry: ~500ms request timeout + ~150ms sleep = ~650ms per iteration
            // Convert seconds to approximate retry count
            (secs * 1000 / 650).max(1) as usize
        })
        .unwrap_or(80) // default: 80 retries * ~650ms = ~52s
}

pub fn ensure_daemon_running(repo_root: Option<&Path>, config: &BudiConfig) -> Result<()> {
    ensure_daemon_running_with_binary(repo_root, config, None)
}

pub fn ensure_daemon_running_with_binary(
    repo_root: Option<&Path>,
    config: &BudiConfig,
    daemon_bin_override: Option<&Path>,
) -> Result<()> {
    if daemon_health(config) {
        // Daemon is running — but check if it's the same version as this CLI.
        // A version mismatch (e.g. after `brew upgrade`) means the old daemon
        // has old code (migrations, endpoints) and must be replaced.
        if !daemon_version_matches(config) {
            eprintln!("budi: restarting daemon (version mismatch)");
            tracing::info!("Daemon version mismatch — restarting with current binary");
            kill_all_daemons();
            if !wait_for_port_release(config, 40, Duration::from_millis(150)) {
                force_kill_all_daemons();
                let _ = wait_for_port_release(config, 20, Duration::from_millis(150));
            }
        } else {
            return Ok(());
        }
    }

    if daemon_port_is_listening(config) {
        if wait_for_daemon_health(
            config,
            24,
            Duration::from_millis(250),
            Duration::from_millis(250),
        ) {
            return Ok(());
        }
        if restart_unhealthy_daemon_listener(repo_root, config, daemon_bin_override)? {
            return Ok(());
        }
        let log_excerpt = daemon_log_tail(repo_root);
        anyhow::bail!(
            "Daemon port is occupied but health endpoint is unavailable at {}.\n\
             {}{log_excerpt}",
            config.daemon_base_url(),
            daemon_port_conflict_hint(config.daemon_port),
        );
    }

    spawn_daemon_process(repo_root, config, daemon_bin_override)?;
    let retries = startup_timeout_retries();
    if wait_for_daemon_health(
        config,
        retries,
        Duration::from_millis(500),
        Duration::from_millis(150),
    ) {
        return Ok(());
    }
    let log_excerpt = daemon_log_tail(repo_root);
    let conflict_hint = if log_excerpt.contains("Address already in use") {
        format!("\n{}", daemon_port_conflict_hint(config.daemon_port))
    } else {
        String::new()
    };
    anyhow::bail!(
        "Daemon failed to become healthy at {}.{conflict_hint}{log_excerpt}",
        config.daemon_base_url(),
    );
}

fn daemon_port_conflict_hint(port: u16) -> String {
    if cfg!(target_os = "windows") {
        format!(
            "Another process may be using port {port}. \
             Check listeners with `Get-NetTCPConnection -LocalPort {port} -State Listen`, \
             stop stale daemon processes with `taskkill /IM budi-daemon.exe /F`, and rerun `budi init`.\n"
        )
    } else {
        format!(
            "Another process may be using port {port}. \
             Check listeners with `lsof -i :{port}`, \
             stop stale daemon processes with `pkill -f \"budi-daemon\"`, and rerun `budi init`.\n"
        )
    }
}

/// Check if the running daemon reports the same version as this CLI binary.
fn daemon_version_matches(config: &BudiConfig) -> bool {
    let cli_version = env!("CARGO_PKG_VERSION");
    let Ok(client) = daemon_client_with_timeout(Duration::from_secs(HEALTH_TIMEOUT_SECS)) else {
        return false;
    };
    let url = format!("{}/health", config.daemon_base_url());
    let Ok(resp) = client.get(url).send() else {
        return false;
    };
    let Ok(json) = resp.json::<serde_json::Value>() else {
        return false;
    };
    match json.get("version").and_then(|v| v.as_str()) {
        Some(daemon_version) => daemon_version == cli_version,
        // Old daemons don't report version — treat as mismatch
        None => false,
    }
}

fn wait_for_daemon_health(
    config: &BudiConfig,
    retries: usize,
    request_timeout: Duration,
    sleep_interval: Duration,
) -> bool {
    for attempt in 0..retries {
        if daemon_health_with_timeout(config, request_timeout) {
            return true;
        }
        if attempt + 1 < retries {
            thread::sleep(sleep_interval);
        }
    }
    false
}

fn restart_unhealthy_daemon_listener(
    repo_root: Option<&Path>,
    config: &BudiConfig,
    daemon_bin_override: Option<&Path>,
) -> Result<bool> {
    let listener_pids = daemon_listener_pids(config.daemon_port)?;
    if listener_pids.is_empty() {
        return Ok(false);
    }
    let mut killed_any = false;
    for pid in listener_pids {
        let Some(command_line) = daemon_process_command(pid) else {
            continue;
        };
        if !is_budi_daemon_command_for_port(&command_line, config.daemon_port) {
            continue;
        }
        if kill_process(pid, "-TERM")? {
            killed_any = true;
        }
    }
    if !killed_any {
        return Ok(false);
    }
    if !wait_for_port_release(config, 30, Duration::from_millis(120)) {
        for pid in daemon_listener_pids(config.daemon_port)? {
            let Some(command_line) = daemon_process_command(pid) else {
                continue;
            };
            if is_budi_daemon_command_for_port(&command_line, config.daemon_port) {
                let _ = kill_process(pid, "-KILL");
            }
        }
    }
    if daemon_port_is_listening(config) {
        return Ok(false);
    }
    spawn_daemon_process(repo_root, config, daemon_bin_override)?;
    Ok(wait_for_daemon_health(
        config,
        startup_timeout_retries(),
        Duration::from_millis(500),
        Duration::from_millis(150),
    ))
}

fn wait_for_port_release(config: &BudiConfig, retries: usize, sleep_interval: Duration) -> bool {
    for attempt in 0..retries {
        if !daemon_port_is_listening(config) {
            return true;
        }
        if attempt + 1 < retries {
            thread::sleep(sleep_interval);
        }
    }
    !daemon_port_is_listening(config)
}

fn daemon_listener_pids(port: u16) -> Result<Vec<u32>> {
    if cfg!(target_os = "windows") {
        let output = match Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!(
                    "(Get-NetTCPConnection -LocalPort {port} -State Listen -ErrorAction SilentlyContinue).OwningProcess"
                ),
            ])
            .output()
        {
            Ok(output) => output,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err).context("Failed to inspect listener pids"),
        };
        if !output.status.success() {
            return Ok(Vec::new());
        }
        return Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.trim().parse::<u32>().ok())
            .collect());
    }

    let output = match Command::new("lsof")
        .arg("-nP")
        .arg(format!("-tiTCP:{port}"))
        .arg("-sTCP:LISTEN")
        .output()
    {
        Ok(output) => output,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).context("Failed to inspect listener pids via lsof"),
    };
    if !output.status.success() {
        return Ok(Vec::new());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .collect())
}

fn daemon_process_command(pid: u32) -> Option<String> {
    let output = if cfg!(target_os = "windows") {
        Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!("(Get-CimInstance Win32_Process -Filter \"ProcessId={pid}\").CommandLine"),
            ])
            .output()
            .ok()?
    } else {
        Command::new("ps")
            .arg("-p")
            .arg(pid.to_string())
            .arg("-o")
            .arg("command=")
            .output()
            .ok()?
    };
    if !output.status.success() {
        return None;
    }
    let command = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if command.is_empty() {
        None
    } else {
        Some(command)
    }
}

pub fn is_budi_daemon_command_for_port(command: &str, port: u16) -> bool {
    let spaced = format!("--port {port}");
    let inline = format!("--port={port}");
    command.contains("budi-daemon")
        && command.contains("serve")
        && (command.contains(&spaced) || command.contains(&inline))
}

fn kill_process(pid: u32, signal: &str) -> Result<bool> {
    if cfg!(target_os = "windows") {
        let status = match Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        {
            Ok(status) => status,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(err) => {
                return Err(err).with_context(|| format!("Failed to kill pid {pid} via taskkill"));
            }
        };
        return Ok(status.success());
    }

    let status = match Command::new("kill")
        .arg(signal)
        .arg(pid.to_string())
        .status()
    {
        Ok(status) => status,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => {
            return Err(err).with_context(|| format!("Failed to send {signal} to pid {pid}"));
        }
    };
    Ok(status.success())
}

/// Kill all budi-daemon processes using platform-appropriate methods (SIGTERM / taskkill).
fn kill_all_daemons() {
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

/// Force-kill all budi-daemon processes (SIGKILL on Unix, taskkill /F on Windows).
fn force_kill_all_daemons() {
    if cfg!(target_os = "windows") {
        let _ = Command::new("taskkill")
            .args(["/F", "/IM", "budi-daemon.exe"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    } else {
        let _ = Command::new("pkill")
            .args(["-9", "-f", "budi-daemon"])
            .status();
    }
}

fn daemon_port_is_listening(config: &BudiConfig) -> bool {
    let endpoint = format!("{}:{}", config.daemon_host, config.daemon_port);
    let Ok(addrs) = endpoint.to_socket_addrs() else {
        return false;
    };
    for addr in addrs {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(250)).is_ok() {
            return true;
        }
    }
    false
}

/// Resolve the daemon log path — use repo-specific path if available, otherwise global.
fn daemon_log_path(repo_root: Option<&Path>) -> Option<PathBuf> {
    if let Some(root) = repo_root {
        config::daemon_log_path(root).ok()
    } else {
        config::budi_home_dir()
            .ok()
            .map(|home| home.join("logs").join("daemon.log"))
    }
}

/// Read the last ~10 lines of daemon.log and format them for inclusion in error messages.
fn daemon_log_tail(repo_root: Option<&Path>) -> String {
    let Some(log_path) = daemon_log_path(repo_root) else {
        return String::new();
    };
    let content = match fs::read_to_string(&log_path) {
        Ok(c) => c,
        Err(_) => return format!("\nCheck daemon log: {}", log_path.display()),
    };
    let lines: Vec<&str> = content.lines().collect();
    let tail: Vec<&str> = if lines.len() > 10 {
        lines[lines.len() - 10..].to_vec()
    } else {
        lines
    };
    if tail.is_empty() {
        return format!("\nDaemon log is empty: {}", log_path.display());
    }
    format!(
        "\nDaemon log ({}):\n{}",
        log_path.display(),
        tail.join("\n")
    )
}

fn spawn_daemon_process(
    repo_root: Option<&Path>,
    config: &BudiConfig,
    daemon_bin_override: Option<&Path>,
) -> Result<()> {
    let daemon_bin = match daemon_bin_override {
        Some(path) => path.to_path_buf(),
        None => resolve_daemon_binary()?,
    };
    let log_path = daemon_log_path(repo_root).context("Could not determine daemon log path")?;
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("Failed opening {}", log_path.display()))?;
    let stderr = stdout.try_clone()?;
    Command::new(daemon_bin)
        .arg("serve")
        .arg("--host")
        .arg(&config.daemon_host)
        .arg("--port")
        .arg(config.daemon_port.to_string())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .stdin(Stdio::null())
        .spawn()
        .with_context(|| "Failed to spawn budi-daemon process".to_string())?;
    Ok(())
}

pub(crate) fn resolve_daemon_binary() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("BUDI_DAEMON_BIN") {
        return Ok(PathBuf::from(path));
    }
    let current = std::env::current_exe().context("Failed to resolve current executable")?;
    Ok(resolve_daemon_binary_from(None, Some(&current)))
}

fn resolve_daemon_binary_from(
    env_override: Option<PathBuf>,
    current_exe: Option<&Path>,
) -> PathBuf {
    if let Some(path) = env_override {
        return path;
    }
    if let Some(current) = current_exe
        && let Some(parent) = current.parent()
    {
        let sibling = parent.join(daemon_binary_name());
        if sibling.exists() {
            return sibling;
        }
    }
    PathBuf::from(daemon_binary_name())
}

fn daemon_binary_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "budi-daemon.exe"
    } else {
        "budi-daemon"
    }
}

#[cfg(test)]
mod tests {
    use super::{daemon_binary_name, resolve_daemon_binary_from};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be monotonic")
            .as_nanos();
        std::env::temp_dir().join(format!("budi-cli-{prefix}-{}-{nanos}", std::process::id()))
    }

    #[test]
    fn resolve_daemon_binary_uses_env_override_first() {
        let override_path = PathBuf::from("/tmp/custom-budi-daemon");
        let resolved = resolve_daemon_binary_from(
            Some(override_path.clone()),
            Some(Path::new("/ignored/budi")),
        );
        assert_eq!(resolved, override_path);
    }

    #[test]
    fn resolve_daemon_binary_prefers_sibling_binary_when_present() {
        let test_dir = unique_temp_dir("daemon-sibling");
        fs::create_dir_all(&test_dir).expect("create test dir");

        let current_exe = test_dir.join("budi");
        let sibling_daemon = test_dir.join(daemon_binary_name());
        fs::write(&sibling_daemon, b"binary").expect("create sibling daemon");

        let resolved = resolve_daemon_binary_from(None, Some(&current_exe));
        assert_eq!(resolved, sibling_daemon);

        fs::remove_dir_all(&test_dir).expect("cleanup test dir");
    }

    #[test]
    fn resolve_daemon_binary_falls_back_to_path_binary_name() {
        let resolved =
            resolve_daemon_binary_from(None, Some(Path::new("/tmp/no-sibling-dir/budi")));
        assert_eq!(resolved, PathBuf::from(daemon_binary_name()));
    }
}
