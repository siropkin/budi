//! Session health: vitals computation, tips, and batch health checks.
//!
//! Four vitals are computed per session:
//! - **context_drag** — input token growth over the session
//! - **cache_efficiency** — cache hit rate
//! - **thrashing** — rapid-fire tool sequences from hook_events
//! - **cost_acceleration** — dominant-model cost ratio (2nd half vs 1st half)

use anyhow::{Context, Result};
use rusqlite::{Connection, params};

use super::MessageRow;
use super::sessions::session_messages;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionHealth {
    pub state: String,
    pub message_count: u64,
    pub total_cost_cents: f64,
    pub vitals: SessionVitals,
    pub tip: String,
    pub details: Vec<HealthDetail>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionVitals {
    pub context_drag: Option<VitalScore>,
    pub cache_efficiency: Option<VitalScore>,
    pub thrashing: Option<VitalScore>,
    pub cost_acceleration: Option<VitalScore>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VitalScore {
    pub state: String,
    pub label: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HealthDetail {
    pub vital: String,
    pub state: String,
    pub label: String,
    pub tip: String,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Compute session health for a single session.
/// If `session_id` is None, uses the most recent session.
pub fn session_health(conn: &Connection, session_id: Option<&str>) -> Result<SessionHealth> {
    let sid = match session_id {
        Some(s) => s.to_string(),
        None => conn
            .query_row(
                "SELECT session_id FROM sessions ORDER BY started_at DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .context("No sessions found")?,
    };

    let provider: String = conn
        .query_row(
            "SELECT COALESCE(provider, 'claude_code') FROM sessions WHERE session_id = ?1",
            params![sid],
            |row| row.get(0),
        )
        .unwrap_or_else(|_| "claude_code".to_string());

    let messages = session_messages(conn, &sid)?;
    let msg_count = messages.len() as u64;
    let total_cost: f64 = messages.iter().map(|m| m.cost_cents).sum();

    let context_drag = calc_context_drag(conn, &sid, &messages);
    let cache_eff = calc_cache_efficiency(&messages);
    let thrashing = calc_thrashing(conn, &sid);
    let cost_accel = calc_cost_acceleration(&messages, total_cost);

    let vitals = SessionVitals {
        context_drag: context_drag.clone(),
        cache_efficiency: cache_eff.clone(),
        thrashing: thrashing.clone(),
        cost_acceleration: cost_accel.clone(),
    };

    let all_vitals: Vec<(&str, &Option<VitalScore>)> = vec![
        ("thrashing", &thrashing),
        ("cache_efficiency", &cache_eff),
        ("context_drag", &context_drag),
        ("cost_acceleration", &cost_accel),
    ];

    let any_vital_computed = all_vitals.iter().any(|(_, v)| v.is_some());
    let overall_state = if any_vital_computed {
        worst_state(&all_vitals)
    } else {
        "gray".to_string()
    };
    let is_cursor = provider == "cursor";
    let details = generate_details(&all_vitals, is_cursor);
    let tip = generate_tip(&overall_state, &details, total_cost, msg_count, is_cursor);

    Ok(SessionHealth {
        state: overall_state,
        message_count: msg_count,
        total_cost_cents: total_cost,
        vitals,
        tip,
        details,
    })
}

/// Batch compute health states for a list of sessions (for the sessions list view).
/// Returns session_id → overall health state.
/// Lightweight batch health check for the sessions list view.
/// Computes only context_drag and cost_acceleration (not cache_efficiency or thrashing)
/// to keep the list query fast. The full `session_health()` computes all four vitals,
/// so list and detail views may show different health states.
pub fn session_health_batch(
    conn: &Connection,
    session_ids: &[&str],
) -> Result<std::collections::HashMap<String, String>> {
    let mut result = std::collections::HashMap::new();
    if session_ids.is_empty() {
        return Ok(result);
    }

    let placeholders: Vec<String> = (1..=session_ids.len()).map(|i| format!("?{i}")).collect();
    let in_clause = placeholders.join(",");

    let sql = format!(
        "SELECT session_id, input_tokens, output_tokens,
                cache_creation_tokens, cache_read_tokens,
                COALESCE(cost_cents, 0.0), model
         FROM messages
         WHERE session_id IN ({in_clause}) AND role = 'assistant'
         ORDER BY session_id, timestamp ASC"
    );

    let mut stmt = conn.prepare(&sql)?;
    let params: Vec<&dyn rusqlite::types::ToSql> = session_ids
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    struct MiniMsg {
        session_id: String,
        model: Option<String>,
        input_tokens: u64,
        cache_read_tokens: u64,
        cache_creation_tokens: u64,
        cost_cents: f64,
    }

    let rows: Vec<MiniMsg> = stmt
        .query_map(params.as_slice(), |row| {
            Ok(MiniMsg {
                session_id: row.get(0)?,
                model: row.get(6)?,
                input_tokens: row.get(1)?,
                cache_read_tokens: row.get(4)?,
                cache_creation_tokens: row.get(3)?,
                cost_cents: row.get(5)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    let mut grouped: std::collections::HashMap<String, Vec<&MiniMsg>> =
        std::collections::HashMap::new();
    for msg in &rows {
        grouped
            .entry(msg.session_id.clone())
            .or_default()
            .push(msg);
    }

    for (sid, msgs) in &grouped {
        let total_cost: f64 = msgs.iter().map(|m| m.cost_cents).sum();
        let n = msgs.len();

        // Context drag — filter to dominant model to avoid cross-model false positives
        let cd = {
            let mut model_count: std::collections::HashMap<&str, usize> =
                std::collections::HashMap::new();
            for m in msgs.iter() {
                if let Some(ref model) = m.model {
                    *model_count.entry(model.as_str()).or_default() += 1;
                }
            }
            let dominant = model_count
                .iter()
                .max_by_key(|(_, c)| **c)
                .map(|(m, _)| *m);
            let model_msgs: Vec<&&MiniMsg> = if let Some(dom) = dominant {
                msgs.iter().filter(|m| m.model.as_deref() == Some(dom)).collect()
            } else {
                msgs.iter().collect()
            };
            let mn = model_msgs.len();
            if mn >= 5 {
                let window = 3.min(mn);
                let first_avg = model_msgs[..window]
                    .iter()
                    .map(|m| (m.input_tokens + m.cache_read_tokens + m.cache_creation_tokens) as f64)
                    .sum::<f64>()
                    / window as f64;
                let last_avg = model_msgs[mn - window..]
                    .iter()
                    .map(|m| (m.input_tokens + m.cache_read_tokens + m.cache_creation_tokens) as f64)
                    .sum::<f64>()
                    / window as f64;
                if first_avg > 0.0 {
                    let ratio = last_avg / first_avg;
                    Some(vital_state_from_ratio(ratio, 3.0, 8.0))
                } else {
                    None
                }
            } else {
                None
            }
        };

        // Cost acceleration — filter to dominant model (by cost) like calc_cost_acceleration does
        let ca = if n >= 8 && total_cost >= 10.0 {
            let mut model_cost: std::collections::HashMap<&str, f64> =
                std::collections::HashMap::new();
            for m in msgs.iter() {
                if let Some(ref model) = m.model {
                    *model_cost.entry(model.as_str()).or_default() += m.cost_cents;
                }
            }
            let dominant = model_cost
                .iter()
                .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
                .map(|(m, _)| *m);
            let main_msgs: Vec<&&MiniMsg> = if let Some(dom) = dominant {
                msgs.iter().filter(|m| m.model.as_deref() == Some(dom)).collect()
            } else {
                msgs.iter().collect()
            };
            let mn = main_msgs.len();
            if mn >= 6 {
                let half = mn / 2;
                let first_avg =
                    main_msgs[..half].iter().map(|m| m.cost_cents).sum::<f64>() / half as f64;
                let second_avg = main_msgs[half..].iter().map(|m| m.cost_cents).sum::<f64>()
                    / (mn - half) as f64;
                if first_avg > 0.0 {
                    let ratio = second_avg / first_avg;
                    Some(vital_state_from_ratio(ratio, 2.0, 4.0))
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        let all: Vec<Option<&str>> = vec![cd.as_deref(), ca.as_deref()];
        let state = all
            .iter()
            .filter_map(|s| *s)
            .max_by_key(|s| match *s {
                "red" => 2,
                "yellow" => 1,
                _ => 0,
            })
            .unwrap_or("green")
            .to_string();

        result.insert(sid.clone(), state);
    }

    // Sessions with no messages default to "green"
    for sid in session_ids {
        result
            .entry(sid.to_string())
            .or_insert_with(|| "green".to_string());
    }

    Ok(result)
}

// ---------------------------------------------------------------------------
// Vital calculations
// ---------------------------------------------------------------------------

fn vital_state_from_ratio(ratio: f64, yellow_threshold: f64, red_threshold: f64) -> String {
    if ratio >= red_threshold {
        "red".to_string()
    } else if ratio >= yellow_threshold {
        "yellow".to_string()
    } else {
        "green".to_string()
    }
}

fn calc_context_drag(
    conn: &Connection,
    session_id: &str,
    messages: &[MessageRow],
) -> Option<VitalScore> {
    // If a compact happened, only consider messages after the last compact.
    let last_compact_ts: Option<String> = conn
        .query_row(
            "SELECT MAX(timestamp) FROM hook_events
             WHERE session_id = ?1 AND event = 'pre_compact'",
            params![session_id],
            |row| row.get(0),
        )
        .ok()
        .flatten();

    let effective: &[MessageRow] = if let Some(ref ts) = last_compact_ts {
        let start = messages.iter().position(|m| m.timestamp.as_str() > ts.as_str());
        match start {
            Some(idx) => &messages[idx..],
            None => messages,
        }
    } else {
        messages
    };

    // Use the dominant model (most messages) to avoid false positives from
    // cross-model comparisons. Subagent models have independent context windows
    // and their token counts are not comparable to the main model's.
    let mut model_count: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for m in effective {
        if let Some(ref model) = m.model {
            *model_count.entry(model.as_str()).or_default() += 1;
        }
    }
    let dominant = model_count
        .iter()
        .max_by_key(|(_, count)| **count)
        .map(|(model, _)| *model);

    let model_msgs: Vec<&MessageRow> = if let Some(dom) = dominant {
        effective
            .iter()
            .filter(|m| m.model.as_deref() == Some(dom))
            .collect()
    } else {
        effective.iter().collect()
    };

    if model_msgs.len() < 5 {
        return None;
    }
    let window = 3.min(model_msgs.len());
    let total_input =
        |m: &&MessageRow| (m.input_tokens + m.cache_read_tokens + m.cache_creation_tokens) as f64;

    let first_avg = model_msgs[..window]
        .iter()
        .map(total_input)
        .sum::<f64>()
        / window as f64;
    let last_avg = model_msgs[model_msgs.len() - window..]
        .iter()
        .map(total_input)
        .sum::<f64>()
        / window as f64;

    if first_avg <= 0.0 {
        return None;
    }
    let ratio = last_avg / first_avg;
    let state = if ratio >= 6.0 {
        "red"
    } else if ratio >= 3.0 {
        "yellow"
    } else {
        "green"
    };
    Some(VitalScore {
        state: state.to_string(),
        label: format!("{ratio:.1}x growth"),
    })
}

fn calc_cache_efficiency(messages: &[MessageRow]) -> Option<VitalScore> {
    if messages.len() < 5 {
        return None;
    }
    let total_cache_read: u64 = messages.iter().map(|m| m.cache_read_tokens).sum();
    let total_input: u64 = messages
        .iter()
        .map(|m| m.input_tokens + m.cache_read_tokens)
        .sum();
    if total_input == 0 {
        return None;
    }
    let hit_rate = total_cache_read as f64 / total_input as f64;
    let pct = (hit_rate * 100.0).round() as u64;
    let state = if hit_rate < 0.70 {
        "red"
    } else if hit_rate < 0.85 {
        "yellow"
    } else {
        "green"
    };
    Some(VitalScore {
        state: state.to_string(),
        label: format!("{pct}% cache"),
    })
}

fn calc_thrashing(conn: &Connection, session_id: &str) -> Option<VitalScore> {
    let mut stmt = conn
        .prepare(
            "SELECT event, timestamp FROM hook_events
         WHERE session_id = ?1 AND event IN ('post_tool_use', 'user_prompt_submit')
         ORDER BY timestamp ASC",
        )
        .ok()?;

    let events: Vec<(String, String)> = stmt
        .query_map(params![session_id], |row| Ok((row.get(0)?, row.get(1)?)))
        .ok()?
        .filter_map(|r| r.ok())
        .collect();

    if events.is_empty() {
        return None;
    }

    // Detect rapid-fire sequences: 12+ tool events within 60s without user prompt.
    // Normal agent turns do 5-10 tool calls (read, edit, grep); thrashing is 12+
    // rapid calls where the agent is retrying the same failing operation.
    let mut rapid_sequences = 0;
    let mut streak_timestamps: Vec<&str> = Vec::new();

    let check_streak = |ts: &[&str], count: &mut usize| {
        if ts.len() < 12 {
            return;
        }
        let first = ts
            .first()
            .and_then(|t| t.parse::<chrono::DateTime<chrono::Utc>>().ok());
        let last = ts
            .last()
            .and_then(|t| t.parse::<chrono::DateTime<chrono::Utc>>().ok());
        if let (Some(f), Some(l)) = (first, last) {
            if (l - f).num_seconds() <= 60 {
                *count += 1;
            }
        }
    };

    for (event, ts) in &events {
        if event == "user_prompt_submit" {
            check_streak(&streak_timestamps, &mut rapid_sequences);
            streak_timestamps.clear();
            continue;
        }
        streak_timestamps.push(ts.as_str());
    }
    check_streak(&streak_timestamps, &mut rapid_sequences);

    let state = if rapid_sequences >= 5 {
        "red"
    } else if rapid_sequences >= 2 {
        "yellow"
    } else {
        "green"
    };

    Some(VitalScore {
        state: state.to_string(),
        label: if rapid_sequences == 0 {
            "no rapid-fire".to_string()
        } else {
            format!("{rapid_sequences} rapid sequence(s)")
        },
    })
}

fn calc_cost_acceleration(messages: &[MessageRow], total_cost: f64) -> Option<VitalScore> {
    if messages.len() < 8 || total_cost < 10.0 {
        return None;
    }

    // Find the dominant model (most cost) to filter out subagent noise.
    let mut model_cost: std::collections::HashMap<&str, f64> = std::collections::HashMap::new();
    for m in messages {
        if let Some(ref model) = m.model {
            *model_cost.entry(model.as_str()).or_default() += m.cost_cents;
        }
    }
    let dominant = model_cost
        .iter()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(k, _)| *k);

    // Use only dominant-model messages for the acceleration check
    let main_msgs: Vec<&MessageRow> = if let Some(dom) = dominant {
        messages
            .iter()
            .filter(|m| m.model.as_deref() == Some(dom))
            .collect()
    } else {
        messages.iter().collect()
    };

    if main_msgs.len() < 6 {
        return None;
    }

    let half = main_msgs.len() / 2;
    let first_avg = main_msgs[..half].iter().map(|m| m.cost_cents).sum::<f64>() / half as f64;
    let second_avg = main_msgs[half..]
        .iter()
        .map(|m| m.cost_cents)
        .sum::<f64>()
        / (main_msgs.len() - half) as f64;

    if first_avg <= 0.0 {
        return None;
    }
    let ratio = second_avg / first_avg;
    let state = if ratio >= 5.0 {
        "red"
    } else if ratio >= 2.5 {
        "yellow"
    } else {
        "green"
    };
    let avg_cost = total_cost / messages.len() as f64;
    Some(VitalScore {
        state: state.to_string(),
        label: format!("{ratio:.1}x cost, {avg_cost:.0}¢/msg"),
    })
}

// ---------------------------------------------------------------------------
// Tip generation
// ---------------------------------------------------------------------------

fn worst_state(vitals: &[(&str, &Option<VitalScore>)]) -> String {
    vitals
        .iter()
        .filter_map(|(_, v)| v.as_ref())
        .map(|v| v.state.as_str())
        .max_by_key(|s| match *s {
            "red" => 2,
            "yellow" => 1,
            _ => 0,
        })
        .unwrap_or("green")
        .to_string()
}

fn generate_details(
    vitals: &[(&str, &Option<VitalScore>)],
    is_cursor: bool,
) -> Vec<HealthDetail> {
    let mut details: Vec<HealthDetail> = Vec::new();

    let new_session = if is_cursor {
        "start a new composer session"
    } else {
        "start a new session"
    };

    for (name, vital) in vitals {
        if let Some(v) = vital {
            if v.state == "green" {
                continue;
            }
            let tip: String = match (*name, v.state.as_str()) {
                ("context_drag", "yellow") => {
                    if is_cursor {
                        format!("Your context has grown significantly.\n→ Consider starting a new composer session if switching tasks.")
                    } else {
                        format!("Your context has grown significantly.\n→ Run /compact to summarize and drop unused context.\n→ Or {new_session} if switching tasks.")
                    }
                }
                ("context_drag", "red") => {
                    format!("Context is bloated — input tokens are 6x+ the session start.\n→ Start fresh. The context is too large for effective work.")
                }
                ("cache_efficiency", "yellow") => {
                    format!("Cache hit rate is dropping below 85%.\n→ Check if tool definitions, MCP config, or system prompt changed.\n→ A model switch mid-session also resets the cache prefix.")
                }
                ("cache_efficiency", "red") => {
                    if is_cursor {
                        format!("Cache is mostly dead — less than 70% hit rate.\n→ Start a new composer session to rebuild the cache prefix.")
                    } else {
                        format!("Cache is mostly dead — less than 70% hit rate.\n→ Run /clear to reset context but preserve the cache-friendly prefix.\n→ Or {new_session}.")
                    }
                }
                ("thrashing", "yellow") => {
                    format!("Agent is making rapid-fire tool calls — possible retry loop.\n→ Check test output or error messages the agent is reacting to.\n→ Intervene if the agent is stuck.")
                }
                ("thrashing", "red") => {
                    format!("Agent is stuck in a loop — multiple rapid-fire tool sequences detected.\n→ Intervene now. The agent is likely retrying the same failing operation.")
                }
                ("cost_acceleration", "yellow") => {
                    if is_cursor {
                        format!("Cost per message is rising — the second half costs 2.5x+ the first.\n→ Context growth may be driving up token counts.\n→ Consider a new composer session.")
                    } else {
                        format!("Cost per message is rising — the second half costs 2.5x+ the first.\n→ Context growth may be driving up token counts.\n→ Consider /compact or a new session.")
                    }
                }
                ("cost_acceleration", "red") => {
                    format!("Cost per message has exploded — 5x+ increase.\n→ {new_session}. You're burning money on bloated context.")
                }
                _ => continue,
            };

            details.push(HealthDetail {
                vital: name.to_string(),
                state: v.state.clone(),
                label: v.label.clone(),
                tip,
            });
        }
    }

    // Sort: red first, then yellow; within same state, keep priority order (thrashing > cache > context > cost)
    details.sort_by(|a, b| {
        let state_ord =
            |s: &str| -> u8 { if s == "red" { 0 } else { 1 } };
        state_ord(&a.state).cmp(&state_ord(&b.state))
    });

    details
}

fn generate_tip(
    overall_state: &str,
    details: &[HealthDetail],
    total_cost: f64,
    msg_count: u64,
    is_cursor: bool,
) -> String {
    if overall_state == "gray" {
        return "Session starting".to_string();
    }
    if overall_state == "green" {
        return "Session healthy".to_string();
    }

    let new_session = if is_cursor {
        "new composer session"
    } else {
        "new session"
    };

    // Use the worst (first) detail for the short tip
    let base = if let Some(d) = details.first() {
        match (d.vital.as_str(), d.state.as_str()) {
            ("context_drag", "yellow") => {
                if is_cursor {
                    format!("Context growing — consider {new_session}")
                } else {
                    "Context growing — consider /compact".to_string()
                }
            }
            ("context_drag", "red") => format!("Context bloated — start {new_session}"),
            ("cache_efficiency", "yellow") => {
                "Cache dropping — check tool/MCP config".to_string()
            }
            ("cache_efficiency", "red") => format!("Cache dead — start {new_session}"),
            ("thrashing", "yellow") => "Agent retrying — check test output".to_string(),
            ("thrashing", "red") => "Agent stuck in loop — intervene now".to_string(),
            ("cost_acceleration", "yellow") => {
                let avg = if msg_count > 0 {
                    total_cost / msg_count as f64
                } else {
                    0.0
                };
                format!("Cost rising — {avg:.0}¢/msg now")
            }
            ("cost_acceleration", "red") => {
                let avg = if msg_count > 0 {
                    total_cost / msg_count as f64 / 100.0
                } else {
                    0.0
                };
                format!("Burning ${avg:.2}/msg — {new_session} recommended")
            }
            _ => format!("Session {overall_state}"),
        }
    } else {
        format!("Session {overall_state}")
    };

    let extra = details.len().saturating_sub(1);
    if extra > 0 {
        format!("{base} (+{extra} issue{})", if extra == 1 { "" } else { "s" })
    } else {
        base
    }
}
