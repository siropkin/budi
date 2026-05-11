//! Session health: vitals computation, tips, and batch health checks.
//!
//! Four vitals are computed per session:
//! - **context_drag** — context-size growth over the current working stretch
//! - **cache_efficiency** — recent cache reuse for the active model stretch
//! - **thrashing** — tool failure loops inside a turn
//! - **cost_acceleration** — cost growth (per user-turn when hook data exists,
//!   otherwise per assistant reply)
//!
//! Overall state rules (see #441 — never paint a trust-killer green over an
//! all-N/A session):
//! - `red` / `yellow` — any scored vital went red/yellow. An actual issue
//!   signal trumps N/A count: one red metric and three N/A still renders red.
//! - `insufficient_data` — no vitals scored, or too many are N/A to make a
//!   trustworthy healthy call (`>= 3 of 4` N/A, i.e. `<= 1 scored`). Renders
//!   as `⚪ INSUFFICIENT DATA` in the CLI; statusline / sessions list fall
//!   through to a neutral open circle rather than a green dot.
//! - `green` — all scored vitals are green AND at most 2 of 4 are N/A. If
//!   exactly 2 are N/A, the tip notes the partial coverage so the user isn't
//!   surprised when they see some `N/A` rows.
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

/// Minimum number of N/A vitals (out of 4) at which the overall verdict
/// flips from `green` → `insufficient_data`. At 3+ N/A we don't have enough
/// signal to call a session "healthy" without eroding trust (#441).
const INSUFFICIENT_DATA_NA_THRESHOLD: usize = 3;

/// Overall state string emitted when too few vitals could be scored to paint
/// a trustworthy verdict. Downstream consumers (CLI `vitals`, `sessions`
/// list, statusline, Cursor extension) must render a neutral icon and never
/// a green light — a green light over all-N/A is the trust-killer from #441.
pub const STATE_INSUFFICIENT_DATA: &str = "insufficient_data";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionHealth {
    pub state: String,
    pub message_count: u64,
    /// Denominator the statusline `message` slot divides session cost by
    /// (#691). Counts user-typed prompts so subagent fan-out and unpriced
    /// rows don't deflate the per-message average. For providers that don't
    /// yet capture user rows (copilot_chat pre-#686 sessions), falls back to
    /// `COUNT(DISTINCT request_id WHERE role='assistant' AND cost_cents > 0)`
    /// so the slot still reads sensibly.
    #[serde(default)]
    pub user_prompt_count: u64,
    pub total_cost_cents: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_lag_hint: Option<String>,
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
        None => {
            match conn.query_row(
                "WITH latest_assistant AS (
                     SELECT session_id, MAX(timestamp) AS last_assistant_at
                     FROM messages
                     WHERE role = 'assistant' AND session_id IS NOT NULL
                     GROUP BY session_id
                 )
                 SELECT s.id
                 FROM sessions s
                 LEFT JOIN latest_assistant la ON la.session_id = s.id
                 ORDER BY
                     (la.last_assistant_at IS NULL) ASC,
                     la.last_assistant_at DESC,
                     COALESCE(s.started_at, s.ended_at) DESC,
                     s.id DESC
                 LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            ) {
                Ok(id) => id,
                Err(rusqlite::Error::QueryReturnedNoRows) => {
                    return Ok(SessionHealth {
                        state: "green".to_string(),
                        message_count: 0,
                        user_prompt_count: 0,
                        total_cost_cents: 0.0,
                        cost_lag_hint: None,
                        vitals: SessionVitals {
                            context_drag: None,
                            cache_efficiency: None,
                            thrashing: None,
                            cost_acceleration: None,
                        },
                        tip: "No sessions yet".to_string(),
                        details: vec![],
                    });
                }
                Err(e) => return Err(e).context("Failed to query latest session"),
            }
        }
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
    let user_prompt_count = compute_user_prompt_count(conn, &sid, &provider_str)?;
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
    let na_count = all_vitals.iter().filter(|(_, v)| v.is_none()).count();
    let details = generate_details(&all_vitals, provider);
    let tip = generate_tip(&overall_state, &details, provider, msg_count, na_count);

    let cost_lag_hint = if provider == ProviderKind::Cursor {
        if let Some(last_msg) = messages.last() {
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&last_msg.timestamp) {
                let age = chrono::Utc::now().signed_duration_since(dt.with_timezone(&chrono::Utc));
                if age.num_minutes() < 10 {
                    Some(crate::analytics::CURSOR_LAG_HINT.to_string())
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    Ok(SessionHealth {
        state: overall_state,
        message_count: msg_count,
        user_prompt_count,
        total_cost_cents: total_cost,
        cost_lag_hint,
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
                COALESCE(cost_cents_effective, 0.0), model, timestamp
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

/// Determines the session-level health state (#441).
///
/// - An actual yellow/red signal always dominates — one red vital with three
///   N/A still renders red, because INSUFFICIENT DATA would hide a real issue.
/// - Otherwise, if too many vitals are N/A (`>= 3 of 4`, i.e. `<= 1 scored`),
///   the verdict is `insufficient_data`. Painting green here erodes trust the
///   moment the user notices the contradiction with the N/A rows.
/// - Otherwise the verdict is `green`. The number of N/A is surfaced in the
///   tip (see `generate_tip`) when it's non-zero, so a partial-coverage green
///   is honest about its partiality.
fn overall_state(vitals: &[(&str, &Option<VitalScore>)]) -> String {
    let scored: Vec<&str> = vitals
        .iter()
        .filter_map(|(_, v)| v.as_ref())
        .map(|v| v.state.as_str())
        .collect();
    let na_count = vitals.len().saturating_sub(scored.len());

    if scored.contains(&"red") {
        return "red".to_string();
    }
    if scored.contains(&"yellow") {
        return "yellow".to_string();
    }

    if na_count >= INSUFFICIENT_DATA_NA_THRESHOLD {
        return STATE_INSUFFICIENT_DATA.to_string();
    }

    "green".to_string()
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
    na_count: usize,
) -> String {
    if overall_state == STATE_INSUFFICIENT_DATA {
        // #602: surface the assistant-message count so a user wondering "why
        // is this stuck on N/A?" sees data is flowing and the warm-up window.
        // First scoring threshold is 5 assistant messages (context_drag); the
        // others (cache_efficiency, cost_acceleration) join in over the next
        // few turns. `thrashing` stays N/A in 8.3.x (rebuild pending).
        return match message_count {
            0 => "Not enough session data yet to assess (no assistant messages yet)".to_string(),
            1 => "Not enough session data yet to assess (1 assistant message so far — vitals warm up over the first ~5 turns)".to_string(),
            n => format!(
                "Not enough session data yet to assess ({n} assistant messages so far — vitals warm up over the first ~5 turns)"
            ),
        };
    }
    if overall_state == "green" {
        if message_count < 5 {
            return "New session".to_string();
        }
        // #441 ambiguity resolution: treat "at least 3 of 4 metrics actually
        // returned a numeric value" as the dominant rule for plain green.
        // Exactly 2 N/A (half the surface missing) is the partial band,
        // because 1 N/A is the normal post-v22 state for any session
        // without hook_events and would be noise on every healthy session.
        if na_count >= 2 {
            return format!(
                "Session healthy (partial — {na_count} metrics need more session data)"
            );
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
    _conn: &Connection,
    _session_ids: &[&str],
) -> Result<HashMap<String, Vec<SessionToolEvent>>> {
    Ok(HashMap::new())
}

/// Denominator for the statusline `message` slot (#691).
///
/// Default path counts `role='user'` rows so a single user prompt fanning
/// out into N subagents stays at `1`, and zero-cost assistant rows from
/// unpriced models don't dilute the average.
///
/// Fallback: `copilot_chat` shipped user-row capture in #686, but pre-#686
/// sessions still on disk don't have any user rows. For that provider only,
/// when no user rows exist, count distinct priced assistant requests so the
/// slot still renders something sensible. Once those legacy sessions age
/// out, the fallback is silently inert because the user-row path wins.
fn compute_user_prompt_count(conn: &Connection, sid: &str, provider: &str) -> Result<u64> {
    let user_count: u64 = conn
        .query_row(
            "SELECT COUNT(*) FROM messages WHERE session_id = ?1 AND role = 'user'",
            params![sid],
            |r| r.get(0),
        )
        .context("Failed to count user-role messages for session")?;
    if user_count > 0 {
        return Ok(user_count);
    }
    if provider == "copilot_chat" {
        let priced_requests: u64 = conn
            .query_row(
                "SELECT COUNT(DISTINCT request_id) FROM messages
                 WHERE session_id = ?1
                   AND role = 'assistant'
                   AND request_id IS NOT NULL
                   AND COALESCE(cost_cents_effective, 0.0) > 0.0",
                params![sid],
                |r| r.get(0),
            )
            .context("Failed to count priced copilot_chat requests for session")?;
        return Ok(priced_requests);
    }
    Ok(0)
}
