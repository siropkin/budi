//! Insights engine — analyzes Claude Code usage patterns and produces
//! actionable recommendations.

use std::path::Path;

use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;

use crate::analytics;

/// All insights — cross-data analysis with actionable recommendations.
#[derive(Debug, Clone, Serialize)]
pub struct Insights {
    pub search_efficiency: SearchEfficiency,
    pub cache_efficiency: CacheEfficiency,
    pub mcp_tools: Vec<McpToolInsight>,
    pub token_heavy_sessions: Vec<TokenHeavySessionInsight>,
    pub claude_md_files: Vec<ClaudeMdInsight>,
    pub session_patterns: analytics::SessionPatternStats,
    pub tool_diversity: analytics::ToolDiversity,
    pub daily_cost: Vec<analytics::DailyCost>,
    pub config_health: ConfigHealth,
    pub recommendations: Vec<Recommendation>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchEfficiency {
    pub search_calls: u64,
    pub total_calls: u64,
    pub ratio: f64,
    pub recommendation: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct McpToolInsight {
    pub server: String,
    pub tool: String,
    pub call_count: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClaudeMdInsight {
    pub path: String,
    pub size_bytes: u64,
    pub est_tokens: u64,
    pub recommendation: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CacheEfficiency {
    pub hit_rate: f64,
    pub total_cache_read_tokens: u64,
    pub total_input_tokens: u64,
    pub recommendation: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TokenHeavySessionInsight {
    pub session_id: String,
    pub project_dir: Option<String>,
    pub repo_id: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub ratio: f64,
}

const TOKEN_HEAVY_THRESHOLD: f64 = 5.0;
const CLAUDE_MD_TOKEN_WARN: u64 = 2000;
const SEARCH_RATIO_WARN: f64 = 0.40;
const CACHE_HIT_LOW: f64 = 0.30;

/// Context overhead summary across all projects.
#[derive(Debug, Clone, Serialize)]
pub struct ConfigHealth {
    pub total_config_files: u64,
    pub total_tokens: u64,
    pub heaviest_project: Option<String>,
    pub heaviest_tokens: u64,
}

/// A prioritised, actionable recommendation.
#[derive(Debug, Clone, Serialize)]
pub struct Recommendation {
    pub category: String,
    pub severity: String, // "info", "warn", "good"
    pub title: String,
    pub detail: String,
}

/// Generate all insights with cross-data analysis.
pub fn generate_insights(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    tz_offset: i32,
) -> Result<Insights> {
    let search = build_search_efficiency(conn, since, until)?;
    let cache = build_cache_efficiency(conn, since, until)?;
    let mcp = build_mcp_insights(conn, since, until)?;
    let heavy = build_token_heavy(conn, since, until)?;
    let claude_md = build_claude_md_insights(conn)?;
    let patterns = analytics::session_patterns(conn, since, until)?;
    let diversity = analytics::tool_diversity(conn, since, until)?;
    let daily = analytics::daily_cost_trend(conn, since, until, tz_offset)?;
    let config = build_config_health(conn)?;

    let mut recs = Vec::new();
    build_recommendations(
        &search, &cache, &heavy, &claude_md, &patterns, &diversity, &config, &daily, &mut recs,
    );

    Ok(Insights {
        search_efficiency: search,
        cache_efficiency: cache,
        mcp_tools: mcp,
        token_heavy_sessions: heavy,
        claude_md_files: claude_md,
        session_patterns: patterns,
        tool_diversity: diversity,
        daily_cost: daily,
        config_health: config,
        recommendations: recs,
    })
}

fn build_config_health(conn: &Connection) -> Result<ConfigHealth> {
    let config_files = analytics::config_files(conn)?;
    let total_tokens: u64 = config_files.iter().map(|f| f.est_tokens).sum();

    // Group by project to find heaviest
    let mut by_project: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    for f in &config_files {
        *by_project.entry(f.project.clone()).or_default() += f.est_tokens;
    }
    let heaviest = by_project
        .iter()
        .max_by_key(|(_, v)| *v)
        .map(|(k, v)| (k.clone(), *v));

    Ok(ConfigHealth {
        total_config_files: config_files.len() as u64,
        total_tokens,
        heaviest_project: heaviest.as_ref().map(|(k, _)| k.clone()),
        heaviest_tokens: heaviest.map(|(_, v)| v).unwrap_or(0),
    })
}

#[allow(clippy::too_many_arguments)]
fn build_recommendations(
    search: &SearchEfficiency,
    cache: &CacheEfficiency,
    heavy: &[TokenHeavySessionInsight],
    claude_md: &[ClaudeMdInsight],
    patterns: &analytics::SessionPatternStats,
    diversity: &analytics::ToolDiversity,
    config: &ConfigHealth,
    daily: &[analytics::DailyCost],
    recs: &mut Vec<Recommendation>,
) {
    // Search efficiency
    if search.total_calls > 0 && search.ratio > SEARCH_RATIO_WARN {
        recs.push(Recommendation {
            category: "search".into(),
            severity: "warn".into(),
            title: format!("{:.0}% of tool calls are searches", search.ratio * 100.0),
            detail: "Add key file paths to CLAUDE.md so Claude finds code faster instead of searching repeatedly.".into(),
        });
    } else if search.total_calls > 20 {
        recs.push(Recommendation {
            category: "search".into(),
            severity: "good".into(),
            title: format!("Search ratio is healthy at {:.0}%", search.ratio * 100.0),
            detail: "Claude is finding code efficiently without excessive searching.".into(),
        });
    }

    // Cache efficiency
    if cache.total_input_tokens > 0 && cache.hit_rate < CACHE_HIT_LOW {
        recs.push(Recommendation {
            category: "cache".into(),
            severity: "warn".into(),
            title: format!("Low cache hit rate: {:.0}%", cache.hit_rate * 100.0),
            detail: "Keep sessions focused on one task. Longer, focused conversations reuse cached context better.".into(),
        });
    } else if cache.total_input_tokens > 0 {
        recs.push(Recommendation {
            category: "cache".into(),
            severity: "good".into(),
            title: format!("Cache hit rate: {:.0}%", cache.hit_rate * 100.0),
            detail: "Prompt caching is saving you money. Keep sessions focused to maintain this."
                .into(),
        });
    }

    // Token-heavy sessions
    if heavy.len() >= 3 {
        recs.push(Recommendation {
            category: "sessions".into(),
            severity: "warn".into(),
            title: format!("{} token-heavy sessions (5x+ input/output ratio)", heavy.len()),
            detail: "These sessions send lots of context but get little output back. Try breaking large tasks into smaller, focused sessions.".into(),
        });
    }

    // CLAUDE.md size
    let oversized: Vec<_> = claude_md
        .iter()
        .filter(|f| f.est_tokens > CLAUDE_MD_TOKEN_WARN)
        .collect();
    if !oversized.is_empty() {
        recs.push(Recommendation {
            category: "config".into(),
            severity: "warn".into(),
            title: format!("{} CLAUDE.md file(s) over {}K tokens", oversized.len(), CLAUDE_MD_TOKEN_WARN / 1000),
            detail: "Large CLAUDE.md files add to every prompt. Review for unused sections to reduce context overhead and cost.".into(),
        });
    }

    // Config overhead
    if config.total_tokens > 10_000 {
        recs.push(Recommendation {
            category: "config".into(),
            severity: "warn".into(),
            title: format!("{}K tokens in config files across all projects", config.total_tokens / 1000),
            detail: format!(
                "All config files (CLAUDE.md, rules, skills, memory) add up to ~{}K tokens per session. Heaviest: {}.",
                config.total_tokens / 1000,
                config.heaviest_project.as_deref().unwrap_or("unknown")
            ),
        });
    }

    // Session patterns
    if patterns.total_sessions > 0 && patterns.avg_duration_mins < 2.0 {
        recs.push(Recommendation {
            category: "sessions".into(),
            severity: "info".into(),
            title: "Very short average sessions".into(),
            detail: format!(
                "Average session is {:.1} min. Short sessions have higher cache overhead. Consider keeping sessions open longer for related tasks.",
                patterns.avg_duration_mins
            ),
        });
    }

    // Tool diversity
    if diversity.total_calls > 50 && diversity.top_tool_pct > 60.0 {
        recs.push(Recommendation {
            category: "tools".into(),
            severity: "info".into(),
            title: format!("{} dominates at {:.0}% of all tool calls", diversity.top_tool.as_deref().unwrap_or("?"), diversity.top_tool_pct),
            detail: "Heavy reliance on a single tool. This is normal for read-heavy exploration, but during implementation you'd expect more Edit/Write usage.".into(),
        });
    }

    // Cost trend — spike detection
    if daily.len() >= 3 {
        let avg_cost: f64 = daily.iter().map(|d| d.cost).sum::<f64>() / daily.len() as f64;
        if let Some(peak) = daily
            .iter()
            .max_by(|a, b| {
                a.cost
                    .partial_cmp(&b.cost)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .filter(|p| avg_cost > 0.0 && p.cost > avg_cost * 3.0)
        {
            recs.push(Recommendation {
                category: "cost".into(),
                severity: "info".into(),
                title: format!("Cost spike on {}: ${:.2}", peak.date, peak.cost),
                detail: format!(
                    "This was {:.1}x the daily average (${:.2}). {} sessions ran that day.",
                    peak.cost / avg_cost,
                    avg_cost,
                    peak.sessions
                ),
            });
        }
    }
}

fn build_search_efficiency(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<SearchEfficiency> {
    let stats = analytics::search_tool_stats(conn, since, until)?;
    let recommendation = if stats.total_calls == 0 {
        None
    } else if stats.ratio > SEARCH_RATIO_WARN {
        Some(format!(
            "{:.0}% of tool calls are searches (Grep/Glob). Consider adding key file paths \
             or patterns to CLAUDE.md so Claude finds code faster.",
            stats.ratio * 100.0
        ))
    } else {
        Some(format!(
            "Search ratio is {:.0}% — healthy. Claude is finding code efficiently.",
            stats.ratio * 100.0
        ))
    };

    Ok(SearchEfficiency {
        search_calls: stats.search_calls,
        total_calls: stats.total_calls,
        ratio: stats.ratio,
        recommendation,
    })
}

fn build_mcp_insights(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Vec<McpToolInsight>> {
    let stats = analytics::mcp_tool_stats(conn, since, until)?;
    Ok(stats
        .into_iter()
        .map(|s| McpToolInsight {
            server: s.server,
            tool: s.tool,
            call_count: s.call_count,
        })
        .collect())
}

fn build_claude_md_insights(conn: &Connection) -> Result<Vec<ClaudeMdInsight>> {
    let dirs = analytics::project_dirs(conn)?;
    let mut results = Vec::new();

    for dir in &dirs {
        let claude_md_path = Path::new(dir).join("CLAUDE.md");
        if let Ok(metadata) = std::fs::metadata(&claude_md_path) {
            let size = metadata.len();
            let est_tokens = size / 4; // rough approximation: 4 bytes per token
            let recommendation = if est_tokens > CLAUDE_MD_TOKEN_WARN {
                Some(format!(
                    "CLAUDE.md is ~{}K tokens. Review for unused sections to reduce context overhead.",
                    est_tokens / 1000
                ))
            } else {
                None
            };
            results.push(ClaudeMdInsight {
                path: claude_md_path.display().to_string(),
                size_bytes: size,
                est_tokens,
                recommendation,
            });
        }
    }

    Ok(results)
}

fn build_cache_efficiency(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<CacheEfficiency> {
    let stats = analytics::cache_stats(conn, since, until)?;
    let recommendation = if stats.total_input_tokens == 0 {
        None
    } else if stats.hit_rate < CACHE_HIT_LOW {
        Some(format!(
            "Cache hit rate is {:.0}%. Longer conversations within the same session improve \
             cache reuse. Consider keeping sessions focused on one task.",
            stats.hit_rate * 100.0
        ))
    } else {
        Some(format!(
            "Cache hit rate is {:.0}% — good. Prompt caching is saving tokens effectively.",
            stats.hit_rate * 100.0
        ))
    };

    Ok(CacheEfficiency {
        hit_rate: stats.hit_rate,
        total_cache_read_tokens: stats.total_cache_read_tokens,
        total_input_tokens: stats.total_input_tokens,
        recommendation,
    })
}

fn build_token_heavy(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Vec<TokenHeavySessionInsight>> {
    let sessions = analytics::token_heavy_sessions(conn, since, until, TOKEN_HEAVY_THRESHOLD)?;
    Ok(sessions
        .into_iter()
        .map(|s| TokenHeavySessionInsight {
            session_id: s.session_id,
            project_dir: s.project_dir,
            repo_id: s.repo_id,
            input_tokens: s.input_tokens,
            output_tokens: s.output_tokens,
            ratio: s.ratio,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::ingest_messages;
    use crate::jsonl::ParsedMessage;

    fn test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .unwrap();
        crate::analytics::migrate_for_test(&conn);
        conn
    }

    fn sample_data() -> Vec<ParsedMessage> {
        vec![ParsedMessage {
            uuid: "a1".to_string(),
            session_id: Some("s1".to_string()),
            timestamp: "2026-03-14T10:00:00Z".parse().unwrap(),
            cwd: Some("/tmp/proj".to_string()),
            role: "assistant".to_string(),
            model: Some("claude-opus-4-6".to_string()),
            input_tokens: 1000,
            output_tokens: 500,
            cache_creation_tokens: 100,
            cache_read_tokens: 400,
            tool_names: vec!["Grep".to_string(), "Glob".to_string(), "Edit".to_string()],
            has_thinking: false,
            stop_reason: Some("end_turn".to_string()),
            text_length: 100,
            version: None,
            git_branch: None,
            repo_id: None,
            provider: "claude_code".to_string(),
            cost_cents: None,
            context_tokens_used: None,
            context_token_limit: None,
            interaction_mode: None,
            session_title: None,
            lines_added: None,
            lines_removed: None,
        }]
    }

    #[test]
    fn generate_insights_runs() {
        let mut conn = test_db();
        ingest_messages(&mut conn, &sample_data()).unwrap();

        let insights = generate_insights(&conn, None, None, 0).unwrap();
        assert_eq!(insights.search_efficiency.search_calls, 2);
        assert_eq!(insights.search_efficiency.total_calls, 3);
        assert!(insights.search_efficiency.recommendation.is_some());
        // Cache: total_input = 1000 + 100 + 400 = 1500, cache_read = 400
        assert_eq!(insights.cache_efficiency.total_input_tokens, 1500);
        assert_eq!(insights.cache_efficiency.total_cache_read_tokens, 400);
    }

    #[test]
    fn search_efficiency_recommendation_high() {
        let mut conn = test_db();
        // 2 out of 3 = 66.7% search ratio → should trigger warning
        ingest_messages(&mut conn, &sample_data()).unwrap();

        let se = build_search_efficiency(&conn, None, None).unwrap();
        assert!(se.ratio > SEARCH_RATIO_WARN);
        let rec = se.recommendation.unwrap();
        assert!(rec.contains("searches"));
    }

    #[test]
    fn empty_db_produces_no_crash() {
        let conn = test_db();
        let insights = generate_insights(&conn, None, None, 0).unwrap();
        assert_eq!(insights.search_efficiency.total_calls, 0);
        assert!(insights.search_efficiency.recommendation.is_none());
        assert!(insights.mcp_tools.is_empty());
        assert!(insights.claude_md_files.is_empty());
        assert!(insights.token_heavy_sessions.is_empty());
    }
}
