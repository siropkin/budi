//! Token cost estimation based on Claude model pricing.

use anyhow::Result;
use rusqlite::Connection;

/// Per-million-token pricing for a Claude model.
struct ModelPricing {
    input: f64,
    output: f64,
    cache_write: f64,
    cache_read: f64,
}

/// Look up pricing by model string (e.g. "claude-opus-4-6", "claude-sonnet-4-6-20260321").
fn pricing_for_model(model: &str) -> ModelPricing {
    let m = model.to_lowercase();
    if m.contains("opus-4-6") || m.contains("opus-4-5") {
        ModelPricing {
            input: 5.0,
            output: 25.0,
            cache_write: 6.25,
            cache_read: 0.50,
        }
    } else if m.contains("opus") {
        // opus-4-1, opus-4-0, older
        ModelPricing {
            input: 15.0,
            output: 75.0,
            cache_write: 18.75,
            cache_read: 1.50,
        }
    } else if m.contains("sonnet") {
        ModelPricing {
            input: 3.0,
            output: 15.0,
            cache_write: 3.75,
            cache_read: 0.30,
        }
    } else if m.contains("haiku") {
        ModelPricing {
            input: 1.0,
            output: 5.0,
            cache_write: 1.25,
            cache_read: 0.10,
        }
    } else {
        // Unknown model — use sonnet pricing as a reasonable default
        ModelPricing {
            input: 3.0,
            output: 15.0,
            cache_write: 3.75,
            cache_read: 0.30,
        }
    }
}

/// Estimated cost breakdown.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CostEstimate {
    pub total_cost: f64,
    pub input_cost: f64,
    pub output_cost: f64,
    pub cache_write_cost: f64,
    pub cache_read_cost: f64,
    pub cache_savings: f64,
}

/// Compute estimated cost from token usage grouped by model.
///
/// Queries the messages table for per-model token totals and applies pricing.
pub fn estimate_cost(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
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
    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    let sql = format!(
        "SELECT COALESCE(model, 'unknown'),
                COALESCE(SUM(input_tokens), 0),
                COALESCE(SUM(output_tokens), 0),
                COALESCE(SUM(cache_creation_tokens), 0),
                COALESCE(SUM(cache_read_tokens), 0)
         FROM messages {}
         GROUP BY COALESCE(model, 'unknown')",
        where_clause
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(param_refs.as_slice(), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, u64>(1)?,
            row.get::<_, u64>(2)?,
            row.get::<_, u64>(3)?,
            row.get::<_, u64>(4)?,
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
        let (model, input, output, cache_write, cache_read) = row?;
        let p = pricing_for_model(&model);
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

    total.total_cost =
        total.input_cost + total.output_cost + total.cache_write_cost + total.cache_read_cost;

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
                has_thinking INTEGER NOT NULL DEFAULT 0,
                stop_reason TEXT,
                text_length INTEGER NOT NULL DEFAULT 0,
                cwd TEXT
            );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn cost_empty_db() {
        let conn = setup_db();
        let cost = estimate_cost(&conn, None, None).unwrap();
        assert_eq!(cost.total_cost, 0.0);
    }

    #[test]
    fn cost_single_opus_message() {
        let conn = setup_db();
        conn.execute(
            "INSERT INTO messages (uuid, role, timestamp, model, input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens)
             VALUES (?1, 'assistant', '2026-03-21T00:00:00Z', 'claude-opus-4-6', ?2, ?3, ?4, ?5)",
            params!["msg1", 1_000_000i64, 100_000i64, 0i64, 0i64],
        ).unwrap();
        let cost = estimate_cost(&conn, None, None).unwrap();
        // 1M input * $5/M = $5.00, 100K output * $25/M = $2.50
        assert_eq!(cost.input_cost, 5.0);
        assert_eq!(cost.output_cost, 2.5);
        assert_eq!(cost.total_cost, 7.5);
    }

    #[test]
    fn cost_with_cache_savings() {
        let conn = setup_db();
        conn.execute(
            "INSERT INTO messages (uuid, role, timestamp, model, input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens)
             VALUES (?1, 'assistant', '2026-03-21T00:00:00Z', 'claude-sonnet-4-6-20260321', ?2, ?3, ?4, ?5)",
            params!["msg1", 100_000i64, 50_000i64, 200_000i64, 500_000i64],
        ).unwrap();
        let cost = estimate_cost(&conn, None, None).unwrap();
        // input: 100K * $3/M = $0.30
        // output: 50K * $15/M = $0.75
        // cache_write: 200K * $3.75/M = $0.75
        // cache_read: 500K * $0.30/M = $0.15
        // total = 0.30 + 0.75 + 0.75 + 0.15 = $1.95
        assert_eq!(cost.total_cost, 1.95);
        // savings: 500K * ($3.00 - $0.30) / 1M = $1.35
        assert_eq!(cost.cache_savings, 1.35);
    }

    #[test]
    fn cost_with_date_filter() {
        let conn = setup_db();
        conn.execute(
            "INSERT INTO messages (uuid, role, timestamp, model, input_tokens, output_tokens)
             VALUES ('old', 'assistant', '2026-03-01T00:00:00Z', 'claude-sonnet-4-6', 1000000, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (uuid, role, timestamp, model, input_tokens, output_tokens)
             VALUES ('new', 'assistant', '2026-03-21T00:00:00Z', 'claude-sonnet-4-6', 1000000, 0)",
            [],
        )
        .unwrap();
        let cost = estimate_cost(&conn, Some("2026-03-20"), None).unwrap();
        // Only the "new" message: 1M * $3/M = $3.00
        assert_eq!(cost.input_cost, 3.0);
        assert_eq!(cost.total_cost, 3.0);
    }
}
