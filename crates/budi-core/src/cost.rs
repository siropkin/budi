//! Token cost estimation dispatched through the provider trait.

use anyhow::Result;
use rusqlite::Connection;

use crate::analytics::{DimensionFilters, UNTAGGED_DIMENSION};

/// Estimated cost breakdown.
///
/// #520 (8.3.2): `total_cost` is `SUM(cost_cents)` from `messages`
/// (the authoritative cost stored at ingest time by `CostEnricher`,
/// which may include thinking tokens, fast-mode multipliers, and web-
/// search fees). The four per-component fields (`input_cost` /
/// `output_cost` / `cache_write_cost` / `cache_read_cost`) are re-
/// derived from base token sums × current manifest pricing and cover
/// only those four buckets. When Cursor fast-mode or thinking-token
/// cost contributes to a session, the four components can sum to less
/// than `total_cost`. `other_cost` captures that gap so the summary
/// view visibly reconciles.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CostEstimate {
    pub total_cost: f64,
    pub input_cost: f64,
    pub output_cost: f64,
    pub cache_write_cost: f64,
    pub cache_read_cost: f64,
    /// #520: residual cost present in `total_cost` but not captured by
    /// the four base-token components above. Non-zero when Cursor
    /// fast-mode, thinking tokens, or web-search fees contributed to
    /// the aggregate. Defaults to 0 for back-compat; callers that pre-
    /// 8.3.2 ignored this field see no behavior change.
    #[serde(default)]
    pub other_cost: f64,
    pub cache_savings: f64,
}

/// Compute estimated cost with optional provider filter.
pub fn estimate_cost_filtered(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    provider: Option<&str>,
) -> Result<CostEstimate> {
    let filters = DimensionFilters::default();
    estimate_cost_with_filters(conn, since, until, provider, &filters)
}

pub fn estimate_cost_with_filters(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    provider: Option<&str>,
    filters: &DimensionFilters,
) -> Result<CostEstimate> {
    let mut conditions = vec!["role = 'assistant'".to_string()];
    let mut param_values: Vec<String> = Vec::new();
    if let Some(s) = since {
        param_values.push(s.to_string());
        conditions.push(format!("timestamp >= ?{}", param_values.len()));
    }
    if let Some(u) = until {
        param_values.push(u.to_string());
        conditions.push(format!("timestamp < ?{}", param_values.len()));
    }
    if let Some(p) = provider {
        param_values.push(p.to_string());
        conditions.push(format!(
            "COALESCE(provider, 'claude_code') = ?{}",
            param_values.len()
        ));
    }
    if !filters.agents.is_empty() {
        let placeholders: Vec<String> = filters
            .agents
            .iter()
            .filter(|v| !v.trim().is_empty())
            .map(|value| {
                param_values.push(value.trim().to_string());
                format!("?{}", param_values.len())
            })
            .collect();
        if !placeholders.is_empty() {
            conditions.push(format!(
                "COALESCE(provider, 'claude_code') IN ({})",
                placeholders.join(", ")
            ));
        }
    }
    if !filters.models.is_empty() {
        let placeholders: Vec<String> = filters
            .models
            .iter()
            .filter(|v| !v.trim().is_empty())
            .map(|value| {
                param_values.push(value.trim().to_string());
                format!("?{}", param_values.len())
            })
            .collect();
        if !placeholders.is_empty() {
            conditions.push(format!(
                "CASE WHEN model IS NULL OR model = '' OR SUBSTR(model, 1, 1) = '<' THEN '{UNTAGGED_DIMENSION}' ELSE model END IN ({})",
                placeholders.join(", ")
            ));
        }
    }
    if !filters.projects.is_empty() {
        let placeholders: Vec<String> = filters
            .projects
            .iter()
            .filter(|v| !v.trim().is_empty())
            .map(|value| {
                param_values.push(value.trim().to_string());
                format!("?{}", param_values.len())
            })
            .collect();
        if !placeholders.is_empty() {
            conditions.push(format!(
                "COALESCE(NULLIF(NULLIF(repo_id, ''), 'unknown'), '{UNTAGGED_DIMENSION}') IN ({})",
                placeholders.join(", ")
            ));
        }
    }
    if !filters.branches.is_empty() {
        let placeholders: Vec<String> = filters
            .branches
            .iter()
            .filter(|v| !v.trim().is_empty())
            .map(|value| {
                let normalized = value
                    .trim()
                    .strip_prefix("refs/heads/")
                    .unwrap_or(value.trim())
                    .to_string();
                param_values.push(if normalized.is_empty() {
                    UNTAGGED_DIMENSION.to_string()
                } else {
                    normalized
                });
                format!("?{}", param_values.len())
            })
            .collect();
        if !placeholders.is_empty() {
            conditions.push(format!(
                "COALESCE(NULLIF(CASE WHEN COALESCE(git_branch, '') LIKE 'refs/heads/%' THEN SUBSTR(COALESCE(git_branch, ''), 12) ELSE COALESCE(git_branch, '') END, ''), '{UNTAGGED_DIMENSION}') IN ({})",
                placeholders.join(", ")
            ));
        }
    }
    debug_assert!(
        !conditions.is_empty(),
        "conditions always starts with role filter"
    );
    let where_clause = format!("WHERE {}", conditions.join(" AND "));
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    // Use pre-computed SUM(cost_cents_effective) for total_cost to match bar
    // charts, and token-based calculation for the input/output/cache breakdown.
    // ADR-0094 §1: every read surface reads `_effective`; until the team-pricing
    // worker (#731) ships, `_effective = _ingested`.
    let sum_sql = format!(
        "SELECT COALESCE(SUM(cost_cents_effective), 0) FROM messages {}",
        where_clause
    );
    let sum_cost_cents: f64 = conn.query_row(&sum_sql, param_refs.as_slice(), |r| r.get(0))?;

    // Group by provider + model to apply correct per-provider pricing for breakdown
    let sql = format!(
        "SELECT provider,
                COALESCE(model, 'unknown'),
                COALESCE(SUM(input_tokens), 0),
                COALESCE(SUM(output_tokens), 0),
                COALESCE(SUM(cache_creation_tokens), 0),
                COALESCE(SUM(cache_read_tokens), 0)
         FROM messages {}
         GROUP BY provider, COALESCE(model, 'unknown')",
        where_clause
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(param_refs.as_slice(), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, u64>(2)?,
            row.get::<_, u64>(3)?,
            row.get::<_, u64>(4)?,
            row.get::<_, u64>(5)?,
        ))
    })?;

    let mut total = CostEstimate {
        total_cost: 0.0,
        input_cost: 0.0,
        output_cost: 0.0,
        cache_write_cost: 0.0,
        cache_read_cost: 0.0,
        other_cost: 0.0,
        cache_savings: 0.0,
    };

    for row in rows {
        let (prov, model, input, output, cache_write, cache_read) = row?;
        // ADR-0091: pricing::lookup is the sole path to rates. Unknown
        // models contributed `cost_cents = 0` to the total at ingest time
        // (enforced by `CostEnricher`), so they must contribute 0 to the
        // breakdown too — skip them rather than applying a phantom rate.
        let p = match crate::pricing::lookup(&model, &prov) {
            crate::pricing::PricingOutcome::Known { pricing, .. } => pricing,
            crate::pricing::PricingOutcome::Unknown { .. } => continue,
        };
        let ic = input as f64 * p.input / 1_000_000.0;
        let oc = output as f64 * p.output / 1_000_000.0;
        let cwc = cache_write as f64 * p.cache_write / 1_000_000.0;
        let crc = cache_read as f64 * p.cache_read / 1_000_000.0;
        // Savings: what cache reads would have cost at full input price
        let savings = cache_read as f64 * (p.input - p.cache_read) / 1_000_000.0;

        total.input_cost += ic;
        total.output_cost += oc;
        total.cache_write_cost += cwc;
        total.cache_read_cost += crc;
        total.cache_savings += savings;
    }

    // Use pre-computed cost_cents for total (consistent with bar charts)
    total.total_cost = sum_cost_cents / 100.0;

    // Round to cents
    total.total_cost = (total.total_cost * 100.0).round() / 100.0;
    total.input_cost = (total.input_cost * 100.0).round() / 100.0;
    total.output_cost = (total.output_cost * 100.0).round() / 100.0;
    total.cache_write_cost = (total.cache_write_cost * 100.0).round() / 100.0;
    total.cache_read_cost = (total.cache_read_cost * 100.0).round() / 100.0;
    total.cache_savings = (total.cache_savings * 100.0).round() / 100.0;

    // #520: the four per-component fields sum from base-token × rate,
    // while `total_cost` is `SUM(cost_cents)` from ingest (which can
    // include thinking / fast-mode / web-search contributions).
    // Compute `other_cost` as the residual so the summary view
    // visually reconciles. Clamp to >= 0 — a negative residual would
    // mean the four components over-counted vs the stored total,
    // which shouldn't happen but guards against a future pricing
    // drift surfacing as a confusing "other -$N" row on screen.
    let components_sum =
        total.input_cost + total.output_cost + total.cache_write_cost + total.cache_read_cost;
    let other = total.total_cost - components_sum;
    total.other_cost = ((other * 100.0).round() / 100.0).max(0.0);

    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .unwrap();
        crate::migration::migrate(&conn).unwrap();
        conn
    }

    #[test]
    fn cost_empty_db() {
        let conn = setup_db();
        let cost = estimate_cost_filtered(&conn, None, None, None).unwrap();
        assert_eq!(cost.total_cost, 0.0);
    }

    #[test]
    fn cost_single_opus_message() {
        let conn = setup_db();
        // 1M input * $5/M = $5.00, 100K output * $25/M = $2.50, total = $7.50 = 750 cents
        conn.execute(
            "INSERT INTO messages (id, role, timestamp, model, input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens, cost_cents_ingested, cost_cents_effective)
             VALUES (?1, 'assistant', '2026-03-21T00:00:00Z', 'claude-opus-4-6', ?2, ?3, ?4, ?5, ?6, ?6)",
            params!["msg1", 1_000_000i64, 100_000i64, 0i64, 0i64, 750.0],
        ).unwrap();
        let cost = estimate_cost_filtered(&conn, None, None, None).unwrap();
        assert_eq!(cost.input_cost, 5.0);
        assert_eq!(cost.output_cost, 2.5);
        assert_eq!(cost.total_cost, 7.5);
    }

    #[test]
    fn cost_with_cache_savings() {
        let conn = setup_db();
        // total = 0.30 + 0.75 + 0.75 + 0.15 = $1.95 = 195 cents
        conn.execute(
            "INSERT INTO messages (id, role, timestamp, model, input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens, cost_cents_ingested, cost_cents_effective)
             VALUES (?1, 'assistant', '2026-03-21T00:00:00Z', 'claude-sonnet-4-6', ?2, ?3, ?4, ?5, ?6, ?6)",
            params!["msg1", 100_000i64, 50_000i64, 200_000i64, 500_000i64, 195.0],
        ).unwrap();
        let cost = estimate_cost_filtered(&conn, None, None, None).unwrap();
        // input: 100K * $3/M = $0.30
        // output: 50K * $15/M = $0.75
        // cache_write: 200K * $3.75/M = $0.75
        // cache_read: 500K * $0.30/M = $0.15
        assert_eq!(cost.total_cost, 1.95);
        // savings: 500K * ($3.00 - $0.30) / 1M = $1.35
        assert_eq!(cost.cache_savings, 1.35);
    }

    #[test]
    fn cost_with_date_filter() {
        let conn = setup_db();
        // 1M input * $3/M = $3.00 = 300 cents each
        conn.execute(
            "INSERT INTO messages (id, role, timestamp, model, input_tokens, output_tokens, cost_cents_ingested, cost_cents_effective)
             VALUES ('old', 'assistant', '2026-03-01T00:00:00Z', 'claude-sonnet-4-6', 1000000, 0, 300.0, 300.0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, role, timestamp, model, input_tokens, output_tokens, cost_cents_ingested, cost_cents_effective)
             VALUES ('new', 'assistant', '2026-03-21T00:00:00Z', 'claude-sonnet-4-6', 1000000, 0, 300.0, 300.0)",
            [],
        )
        .unwrap();
        let cost = estimate_cost_filtered(&conn, Some("2026-03-20"), None, None).unwrap();
        // Only the "new" message: 1M * $3/M = $3.00
        assert_eq!(cost.input_cost, 3.0);
        assert_eq!(cost.total_cost, 3.0);
    }

    /// Verify that mixed-model cost calculation applies correct per-model pricing.
    #[test]
    fn cost_mixed_models() {
        let conn = setup_db();
        // Opus 4.6: 100K input * $5/M = $0.50 = 50 cents
        conn.execute(
            "INSERT INTO messages (id, role, timestamp, model, input_tokens, output_tokens, cost_cents_ingested, cost_cents_effective)
             VALUES ('m1', 'assistant', '2026-03-21T00:00:00Z', 'claude-opus-4-6', 100000, 0, 50.0, 50.0)",
            [],
        ).unwrap();
        // Haiku 4.5: 100K input * $1/M = $0.10 = 10 cents
        conn.execute(
            "INSERT INTO messages (id, role, timestamp, model, input_tokens, output_tokens, cost_cents_ingested, cost_cents_effective)
             VALUES ('m2', 'assistant', '2026-03-21T00:00:00Z', 'claude-haiku-4-5-20251001', 100000, 0, 10.0, 10.0)",
            [],
        ).unwrap();
        // Sonnet 4.6: 100K input * $3/M = $0.30 = 30 cents
        conn.execute(
            "INSERT INTO messages (id, role, timestamp, model, input_tokens, output_tokens, cost_cents_ingested, cost_cents_effective)
             VALUES ('m3', 'assistant', '2026-03-21T00:00:00Z', 'claude-sonnet-4-6', 100000, 0, 30.0, 30.0)",
            [],
        ).unwrap();
        let cost = estimate_cost_filtered(&conn, None, None, None).unwrap();
        // Total: $0.50 + $0.10 + $0.30 = $0.90
        assert_eq!(cost.total_cost, 0.90);
        // Input breakdown: $0.50 + $0.10 + $0.30 = $0.90
        assert_eq!(cost.input_cost, 0.90);
    }

    /// Verify that token fields don't overlap (Anthropic API: input_tokens is
    /// non-cached input, separate from cache_creation and cache_read tokens).
    #[test]
    fn cost_token_fields_no_double_counting() {
        let conn = setup_db();
        // Simulate real data: input=3 (non-cached), cache_create=14873, cache_read=0
        // Cost should be: 3*$5/M + 0*$25/M + 14873*$6.25/M + 0*$0.50/M
        let input_cost = 3.0 * 5.0 / 1_000_000.0;
        let cache_write_cost = 14873.0 * 6.25 / 1_000_000.0;
        let total_dollars = input_cost + cache_write_cost;
        let total_cents = total_dollars * 100.0;
        conn.execute(
            "INSERT INTO messages (id, role, timestamp, model, input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens, cost_cents_ingested, cost_cents_effective)
             VALUES ('m1', 'assistant', '2026-03-21T00:00:00Z', 'claude-opus-4-6', 3, 0, 14873, 0, ?1, ?1)",
            params![total_cents],
        ).unwrap();
        let cost = estimate_cost_filtered(&conn, None, None, None).unwrap();
        // input_tokens (3) charged at input rate, NOT at cache_write rate
        // cache_creation_tokens (14873) charged at cache_write rate
        // These must not overlap
        // input_cost: 3 * $5/M = $0.000015 → rounds to $0.00
        assert!(
            cost.input_cost < 0.01,
            "input cost should be tiny for 3 tokens, got {}",
            cost.input_cost
        );
        // cache_write_cost: 14873 * $6.25/M = $0.0929 → rounds to $0.09
        assert!(
            cost.cache_write_cost >= 0.09,
            "cache write should be ~$0.09, got {}",
            cost.cache_write_cost
        );
    }

    /// Validate that cost_cents stored per-message matches aggregate token-based
    /// recalculation. This catches rounding drift, double-counting, and pricing bugs.
    /// Simulates a realistic workload with mixed models and cache patterns.
    #[test]
    fn cost_aggregate_matches_per_message_sum() {
        let conn = setup_db();
        // Simulate realistic mix: many small Opus messages, some Sonnet, some Haiku
        let scenarios: &[(&str, &str, u64, u64, u64, u64)] = &[
            // (uuid_prefix, model, input, output, cache_w, cache_r)
            // Typical first message: small input, small output, large cache write
            ("opus1", "claude-opus-4-6", 3, 32, 16267, 9985),
            // Mid-conversation: growing cache reads
            ("opus2", "claude-opus-4-6", 1, 273, 845, 49002),
            // Large response
            ("opus3", "claude-opus-4-6", 1, 4521, 302, 51685),
            // Tool use (small output)
            ("opus4", "claude-opus-4-6", 1, 36, 879, 51383),
            // Sonnet message
            ("son1", "claude-sonnet-4-6", 5, 512, 8234, 42000),
            // Haiku
            ("hai1", "claude-haiku-4-5-20251001", 10, 128, 3000, 15000),
            // Opus 4.5 (different model ID, same pricing)
            ("opus45", "claude-opus-4-5-20251101", 2, 200, 10000, 30000),
        ];

        let mut expected_total_cents = 0.0f64;
        for (prefix, model, inp, out, cw, cr) in scenarios {
            let pricing = match crate::pricing::lookup(model, "claude_code") {
                crate::pricing::PricingOutcome::Known { pricing, .. } => pricing,
                crate::pricing::PricingOutcome::Unknown { .. } => {
                    panic!("test fixture model {model} must be known in the manifest")
                }
            };
            let cost = *inp as f64 * pricing.input / 1_000_000.0
                + *out as f64 * pricing.output / 1_000_000.0
                + *cw as f64 * pricing.cache_write / 1_000_000.0
                + *cr as f64 * pricing.cache_read / 1_000_000.0;
            let cost_cents = cost * 100.0;
            expected_total_cents += cost_cents;

            conn.execute(
                "INSERT INTO messages (id, role, timestamp, model, input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens, cost_cents_ingested, cost_cents_effective)
                 VALUES (?1, 'assistant', '2026-03-21T00:00:00Z', ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
                params![*prefix, *model, *inp as i64, *out as i64, *cw as i64, *cr as i64, cost_cents],
            ).unwrap();
        }

        let cost = estimate_cost_filtered(&conn, None, None, None).unwrap();
        let stored_total_cents = expected_total_cents;
        let stored_total_usd = stored_total_cents / 100.0;

        // Aggregate recalculated cost (from estimate_cost_filtered breakdown)
        let breakdown_total =
            cost.input_cost + cost.output_cost + cost.cache_write_cost + cost.cache_read_cost;

        // total_cost comes from SUM(cost_cents)/100 — should match stored values
        assert!(
            (cost.total_cost - (stored_total_usd * 100.0).round() / 100.0).abs() < 0.01,
            "total_cost ({}) should match stored sum ({})",
            cost.total_cost,
            stored_total_usd
        );

        // Breakdown should approximately match total (may differ slightly due to rounding)
        assert!(
            (breakdown_total - cost.total_cost).abs() < 0.02,
            "breakdown ({}) should match total ({})",
            breakdown_total,
            cost.total_cost
        );
    }

    /// Verify that the Anthropic API's token semantics are correctly handled:
    /// input_tokens is NON-CACHED input (exclusive of cache tokens).
    /// Total billed input = input_tokens + cache_creation_input_tokens + cache_read_input_tokens,
    /// each at their respective rate.
    #[test]
    fn anthropic_token_semantics_no_overlap() {
        // Real example from JSONL: input=3, cache_create=16267, cache_read=9985
        // If input_tokens INCLUDED cache, total input would be 3 (absurdly small for a full prompt).
        // The fact that input_tokens=3 while cache=26252 proves they're exclusive.
        //
        // Correct cost: 3 × $5/M + 16267 × $6.25/M + 9985 × $0.50/M
        // Wrong cost (if double-counting): (3+16267+9985) × $5/M + 16267 × $6.25/M + 9985 × $0.50/M
        let p = match crate::pricing::lookup("claude-opus-4-6", "claude_code") {
            crate::pricing::PricingOutcome::Known { pricing, .. } => pricing,
            crate::pricing::PricingOutcome::Unknown { .. } => {
                panic!("claude-opus-4-6 must be known in the manifest")
            }
        };
        let correct =
            3.0 * p.input / 1e6 + 16267.0 * p.cache_write / 1e6 + 9985.0 * p.cache_read / 1e6;
        let wrong_double_count = (3.0 + 16267.0 + 9985.0) * p.input / 1e6
            + 16267.0 * p.cache_write / 1e6
            + 9985.0 * p.cache_read / 1e6;

        // Our calculation should match the correct approach
        let conn = setup_db();
        let cost_cents = correct * 100.0;
        conn.execute(
            "INSERT INTO messages (id, role, timestamp, model, input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens, cost_cents_ingested, cost_cents_effective)
             VALUES ('test', 'assistant', '2026-03-21T00:00:00Z', 'claude-opus-4-6', 3, 0, 16267, 9985, ?1, ?1)",
            params![cost_cents],
        ).unwrap();
        let result = estimate_cost_filtered(&conn, None, None, None).unwrap();

        // Correct cost: ~$0.1067
        assert!(
            (result.total_cost - correct).abs() < 0.01,
            "should use correct (exclusive) token accounting: got {}, expected ~{}",
            result.total_cost,
            correct
        );
        // Wrong cost would be ~$0.2379 (2.2x higher) — verify we're NOT doing this
        assert!(
            (result.total_cost - wrong_double_count).abs() > 0.05,
            "should NOT double-count input tokens"
        );
    }

    /// Simulate realistic workload and verify cost_cents precision is maintained
    /// at f64 level (no premature rounding to integer cents).
    #[test]
    fn cost_cents_stored_as_f64_not_integer() {
        let conn = setup_db();
        // A message that costs exactly 0.5 cents ($0.005)
        // If stored as integer: 0 or 1 cent (up to 100% error)
        // If stored as f64: 0.5 cents exactly
        conn.execute(
            "INSERT INTO messages (id, role, timestamp, model, input_tokens, output_tokens, cost_cents_ingested, cost_cents_effective)
             VALUES ('half', 'assistant', '2026-03-21T00:00:00Z', 'claude-opus-4-6', 0, 0, 0.5, 0.5)",
            [],
        ).unwrap();
        let stored: f64 = conn
            .query_row(
                "SELECT cost_cents_effective FROM messages WHERE id = 'half'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            (stored - 0.5).abs() < 1e-10,
            "cost_cents should store sub-cent precision"
        );
    }

    #[test]
    fn cost_sub_cent_messages_not_lost() {
        let conn = setup_db();
        // 100 small messages, each: 3 input + 36 output tokens on Opus 4.6
        // Per message: 3*$5/1M + 36*$25/1M = $0.000015 + $0.0009 = $0.000915 = 0.0915 cents
        // Total: 100 * 0.0915 = 9.15 cents = $0.0915
        // Before fix: each rounded to 0 cents → total $0.00 (100% loss)
        // After fix: each stored as 0.0915 cents → total $0.09 (rounded at display)
        for i in 0..100 {
            conn.execute(
                "INSERT INTO messages (id, role, timestamp, model, input_tokens, output_tokens, cost_cents_ingested, cost_cents_effective)
                 VALUES (?1, 'assistant', '2026-03-21T00:00:00Z', 'claude-opus-4-6', 3, 36, ?2, ?2)",
                params![format!("msg{}", i), 0.0915],
            ).unwrap();
        }
        let cost = estimate_cost_filtered(&conn, None, None, None).unwrap();
        // 100 * 0.0915 cents = 9.15 cents = $0.0915 → rounds to $0.09
        assert_eq!(cost.total_cost, 0.09);
        assert!(
            cost.total_cost > 0.0,
            "sub-cent messages should not be lost"
        );
    }

    /// #520: when `cost_cents` at ingest exceeds the sum of the four
    /// base-token components (e.g. Cursor fast-mode, thinking tokens,
    /// web-search fees), `other_cost` captures the residual so the
    /// summary render sums back to the top line.
    #[test]
    fn cost_other_component_captures_residual_above_base_tokens() {
        let conn = setup_db();
        // Seed a row whose stored `cost_cents` (1000 ¢ = $10.00) is
        // higher than the pure base-token × manifest-rate recomputation.
        // For `claude-opus-4-6`: 1M input × $5/M = $5.00, 100K output ×
        // $25/M = $2.50 → components sum $7.50. The extra $2.50 is the
        // "other" bucket — thinking/fast-mode/web-search in real data.
        conn.execute(
            "INSERT INTO messages (id, role, timestamp, model, input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens, cost_cents_ingested, cost_cents_effective)
             VALUES (?1, 'assistant', '2026-03-21T00:00:00Z', 'claude-opus-4-6', ?2, ?3, 0, 0, ?4, ?4)",
            params!["msg1", 1_000_000i64, 100_000i64, 1000.0],
        ).unwrap();
        let cost = estimate_cost_filtered(&conn, None, None, None).unwrap();
        assert_eq!(cost.total_cost, 10.0);
        assert_eq!(cost.input_cost, 5.0);
        assert_eq!(cost.output_cost, 2.5);
        assert_eq!(cost.cache_write_cost, 0.0);
        assert_eq!(cost.cache_read_cost, 0.0);
        assert_eq!(cost.other_cost, 2.5);
        // Reconciliation: four components + other == total.
        let sum = cost.input_cost
            + cost.output_cost
            + cost.cache_write_cost
            + cost.cache_read_cost
            + cost.other_cost;
        assert!(
            (sum - cost.total_cost).abs() < 0.005,
            "sub-line must reconcile to top line: sum={sum}, total={}",
            cost.total_cost,
        );
    }

    /// #520: when the four base-token components already equal the
    /// stored total, `other_cost` is 0 so the render suppresses the
    /// "other" cell and the sub-line reads as pre-8.3.2.
    #[test]
    fn cost_other_component_is_zero_when_no_residual() {
        let conn = setup_db();
        // `claude-opus-4-6`: 1M input × $5/M + 100K output × $25/M = $7.50.
        // Stored `cost_cents` matches the recomputation exactly, so
        // `other_cost` should be zero.
        conn.execute(
            "INSERT INTO messages (id, role, timestamp, model, input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens, cost_cents_ingested, cost_cents_effective)
             VALUES (?1, 'assistant', '2026-03-21T00:00:00Z', 'claude-opus-4-6', ?2, ?3, 0, 0, ?4, ?4)",
            params!["msg1", 1_000_000i64, 100_000i64, 750.0],
        ).unwrap();
        let cost = estimate_cost_filtered(&conn, None, None, None).unwrap();
        assert_eq!(cost.total_cost, 7.5);
        assert_eq!(cost.other_cost, 0.0);
    }

    /// #520 defensive: a negative residual (components > total — only
    /// possible under an incoherent pricing state) clamps to 0 so the
    /// render never shows a confusing "other -$N" cell. The stored
    /// `total_cost` remains the authoritative number.
    #[test]
    fn cost_other_component_clamps_negative_residual_to_zero() {
        let conn = setup_db();
        // Under-stored cost_cents vs token × rate. The recomputation
        // says ~$7.50 for the four base components, but we persist
        // only $5.00 at ingest time — `other_cost` should clamp at 0
        // rather than flip negative.
        conn.execute(
            "INSERT INTO messages (id, role, timestamp, model, input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens, cost_cents_ingested, cost_cents_effective)
             VALUES (?1, 'assistant', '2026-03-21T00:00:00Z', 'claude-opus-4-6', ?2, ?3, 0, 0, ?4, ?4)",
            params!["msg1", 1_000_000i64, 100_000i64, 500.0],
        ).unwrap();
        let cost = estimate_cost_filtered(&conn, None, None, None).unwrap();
        assert_eq!(cost.total_cost, 5.0);
        assert_eq!(cost.other_cost, 0.0);
    }
}
