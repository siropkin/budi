use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use budi_core::config::{self, BudiConfig};
use budi_core::hooks::{PostToolUseInput, UserPromptSubmitInput, UserPromptSubmitOutput};
use serde_json::{Value, json};

use crate::daemon::{ensure_daemon_running, fetch_session_stats};
use crate::HOOK_LOG_LOCK_TIMEOUT_MS;
use crate::HOOK_LOG_LOCK_STALE_SECS;

pub fn cmd_hook_user_prompt_submit() -> Result<()> {
    let hook_started = Instant::now();
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let parsed: UserPromptSubmitInput = match serde_json::from_str(&input) {
        Ok(v) => v,
        Err(_) => {
            emit_hook_response(UserPromptSubmitOutput::allow_with_context(String::new()))?;
            return Ok(());
        }
    };

    let cwd = PathBuf::from(&parsed.common.cwd);
    let session_id = parsed.common.session_id.clone();
    let repo_root = match config::find_repo_root(&cwd) {
        Ok(path) => path,
        Err(_) => {
            emit_hook_response(UserPromptSubmitOutput::allow_with_context(String::new()))?;
            return Ok(());
        }
    };
    let config = config::load_or_default(&repo_root)?;

    log_hook_event(&repo_root, &config, || {
        json!({
            "event": "UserPromptSubmit",
            "phase": "input",
            "ts_unix_ms": now_unix_ms(),
            "session_id": session_id.clone(),
            "cwd": parsed.common.cwd,
            "prompt_chars": parsed.prompt.len(),
        })
    });

    // Record the prompt in daemon stats (via HTTP hook if daemon is running).
    // In v4 we no longer inject context — just track analytics.
    log_hook_event(&repo_root, &config, || {
        json!({
            "event": "UserPromptSubmit",
            "phase": "output",
            "ts_unix_ms": now_unix_ms(),
            "session_id": session_id.clone(),
            "latency_ms": hook_started.elapsed().as_millis(),
            "success": true,
            "context_chars": 0,
        })
    });

    emit_hook_response(UserPromptSubmitOutput::allow_with_context(String::new()))
}

pub fn cmd_hook_post_tool_use() -> Result<()> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let parsed: PostToolUseInput = match serde_json::from_str(&input) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };

    let cwd = PathBuf::from(&parsed.common.cwd);
    let Ok(repo_root) = config::find_repo_root(&cwd) else {
        return Ok(());
    };
    let Ok(config) = config::load_or_default(&repo_root) else {
        return Ok(());
    };

    log_hook_event(&repo_root, &config, || {
        json!({
            "event": "PostToolUse",
            "phase": "input",
            "ts_unix_ms": now_unix_ms(),
            "session_id": parsed.common.session_id.clone(),
            "tool_name": parsed.tool_name,
        })
    });
    Ok(())
}

pub fn cmd_hook_session_start() -> Result<()> {
    let mut input = String::new();
    let _ = io::stdin().read_to_string(&mut input);

    let cwd = std::env::current_dir()?;
    let Ok(repo_root) = config::find_repo_root(&cwd) else {
        return Ok(());
    };
    let config = config::load_or_default(&repo_root)?;
    let _ = ensure_daemon_running(&repo_root, &config);
    Ok(())
}

pub fn cmd_hook_subagent_start() -> Result<()> {
    let mut input = String::new();
    let _ = io::stdin().read_to_string(&mut input);
    // No project map injection in v4 — analytics only.
    Ok(())
}

pub fn cmd_hook_session_end() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let Ok(repo_root) = config::find_repo_root(&cwd) else {
        return Ok(());
    };
    let Ok(config) = config::load_or_default(&repo_root) else {
        return Ok(());
    };

    let session_id = std::env::var("CLAUDE_SESSION_ID").ok();

    if let Some(ref sid) = session_id
        && let Some(stats) = fetch_session_stats(&config, sid)
    {
        let queries = stats.get("queries").and_then(|v| v.as_u64()).unwrap_or(0);
        if queries > 0 {
            let skips = stats.get("skips").and_then(|v| v.as_u64()).unwrap_or(0);
            eprintln!("budi: {} prompts tracked, {} skipped", queries, skips);
        }
    }
    Ok(())
}

fn emit_hook_response(output: UserPromptSubmitOutput) -> Result<()> {
    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}

// ─── Hook Logging ────────────────────────────────────────────────────────────

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[derive(Debug)]
struct HookLogLockGuard {
    lock_path: PathBuf,
    _lock_file: fs::File,
}

impl Drop for HookLogLockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.lock_path);
    }
}

fn hook_log_lock_path(log_path: &Path) -> PathBuf {
    let lock_name = log_path
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| format!("{name}.lock"))
        .unwrap_or_else(|| "hook-io.jsonl.lock".to_string());
    log_path.with_file_name(lock_name)
}

fn clear_stale_hook_log_lock(lock_path: &Path) {
    let Ok(metadata) = fs::metadata(lock_path) else {
        return;
    };
    let Ok(modified) = metadata.modified() else {
        return;
    };
    let Ok(age) = SystemTime::now().duration_since(modified) else {
        return;
    };
    if age > Duration::from_secs(HOOK_LOG_LOCK_STALE_SECS) {
        let _ = fs::remove_file(lock_path);
    }
}

fn acquire_hook_log_lock(log_path: &Path) -> Option<HookLogLockGuard> {
    let lock_path = hook_log_lock_path(log_path);
    let started = Instant::now();
    loop {
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&lock_path)
        {
            Ok(lock_file) => {
                return Some(HookLogLockGuard {
                    lock_path,
                    _lock_file: lock_file,
                });
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                clear_stale_hook_log_lock(&lock_path);
                if started.elapsed() >= Duration::from_millis(HOOK_LOG_LOCK_TIMEOUT_MS) {
                    return None;
                }
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return None,
        }
    }
}

fn log_hook_event<F>(repo_root: &Path, config: &BudiConfig, build_value: F)
where
    F: FnOnce() -> Value,
{
    if !config.debug_io {
        return;
    }
    let Ok(log_path) = config::hook_log_path(repo_root) else {
        return;
    };
    if let Some(parent) = log_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let Some(_lock_guard) = acquire_hook_log_lock(&log_path) else {
        return;
    };
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&log_path) {
        let mut line = build_value();
        if let Some(obj) = line.as_object_mut() {
            obj.insert(
                "repo_root".to_string(),
                json!(repo_root.display().to_string()),
            );
        }
        if let Ok(mut serialized) = serde_json::to_vec(&line) {
            serialized.push(b'\n');
            let _ = file.write_all(&serialized);
        }
    }
}
