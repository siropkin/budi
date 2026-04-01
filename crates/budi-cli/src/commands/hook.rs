//! `budi hook` — read hook JSON from stdin and POST it to the daemon.
//! Exits 0 quickly so the editor is not blocked; delivery failures are appended to
//! `<budi-home>/hook-debug.log` (used by `budi doctor`).

use std::io::Read;

use budi_core::config;

pub fn cmd_hook() -> anyhow::Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    if input.trim().is_empty() {
        return Ok(());
    }

    if let Ok(json) = serde_json::from_str::<serde_json::Value>(input.trim()) {
        update_cursor_session_state(&json);
    }

    let base_url = load_daemon_url();
    let url = format!("{base_url}/hooks/ingest");

    // Short timeout; failures are recorded below (not printed to stdout).
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build();
    let result = match client {
        Ok(client) => client
            .post(&url)
            .header("Content-Type", "application/json")
            .body(input)
            .send()
            .map(|_| ())
            .map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    };

    // Always log hook delivery failures to <budi-home>/hook-debug.log.
    // This file is checked by `budi doctor` and helps diagnose hook issues.
    if let Err(ref err) = result
        && let Ok(log_dir) = config::budi_home_dir()
    {
        let log_path = log_dir.join("hook-debug.log");
        let ts = chrono::Utc::now().to_rfc3339();
        let line = format!("[{ts}] hook POST to {url} failed: {err}\n");
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
        rotate_hook_log(&log_path);
    }

    Ok(())
}

/// Keep only the last 100 lines when the log exceeds 50 KB.
fn rotate_hook_log(path: &std::path::Path) {
    const MAX_BYTES: u64 = 50_000;
    const KEEP_LINES: usize = 100;
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() <= MAX_BYTES {
        return;
    }
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    let lines: Vec<&str> = content.lines().collect();
    let kept = &lines[lines.len().saturating_sub(KEEP_LINES)..];
    let _ = std::fs::write(path, kept.join("\n") + "\n");
}

/// Load daemon URL from config, falling back to defaults.
fn load_daemon_url() -> String {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| config::find_repo_root(&cwd).ok())
        .and_then(|root| config::load_or_default(&root).ok())
        .unwrap_or_default()
        .daemon_base_url()
}

// ---------------------------------------------------------------------------
// Cursor session state for the VS Code extension
// ---------------------------------------------------------------------------

/// Persist lightweight session state so the Cursor extension can resolve
/// the active session_id for the current workspace without querying the daemon.
/// File: `<budi-home>/cursor-sessions.json`
fn update_cursor_session_state(json: &serde_json::Value) {
    if json.get("cursor_version").is_none() {
        return;
    }

    let event = json
        .get("hook_event_name")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let session_id = json
        .get("conversation_id")
        .or_else(|| json.get("session_id"))
        .and_then(|v| v.as_str());

    let Some(session_id) = session_id else {
        return;
    };

    let workspace = json
        .get("workspace_roots")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .unwrap_or("");

    match event {
        "sessionStart" => {
            let composer_mode = json.get("composer_mode").and_then(|v| v.as_str());
            session_state_upsert(session_id, workspace, composer_mode, true);
        }
        "sessionEnd" => {
            session_state_mark_inactive(session_id);
        }
        _ => {
            session_state_touch(session_id, workspace);
        }
    }
}

fn session_state_path() -> Option<std::path::PathBuf> {
    config::budi_home_dir()
        .ok()
        .map(|d| d.join("cursor-sessions.json"))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CursorSessionState {
    sessions: Vec<CursorSessionEntry>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CursorSessionEntry {
    session_id: String,
    workspace_path: String,
    started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    composer_mode: Option<String>,
    active: bool,
    /// Updated on every hook event so the extension can pick the most recently used session.
    #[serde(skip_serializing_if = "Option::is_none")]
    last_active_at: Option<String>,
}

fn read_session_state() -> CursorSessionState {
    let Some(path) = session_state_path() else {
        return CursorSessionState { sessions: vec![] };
    };
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or(CursorSessionState { sessions: vec![] })
}

fn write_session_state(state: &CursorSessionState) {
    let Some(path) = session_state_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(
        &path,
        serde_json::to_string_pretty(state).unwrap_or_default(),
    );
}

fn session_state_upsert(
    session_id: &str,
    workspace: &str,
    composer_mode: Option<&str>,
    active: bool,
) {
    let mut state = read_session_state();
    let now = chrono::Utc::now().to_rfc3339();

    if let Some(entry) = state
        .sessions
        .iter_mut()
        .find(|s| s.session_id == session_id)
    {
        entry.active = active;
        entry.started_at = now.clone();
        entry.last_active_at = Some(now);
        if !workspace.is_empty() {
            entry.workspace_path = workspace.to_string();
        }
        if let Some(mode) = composer_mode {
            entry.composer_mode = Some(mode.to_string());
        }
    } else {
        state.sessions.push(CursorSessionEntry {
            session_id: session_id.to_string(),
            workspace_path: workspace.to_string(),
            started_at: now.clone(),
            composer_mode: composer_mode.map(|s| s.to_string()),
            active,
            last_active_at: Some(now),
        });
    }

    prune_old_sessions(&mut state);
    write_session_state(&state);
}

/// Bump `last_active_at` for any hook event so the extension tracks the focused session.
fn session_state_touch(session_id: &str, workspace: &str) {
    let mut state = read_session_state();
    let now = chrono::Utc::now().to_rfc3339();

    if let Some(entry) = state
        .sessions
        .iter_mut()
        .find(|s| s.session_id == session_id)
    {
        entry.last_active_at = Some(now);
        entry.active = true;
        if !workspace.is_empty() && entry.workspace_path.is_empty() {
            entry.workspace_path = workspace.to_string();
        }
    } else {
        state.sessions.push(CursorSessionEntry {
            session_id: session_id.to_string(),
            workspace_path: workspace.to_string(),
            started_at: now.clone(),
            composer_mode: None,
            active: true,
            last_active_at: Some(now),
        });
    }

    prune_old_sessions(&mut state);
    write_session_state(&state);
}

fn session_state_mark_inactive(session_id: &str) {
    let mut state = read_session_state();
    if let Some(entry) = state
        .sessions
        .iter_mut()
        .find(|s| s.session_id == session_id)
    {
        entry.active = false;
    }
    prune_old_sessions(&mut state);
    write_session_state(&state);
}

fn prune_old_sessions(state: &mut CursorSessionState) {
    let cutoff = chrono::Utc::now() - chrono::Duration::days(7);
    state.sessions.retain(|s| {
        s.active
            || chrono::DateTime::parse_from_rfc3339(&s.started_at)
                .map(|dt| dt > cutoff)
                .unwrap_or(false)
    });
}
