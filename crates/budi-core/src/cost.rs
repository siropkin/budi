//! Token cost estimation dispatched through the provider trait.

use anyhow::Result;
use rusqlite::Connection;

use crate::provider::ModelPricing;

/// Look up pricing for a model using a specific provider's pricing table.
fn pricing_for_model_by_provider(model: &str, provider: Option<&str>) -> ModelPricing {
    match provider {
        Some("cursor") => crate::providers::cursor::cursor_pricing_for_model(model),
        _ => crate::providers::claude_code::claude_pricing_for_model(model),
    }
}

/// Estimated cost breakdown.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CostEstimate {
    pub total_cost: f64,
    pub input_cost: f64,
    pub output_cost: f64,
    pub cache_write_cost: f64,
    pub cache_read_cost: f64,
    pub cache_savings: f64,
}

/// Compute estimated cost with optional provider filter.
pub fn estimate_cost_filtered(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    provider: Option<&str>,
) -> Result<CostEstimate> {
    let mut conditions = Vec::new();
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
            "provider = ?{}",
            param_values.len()
        ));
    }
    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    // Use pre-computed SUM(cost_cents) for total_cost to match bar charts,
    // and token-based calculation for the input/output/cache breakdown.
    let sum_sql = format!(
        "SELECT COALESCE(SUM(cost_cents), 0) FROM messages {}",
        where_clause
    );
    let sum_cost_cents: f64 =
        conn.query_row(&sum_sql, param_refs.as_slice(), |r| r.get(0))?;

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
        cache_savings: 0.0,
    };

    for row in rows {
        let (prov, model, input, output, cache_write, cache_read) = row?;
        let p = pricing_for_model_by_provider(&model, Some(&prov));
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

    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE messages (
                uuid TEXT PRIMARY KEY,
                session_id TEXT,
                role TEXT NOT NULL,
                timestamp TEXT NOT NULL,
                model TEXT,
                input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
                cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                cost_cents REAL NOT NULL DEFAULT 0,
                cwd TEXT,
                provider TEXT DEFAULT 'claude_code'
            );",
        )
        .unwrap();
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
            "INSERT INTO messages (uuid, role, timestamp, model, input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens, cost_cents)
             VALUES (?1, 'assistant', '2026-03-21T00:00:00Z', 'claude-opus-4-6', ?2, ?3, ?4, ?5, ?6)",
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
            "INSERT INTO messages (uuid, role, timestamp, model, input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens, cost_cents)
             VALUES (?1, 'assistant', '2026-03-21T00:00:00Z', 'claude-sonnet-4-6-20260321', ?2, ?3, ?4, ?5, ?6)",
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
            "INSERT INTO messages (uuid, role, timestamp, model, input_tokens, output_tokens, cost_cents)
             VALUES ('old', 'assistant', '2026-03-01T00:00:00Z', 'claude-sonnet-4-6', 1000000, 0, 300.0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (uuid, role, timestamp, model, input_tokens, output_tokens, cost_cents)
             VALUES ('new', 'assistant', '2026-03-21T00:00:00Z', 'claude-sonnet-4-6', 1000000, 0, 300.0)",
            [],
        )
        .unwrap();
        let cost = estimate_cost_filtered(&conn, Some("2026-03-20"), None, None).unwrap();
        // Only the "new" message: 1M * $3/M = $3.00
        assert_eq!(cost.input_cost, 3.0);
        assert_eq!(cost.total_cost, 3.0);
    }
}
