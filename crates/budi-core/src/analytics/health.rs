//! Session health: vitals computation, tips, and batch health checks.
//!
//! Four vitals are computed per session:
//! - **context_drag** — context-size growth over the current working stretch
//! - **cache_efficiency** — recent cache reuse for the active model stretch
//! - **thrashing** — tool failure loops inside a turn
//! - **cost_acceleration** — cost growth (per user-turn when hook data exists,
//!   otherwise per assistant reply)
//!
//! Overall state rules:
//! - `green` — new session (no vitals scored yet) or all scored vitals are green
//! - `yellow`/`red` — worst scored vital wins
//!
//! Tips are provider-aware: Claude Code, Cursor, and unknown providers each
//! get different action recommendations where the underlying workflows differ.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};

use super::sessions::session_messages;

// ---------------------------------------------------------------------------
// Provider-aware tip policy
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderKind {
    ClaudeCode,
    Cursor,
    Other,
}

impl ProviderKind {
    fn from_str(s: &str) -> Self {
        match s {
            "cursor" => Self::Cursor,
            "claude_code" => Self::ClaudeCode,
            _ => Self::Other,
        }
    }

    fn new_session_action(&self) -> &'static str {
        match self {
            Self::Cursor => "Start a new composer session.",
            _ => "Start a new session.",
        }
    }

    fn new_session_short(&self) -> &'static str {
        match self {
            Self::Cursor => "new composer session",
            _ => "new session",
        }
    }
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

const MIN_VITALS_FOR_GREEN: usize = 2;

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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<String>,
}

#[derive(Debug, Clone)]
struct HealthMessage {
    timestamp: String,
    model: Option<String>,
    input_tokens: u64,
    cache_creation_tokens: u64,
    cache_read_tokens: u64,
    cost_cents: f64,
}

#[derive(Debug, Clone)]
struct SessionToolEvent {
    event: String,
    timestamp: String,
    tool_name: Option<String>,
}

const CONTEXT_DRAG_YELLOW_RATIO: f64 = 3.0;
const CONTEXT_DRAG_RED_RATIO: f64 = 6.0;
const CONTEXT_DRAG_YELLOW_MIN_INPUT: f64 = 12_000.0;
const CONTEXT_DRAG_RED_MIN_INPUT: f64 = 24_000.0;
const CONTEXT_DRAG_YELLOW_MIN_DELTA: f64 = 6_000.0;
const CONTEXT_DRAG_RED_MIN_DELTA: f64 = 12_000.0;

const CACHE_REUSE_YELLOW: f64 = 0.60;
const CACHE_REUSE_RED: f64 = 0.35;
const CACHE_REUSE_WINDOW: usize = 6;

const COST_ACCEL_YELLOW_RATIO: f64 = 2.0;
const COST_ACCEL_RED_RATIO: f64 = 4.0;
const COST_ACCEL_MIN_SESSION_CENTS: f64 = 25.0;
const COST_ACCEL_YELLOW_MIN_TURN_CENTS: f64 = 5.0;
const COST_ACCEL_RED_MIN_TURN_CENTS: f64 = 12.0;
const COST_ACCEL_MIN_TURNS: usize = 4;
const COST_ACCEL_MIN_REQUESTS: usize = 6;

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
                "SELECT id FROM sessions ORDER BY started_at DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .context("No sessions found")?,
    };

    let provider_str: String = conn
        .query_row(
            "SELECT COALESCE(provider, 'claude_code') FROM sessions WHERE id = ?1",
            params![sid],
            |row| row.get(0),
        )
        .unwrap_or_else(|_| "claude_code".to_string());
    let provider = ProviderKind::from_str(&provider_str);

    let messages = session_messages(conn, &sid)?;
    let msg_count = messages.len() as u64;
    let total_cost: f64 = messages.iter().map(|m| m.cost_cents).sum();
    let metrics = messages
        .iter()
        .map(|m| HealthMessage {
            timestamp: m.timestamp.clone(),
            model: m.model.clone(),
            input_tokens: m.input_tokens,
            cache_creation_tokens: m.cache_creation_tokens,
            cache_read_tokens: m.cache_read_tokens,
            cost_cents: m.cost_cents,
        })
        .collect::<Vec<_>>();
    let mut tool_event_map = load_tool_events(conn, &[sid.as_str()])?;
    let tool_events = tool_event_map.remove(&sid).unwrap_or_default();
    let last_compact_ts = last_compact_timestamp(&tool_events);

    let context_drag = calc_context_drag(&metrics, last_compact_ts.as_deref());
    let cache_eff = calc_cache_efficiency(&metrics, last_compact_ts.as_deref());
    let thrashing = calc_thrashing(&tool_events);
    let cost_accel = calc_cost_acceleration(&metrics, &tool_events, last_compact_ts.as_deref());

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

    let overall_state = overall_state(&all_vitals);
    let details = generate_details(&all_vitals, provider);
    let tip = generate_tip(&overall_state, &details, provider, msg_count);

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
/// Uses the same thresholds as the full session detail view.
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
                COALESCE(cost_cents, 0.0), model, timestamp
         FROM messages
         WHERE session_id IN ({in_clause}) AND role = 'assistant'
         ORDER BY session_id, timestamp ASC"
    );

    let mut stmt = conn.prepare(&sql)?;
    let params: Vec<&dyn rusqlite::types::ToSql> = session_ids
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    let rows: Vec<(String, HealthMessage)> = stmt
        .query_map(params.as_slice(), |row| {
            Ok((
                row.get(0)?,
                HealthMessage {
                    timestamp: row.get(7)?,
                    model: row.get(6)?,
                    input_tokens: row.get(1)?,
                    cache_creation_tokens: row.get(3)?,
                    cache_read_tokens: row.get(4)?,
                    cost_cents: row.get(5)?,
                },
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();

    let mut grouped: std::collections::HashMap<String, Vec<HealthMessage>> =
        std::collections::HashMap::new();
    for (sid, msg) in rows {
        grouped.entry(sid).or_default().push(msg);
    }

    let event_map = load_tool_events(conn, session_ids)?;

    for sid in session_ids {
        let msgs = grouped.get(*sid).cloned().unwrap_or_default();
        let events = event_map.get(*sid).cloned().unwrap_or_default();
        let last_compact_ts = last_compact_timestamp(&events);

        let context_drag = calc_context_drag(&msgs, last_compact_ts.as_deref());
        let cache_efficiency = calc_cache_efficiency(&msgs, last_compact_ts.as_deref());
        let thrashing = calc_thrashing(&events);
        let cost_acceleration = calc_cost_acceleration(&msgs, &events, last_compact_ts.as_deref());
        let all_vitals: Vec<(&str, &Option<VitalScore>)> = vec![
            ("thrashing", &thrashing),
            ("cache_efficiency", &cache_efficiency),
            ("context_drag", &context_drag),
            ("cost_acceleration", &cost_acceleration),
        ];

        result.insert((*sid).to_string(), overall_state(&all_vitals));
    }

    Ok(result)
}

// ---------------------------------------------------------------------------
// Vital calculations
// ---------------------------------------------------------------------------

fn calc_context_drag(
    messages: &[HealthMessage],
    last_compact_ts: Option<&str>,
) -> Option<VitalScore> {
    let effective = dominant_model_messages(&filter_after_last_compact(messages, last_compact_ts));
    if effective.len() < 5 {
        return None;
    }

    let window = 3.min(effective.len());
    let first_avg = average_context_size(&effective[..window]);
    let last_avg = average_context_size(&effective[effective.len() - window..]);
    if first_avg <= 0.0 {
        return None;
    }

    let ratio = last_avg / first_avg;
    let delta = last_avg - first_avg;
    let state = if ratio >= CONTEXT_DRAG_RED_RATIO
        && (last_avg >= CONTEXT_DRAG_RED_MIN_INPUT || delta >= CONTEXT_DRAG_RED_MIN_DELTA)
    {
        "red"
    } else if ratio >= CONTEXT_DRAG_YELLOW_RATIO
        && (last_avg >= CONTEXT_DRAG_YELLOW_MIN_INPUT || delta >= CONTEXT_DRAG_YELLOW_MIN_DELTA)
    {
        "yellow"
    } else {
        "green"
    };

    Some(VitalScore {
        state: state.to_string(),
        label: format!("{ratio:.1}x growth, {}", format_token_count(last_avg)),
    })
}

fn calc_cache_efficiency(
    messages: &[HealthMessage],
    last_compact_ts: Option<&str>,
) -> Option<VitalScore> {
    let effective = filter_after_last_compact(messages, last_compact_ts);
    let recent = recent_model_run(&effective);
    if recent.len() < 4 {
        return None;
    }

    let window_start = recent.len().saturating_sub(CACHE_REUSE_WINDOW);
    let window = &recent[window_start..];
    let total_cache_read: u64 = window.iter().map(|m| m.cache_read_tokens).sum();
    let total_input: u64 = window
        .iter()
        .map(|m| m.input_tokens + m.cache_read_tokens)
        .sum();
    if total_input == 0 {
        return None;
    }

    let hit_rate = total_cache_read as f64 / total_input as f64;
    let pct = (hit_rate * 100.0).round() as u64;
    let state = if hit_rate < CACHE_REUSE_RED {
        "red"
    } else if hit_rate < CACHE_REUSE_YELLOW {
        "yellow"
    } else {
        "green"
    };

    Some(VitalScore {
        state: state.to_string(),
        label: format!("{pct}% recent reuse"),
    })
}

fn calc_thrashing(events: &[SessionToolEvent]) -> Option<VitalScore> {
    let has_tool_events = events
        .iter()
        .any(|e| matches!(e.event.as_str(), "post_tool_use" | "post_tool_use_failure"));
    if !has_tool_events {
        return None;
    }

    let mut turns: Vec<Vec<&SessionToolEvent>> = Vec::new();
    let mut current: Vec<&SessionToolEvent> = Vec::new();
    for event in events {
        if event.event == "user_prompt_submit" {
            if !current.is_empty() {
                turns.push(std::mem::take(&mut current));
            }
            continue;
        }
        if matches!(
            event.event.as_str(),
            "post_tool_use" | "post_tool_use_failure"
        ) {
            current.push(event);
        }
    }
    if !current.is_empty() {
        turns.push(current);
    }

    let score: u32 = turns.iter().map(|turn| score_turn_loop(turn)).sum();
    let loop_count = turns
        .iter()
        .filter(|turn| score_turn_loop(turn) > 0)
        .count();
    let state = if score >= 2 {
        "red"
    } else if score >= 1 {
        "yellow"
    } else {
        "green"
    };

    Some(VitalScore {
        state: state.to_string(),
        label: if loop_count == 0 {
            "no retry loops".to_string()
        } else {
            format!(
                "{loop_count} retry loop{}",
                if loop_count == 1 { "" } else { "s" }
            )
        },
    })
}

fn calc_cost_acceleration(
    messages: &[HealthMessage],
    events: &[SessionToolEvent],
    last_compact_ts: Option<&str>,
) -> Option<VitalScore> {
    let effective = dominant_model_messages(&filter_after_last_compact(messages, last_compact_ts));
    let turn_costs = user_turn_costs(&effective, events, last_compact_ts);
    let (costs, label_unit) = if turn_costs.len() >= COST_ACCEL_MIN_TURNS {
        (turn_costs, "turn")
    } else if turn_costs.is_empty() {
        let request_costs: Vec<f64> = effective.iter().map(|m| m.cost_cents).collect();
        if request_costs.len() < COST_ACCEL_MIN_REQUESTS {
            return None;
        }
        (request_costs, "reply")
    } else {
        // When we have prompt boundaries but only a couple of turns, suppress the vital
        // instead of falling back to request-level math. That avoids false reds on
        // agentic sessions where one user turn fans out into several assistant calls.
        return None;
    };

    let total_cost: f64 = costs.iter().sum();
    if total_cost < COST_ACCEL_MIN_SESSION_CENTS {
        return None;
    }

    let half = costs.len() / 2;
    if half == 0 || costs.len() == half {
        return None;
    }

    let first_avg = costs[..half].iter().sum::<f64>() / half as f64;
    let second_avg = costs[half..].iter().sum::<f64>() / (costs.len() - half) as f64;
    if first_avg <= 0.0 {
        return None;
    }

    let ratio = second_avg / first_avg;
    let state = if ratio >= COST_ACCEL_RED_RATIO && second_avg >= COST_ACCEL_RED_MIN_TURN_CENTS {
        "red"
    } else if ratio >= COST_ACCEL_YELLOW_RATIO && second_avg >= COST_ACCEL_YELLOW_MIN_TURN_CENTS {
        "yellow"
    } else {
        "green"
    };

    Some(VitalScore {
        state: state.to_string(),
        label: format!("{ratio:.1}x growth, {:.0}¢/{label_unit}", second_avg),
    })
}

fn user_turn_costs(
    messages: &[&HealthMessage],
    events: &[SessionToolEvent],
    last_compact_ts: Option<&str>,
) -> Vec<f64> {
    let compact_at = last_compact_ts.and_then(parse_timestamp);
    let prompt_times: Vec<DateTime<Utc>> = events
        .iter()
        .filter(|e| e.event == "user_prompt_submit")
        .filter_map(|e| parse_timestamp(&e.timestamp))
        .filter(|ts| match compact_at.as_ref() {
            Some(compact) => ts > compact,
            None => true,
        })
        .collect();
    if prompt_times.is_empty() {
        return Vec::new();
    }

    let mut turn_costs = vec![0.0; prompt_times.len()];
    let mut turn_message_counts = vec![0usize; prompt_times.len()];
    let mut prompt_idx = 0usize;

    for message in messages {
        let Some(message_ts) = parse_timestamp(&message.timestamp) else {
            continue;
        };

        while prompt_idx + 1 < prompt_times.len() && message_ts >= prompt_times[prompt_idx + 1] {
            prompt_idx += 1;
        }

        if message_ts >= prompt_times[prompt_idx] {
            turn_costs[prompt_idx] += message.cost_cents;
            turn_message_counts[prompt_idx] += 1;
        }
    }

    turn_costs
        .into_iter()
        .zip(turn_message_counts)
        .filter_map(|(cost, count)| if count > 0 { Some(cost) } else { None })
        .collect()
}

// ---------------------------------------------------------------------------
// Overall state
// ---------------------------------------------------------------------------

/// Determines the session-level health state.
/// - New sessions and sessions with too few vitals → `green` (positive default).
/// - Otherwise returns the worst scored state.
fn overall_state(vitals: &[(&str, &Option<VitalScore>)]) -> String {
    let scored: Vec<&str> = vitals
        .iter()
        .filter_map(|(_, v)| v.as_ref())
        .map(|v| v.state.as_str())
        .collect();

    if scored.is_empty() {
        return "green".to_string();
    }

    let worst = scored
        .iter()
        .max_by_key(|s| match **s {
            "red" => 2,
            "yellow" => 1,
            _ => 0,
        })
        .unwrap_or(&"green");

    if *worst == "green" && scored.len() < MIN_VITALS_FOR_GREEN {
        return "green".to_string();
    }
    worst.to_string()
}

// ---------------------------------------------------------------------------
// Tip generation (provider-aware)
// ---------------------------------------------------------------------------

fn generate_details(
    vitals: &[(&str, &Option<VitalScore>)],
    provider: ProviderKind,
) -> Vec<HealthDetail> {
    let mut details: Vec<HealthDetail> = Vec::new();
    let new_session = provider.new_session_action();

    for (name, vital) in vitals {
        if let Some(v) = vital {
            if v.state == "green" {
                continue;
            }
            let (tip, actions): (String, Vec<String>) = match (*name, v.state.as_str()) {
                ("context_drag", "yellow") => {
                    let actions = match provider {
                        ProviderKind::Cursor => vec![
                            "If the agent is losing focus or you changed tasks, start a new composer session.".to_string(),
                        ],
                        ProviderKind::ClaudeCode => vec![
                            "Run /compact if you want to keep working on the same task.".to_string(),
                            "If the task changed, start a new session instead.".to_string(),
                        ],
                        ProviderKind::Other => vec![
                            "Trim context or start fresh if the agent is losing focus.".to_string(),
                        ],
                    };
                    ("Context size is getting large.".to_string(), actions)
                }
                ("context_drag", "red") => {
                    let actions = match provider {
                        ProviderKind::ClaudeCode => vec![
                            new_session.to_string(),
                            "If you must keep the thread, run /compact first and keep only the current plan.".to_string(),
                        ],
                        _ => vec![new_session.to_string()],
                    };
                    (
                        "Context is large enough to hurt reliability.".to_string(),
                        actions,
                    )
                }
                ("cache_efficiency", "yellow") => {
                    let second = match provider {
                        ProviderKind::Cursor => {
                            "If the session feels sluggish, start a new composer session."
                                .to_string()
                        }
                        ProviderKind::ClaudeCode => {
                            "If the session feels sluggish, run /compact or start fresh."
                                .to_string()
                        }
                        ProviderKind::Other => {
                            "If the session feels sluggish, start fresh.".to_string()
                        }
                    };
                    (
                        "Cache reuse is lower than usual, so turns may get slower.".to_string(),
                        vec![
                            "This is normal after switching models, tools, or task shape."
                                .to_string(),
                            second,
                        ],
                    )
                }
                ("cache_efficiency", "red") => {
                    let first_action = match provider {
                        ProviderKind::Cursor => {
                            "Start a new composer session to rebuild a clean cache prefix."
                                .to_string()
                        }
                        ProviderKind::ClaudeCode => {
                            "Run /clear or start a new session to rebuild the cache.".to_string()
                        }
                        ProviderKind::Other => "Start fresh to rebuild the cache.".to_string(),
                    };
                    (
                        "Cache reuse is very low — you are paying for fresh context each turn.".to_string(),
                        vec![
                            first_action,
                            "If you just changed models or tools, you can ignore this until the cache warms up.".to_string(),
                        ],
                    )
                }
                ("thrashing", "yellow") => (
                    "The agent may be stuck in a failing loop.".to_string(),
                    vec![
                        "Check the latest error or test output before letting it continue."
                            .to_string(),
                        "Give a narrower next step if it keeps retrying.".to_string(),
                    ],
                ),
                ("thrashing", "red") => (
                    "The agent is stuck in a retry loop.".to_string(),
                    vec![
                        "Stop and inspect the most recent failure.".to_string(),
                        "Restart with a more specific prompt after fixing the blocker.".to_string(),
                    ],
                ),
                ("cost_acceleration", "yellow") => {
                    let first = match provider {
                        ProviderKind::Cursor => {
                            "If focus is drifting, start a new composer session.".to_string()
                        }
                        ProviderKind::ClaudeCode => {
                            "Run /compact if you still need the current thread.".to_string()
                        }
                        ProviderKind::Other => "Start fresh if the task has changed.".to_string(),
                    };
                    (
                        format!("Cost is rising — {}", v.label),
                        vec![
                            first,
                            "If the task changed, start fresh instead of carrying old context."
                                .to_string(),
                        ],
                    )
                }
                ("cost_acceleration", "red") => (
                    format!("Cost has spiked — {}", v.label),
                    vec![
                        new_session.to_string(),
                        "Carry over only the plan, file paths, or failing command you still need."
                            .to_string(),
                    ],
                ),
                _ => continue,
            };

            details.push(HealthDetail {
                vital: name.to_string(),
                state: v.state.clone(),
                label: v.label.clone(),
                tip,
                actions,
            });
        }
    }

    details.sort_by(|a, b| {
        let state_ord = |s: &str| -> u8 { if s == "red" { 0 } else { 1 } };
        let vital_ord = |v: &str| -> u8 {
            match v {
                "thrashing" => 0,
                "cache_efficiency" => 1,
                "context_drag" => 2,
                _ => 3,
            }
        };
        state_ord(&a.state)
            .cmp(&state_ord(&b.state))
            .then_with(|| vital_ord(&a.vital).cmp(&vital_ord(&b.vital)))
    });

    details
}

fn generate_tip(
    overall_state: &str,
    details: &[HealthDetail],
    provider: ProviderKind,
    message_count: u64,
) -> String {
    if overall_state == "green" {
        if message_count < 5 {
            return "New session".to_string();
        }
        return "Session healthy".to_string();
    }

    let new_session = provider.new_session_short();

    let base = if let Some(d) = details.first() {
        match (d.vital.as_str(), d.state.as_str()) {
            ("context_drag", "yellow") => match provider {
                ProviderKind::Cursor => {
                    format!("Context growing — start {new_session} if focus drops")
                }
                ProviderKind::ClaudeCode => "Context growing — /compact soon".to_string(),
                ProviderKind::Other => "Context growing — trim context soon".to_string(),
            },
            ("context_drag", "red") => format!("Context too large — start {new_session}"),
            ("cache_efficiency", "yellow") => "Cache reuse low — slower turns possible".to_string(),
            ("cache_efficiency", "red") => format!("Cache reuse very low — start {new_session}"),
            ("thrashing", "yellow") => "Agent may be stuck — check latest error".to_string(),
            ("thrashing", "red") => "Agent stuck retrying — intervene now".to_string(),
            ("cost_acceleration", "yellow") => {
                format!("Cost rising — {}", d.label)
            }
            ("cost_acceleration", "red") => format!("Cost spiking — start {new_session}"),
            _ => format!("Session {overall_state}"),
        }
    } else {
        format!("Session {overall_state}")
    };

    let extra = details.len().saturating_sub(1);
    if extra > 0 {
        format!(
            "{base} (+{extra} issue{})",
            if extra == 1 { "" } else { "s" }
        )
    } else {
        base
    }
}

fn filter_after_last_compact<'a>(
    messages: &'a [HealthMessage],
    last_compact_ts: Option<&str>,
) -> Vec<&'a HealthMessage> {
    match last_compact_ts {
        Some(ts) => {
            let start = messages.iter().position(|m| m.timestamp.as_str() > ts);
            match start {
                Some(idx) => messages[idx..].iter().collect(),
                None => messages.iter().collect(),
            }
        }
        None => messages.iter().collect(),
    }
}

fn dominant_model_messages<'a>(messages: &[&'a HealthMessage]) -> Vec<&'a HealthMessage> {
    let mut model_count: HashMap<&str, usize> = HashMap::new();
    for m in messages {
        if let Some(ref model) = m.model {
            *model_count.entry(model.as_str()).or_default() += 1;
        }
    }
    let dominant = model_count
        .iter()
        .max_by_key(|(_, count)| **count)
        .map(|(model, _)| *model);

    if let Some(dom) = dominant {
        messages
            .iter()
            .copied()
            .filter(|m| m.model.as_deref() == Some(dom))
            .collect()
    } else {
        messages.to_vec()
    }
}

fn recent_model_run<'a>(messages: &[&'a HealthMessage]) -> Vec<&'a HealthMessage> {
    if messages.is_empty() {
        return Vec::new();
    }

    let last_model = messages.last().and_then(|m| m.model.as_deref());
    let mut out: Vec<&HealthMessage> = Vec::new();
    for message in messages.iter().rev() {
        if out.is_empty() || message.model.as_deref() == last_model {
            out.push(*message);
        } else {
            break;
        }
    }
    out.reverse();
    out
}

fn average_context_size(messages: &[&HealthMessage]) -> f64 {
    messages
        .iter()
        .map(|m| (m.input_tokens + m.cache_read_tokens + m.cache_creation_tokens) as f64)
        .sum::<f64>()
        / messages.len() as f64
}

fn format_token_count(tokens: f64) -> String {
    if tokens >= 1_000_000.0 {
        format!("{:.1}M context", tokens / 1_000_000.0)
    } else if tokens >= 1_000.0 {
        format!("{:.0}k context", tokens / 1_000.0)
    } else {
        format!("{tokens:.0} context")
    }
}

fn last_compact_timestamp(events: &[SessionToolEvent]) -> Option<String> {
    events
        .iter()
        .filter(|e| e.event == "pre_compact")
        .map(|e| e.timestamp.clone())
        .max()
}

fn score_turn_loop(turn: &[&SessionToolEvent]) -> u32 {
    if turn.len() < 4 {
        return 0;
    }

    let start = turn.first().and_then(|e| parse_timestamp(&e.timestamp));
    let end = turn.last().and_then(|e| parse_timestamp(&e.timestamp));
    let span_secs = match (start, end) {
        (Some(start), Some(end)) => (end - start).num_seconds(),
        _ => 0,
    };

    let mut unique_tools: HashSet<&str> = HashSet::new();
    let mut failure_count = 0usize;
    let mut failures_by_tool: HashMap<&str, usize> = HashMap::new();
    let mut longest_same_tool_run = 0usize;
    let mut current_tool = "";
    let mut current_run = 0usize;

    for event in turn {
        let tool = event.tool_name.as_deref().unwrap_or("unknown");
        unique_tools.insert(tool);

        if tool == current_tool {
            current_run += 1;
        } else {
            current_tool = tool;
            current_run = 1;
        }
        longest_same_tool_run = longest_same_tool_run.max(current_run);

        if event.event == "post_tool_use_failure" {
            failure_count += 1;
            *failures_by_tool.entry(tool).or_default() += 1;
        }
    }

    let repeated_failure = failures_by_tool.values().copied().max().unwrap_or(0);
    let failure_storm = failure_count >= 4 && failure_count * 2 >= turn.len() && span_secs <= 120;
    let same_tool_burst = longest_same_tool_run >= 4 && span_secs <= 90;
    let ping_pong = turn.len() >= 8 && unique_tools.len() <= 2 && span_secs <= 90;

    if repeated_failure >= 5 || (same_tool_burst && failure_count >= 4) {
        2
    } else if repeated_failure >= 3
        || failure_storm
        || (same_tool_burst && failure_count >= 2)
        || (ping_pong && failure_count >= 2)
    {
        1
    } else {
        0
    }
}

fn parse_timestamp(ts: &str) -> Option<DateTime<Utc>> {
    ts.parse::<DateTime<Utc>>().ok()
}

fn load_tool_events(
    conn: &Connection,
    session_ids: &[&str],
) -> Result<HashMap<String, Vec<SessionToolEvent>>> {
    let mut grouped = HashMap::new();
    if session_ids.is_empty() {
        return Ok(grouped);
    }

    let placeholders: Vec<String> = (1..=session_ids.len()).map(|i| format!("?{i}")).collect();
    let in_clause = placeholders.join(",");
    let sql = format!(
        "SELECT session_id, event, timestamp, tool_name
         FROM hook_events
         WHERE session_id IN ({in_clause})
           AND event IN ('pre_compact', 'post_tool_use', 'post_tool_use_failure', 'user_prompt_submit')
         ORDER BY session_id, timestamp ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let params: Vec<&dyn rusqlite::types::ToSql> = session_ids
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    let rows = stmt.query_map(params.as_slice(), |row| {
        Ok((
            row.get::<_, String>(0)?,
            SessionToolEvent {
                event: row.get(1)?,
                timestamp: row.get(2)?,
                tool_name: row.get(3)?,
            },
        ))
    })?;

    for row in rows.filter_map(|r| r.ok()) {
        grouped.entry(row.0).or_insert_with(Vec::new).push(row.1);
    }

    Ok(grouped)
}
