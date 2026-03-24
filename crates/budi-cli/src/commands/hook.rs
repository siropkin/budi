use std::io::{self, Read};

use anyhow::Result;
use budi_core::config;
use budi_core::hooks::{PostToolUseInput, UserPromptSubmitInput, UserPromptSubmitOutput};

use crate::daemon::{ensure_daemon_running, fetch_session_stats};

pub fn cmd_hook_user_prompt_submit() -> Result<()> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let _parsed: UserPromptSubmitInput = match serde_json::from_str(&input) {
        Ok(v) => v,
        Err(_) => {
            emit_hook_response(UserPromptSubmitOutput::allow_with_context(String::new()))?;
            return Ok(());
        }
    };

    emit_hook_response(UserPromptSubmitOutput::allow_with_context(String::new()))
}

pub fn cmd_hook_post_tool_use() -> Result<()> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let _parsed: PostToolUseInput = match serde_json::from_str(&input) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
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
            eprintln!("budi: {} prompts tracked", queries);
        }
    }
    Ok(())
}

fn emit_hook_response(output: UserPromptSubmitOutput) -> Result<()> {
    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}
