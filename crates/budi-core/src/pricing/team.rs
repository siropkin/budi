//! Team pricing — org-negotiated rates applied to `messages.cost_cents_effective`.
//!
//! See [ADR-0094] §6 (cloud → local pull) and §7 (recalculation semantics).
//!
//! At runtime an in-process `Option<TeamPricing>` slot holds the active list.
//! [`install`] hot-swaps it; [`snapshot`] reads it (`RwLock`-guarded so a
//! recompute can run while CLI calls inspect state).
//!
//! When no list is active the slot holds `None`, and [`recompute_messages`]
//! resets `_effective := _ingested` for any disagreeing row — the
//! no-cloud-config path is a no-op because the column already mirrors
//! `_ingested` from the ingest writer.
//!
//! [ADR-0094]: https://github.com/siropkin/budi/blob/main/docs/adr/0094-custom-team-pricing-and-effective-cost-recalculation.md

use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

/// One row of the active org price list. Mirrors the on-the-wire shape from
/// `GET /v1/pricing/active`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TeamPricingRow {
    pub platform: String,
    pub model_pattern: String,
    pub region: String,
    /// `input` | `output` | `cache_read` | `cache_write`.
    pub token_type: String,
    pub sale_usd_per_mtok: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_usd_per_mtok: Option<f64>,
}

/// Defaults the org declared for dimensions the local envelope doesn't
/// carry yet (ADR-0094 §4).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TeamPricingDefaults {
    pub platform: String,
    pub region: String,
}

/// Full active price list. Returned by `GET /v1/pricing/active` and cached
/// at [`cache_path`] for warm-start after a daemon restart.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TeamPricing {
    pub org_id: String,
    pub list_version: u32,
    pub effective_from: String,
    #[serde(default)]
    pub effective_to: Option<String>,
    pub defaults: TeamPricingDefaults,
    pub rows: Vec<TeamPricingRow>,
    #[serde(default)]
    pub generated_at: Option<String>,
}

/// Per-token-type sale rates resolved for one `(model, provider, region)`
/// triple. Unit: USD per million tokens.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TeamRates {
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
}

impl TeamPricing {
    /// Resolve a `(model, provider, region)` triple against the active list.
    ///
    /// Returns `None` when no row in the list matches the model — the caller
    /// keeps `cost_cents_effective := cost_cents_ingested` for that row,
    /// matching the ADR-0094 §7 `coalesce` semantics.
    ///
    /// `region` is the row's region when known; `None` means "use the org
    /// default" (the v1 path, since the ingest envelope doesn't carry a
    /// region today — ADR-0094 §4).
    pub fn resolve(&self, model: &str, _provider: &str, region: Option<&str>) -> Option<TeamRates> {
        let effective_region = region.unwrap_or(self.defaults.region.as_str());
        let mut rates = TeamRates::default();
        let mut hit = false;
        for row in &self.rows {
            if !region_matches(&row.region, effective_region) {
                continue;
            }
            if !model_matches(&row.model_pattern, model) {
                continue;
            }
            hit = true;
            let r = Some(row.sale_usd_per_mtok);
            match row.token_type.as_str() {
                "input" => rates.input = r,
                "output" => rates.output = r,
                "cache_read" => rates.cache_read = r,
                "cache_write" => rates.cache_write = r,
                _ => {}
            }
        }
        if hit { Some(rates) } else { None }
    }
}

fn region_matches(row_region: &str, query: &str) -> bool {
    row_region == "*" || row_region.is_empty() || row_region.eq_ignore_ascii_case(query)
}

/// Trailing-`*` glob match. Mirrors the cloud-side resolver shape; richer
/// patterns are an explicit non-goal for v1 (the price-list CSV already uses
/// model-id prefixes).
fn model_matches(pattern: &str, model: &str) -> bool {
    if pattern == model {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return model.starts_with(prefix);
    }
    false
}

// ---------------------------------------------------------------------------
// Process-global active state
// ---------------------------------------------------------------------------

static ACTIVE: OnceLock<RwLock<Option<TeamPricing>>> = OnceLock::new();

fn slot() -> &'static RwLock<Option<TeamPricing>> {
    ACTIVE.get_or_init(|| RwLock::new(None))
}

/// Hot-swap the active team pricing. `None` clears the slot — the daemon
/// uses this for the 404 path (no active list for this org) and on auth
/// failure during the first poll.
pub fn install(pricing: Option<TeamPricing>) {
    let mut g = slot().write().expect("team pricing RwLock poisoned");
    *g = pricing;
}

/// Snapshot the active team pricing for read-only inspection (CLI, statusline).
pub fn snapshot() -> Option<TeamPricing> {
    slot().read().expect("team pricing RwLock poisoned").clone()
}

// ---------------------------------------------------------------------------
// On-disk cache
// ---------------------------------------------------------------------------

/// `~/.local/share/budi/team-pricing.json` on Linux/macOS,
/// `%LOCALAPPDATA%\budi\team-pricing.json` on Windows. Mirrors the
/// LiteLLM manifest cache layout.
pub fn cache_path() -> Result<PathBuf> {
    Ok(crate::config::budi_home_dir()?.join("team-pricing.json"))
}

pub fn load_cache(path: &Path) -> Result<Option<TeamPricing>> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let p: TeamPricing = serde_json::from_slice(&bytes).context("parse team-pricing cache")?;
    Ok(Some(p))
}

pub fn write_cache(path: &Path, pricing: &TeamPricing) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(pricing)?;
    std::fs::write(&tmp, &bytes).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

pub fn clear_cache(path: &Path) -> Result<()> {
    if path.exists() {
        std::fs::remove_file(path).with_context(|| format!("remove {}", path.display()))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Recompute
// ---------------------------------------------------------------------------

/// Outcome of one [`recompute_messages`] pass. Persisted to
/// `recalculation_runs_local` by the daemon worker and surfaced by
/// `budi pricing status` (#732).
#[derive(Debug, Clone, Default, Serialize)]
pub struct RecomputeSummary {
    pub list_version: u32,
    pub rows_processed: i64,
    pub rows_changed: i64,
    pub before_total_cents: f64,
    pub after_total_cents: f64,
}

/// One audit row from `recalculation_runs_local`. Same field shape as
/// `RecomputeSummary` plus the `started_at`/`finished_at` wall-clock
/// timestamps the daemon stamps at insert time.
#[derive(Debug, Clone, Serialize)]
pub struct RecomputeAuditRow {
    pub started_at: String,
    pub finished_at: String,
    pub list_version: u32,
    pub rows_processed: i64,
    pub rows_changed: i64,
    pub before_total_cents: f64,
    pub after_total_cents: f64,
}

/// Daemon-side snapshot of the team-pricing layer surfaced by
/// `GET /pricing/status` (under `team_pricing`) and rendered by
/// `budi pricing status` (#732).
///
/// `active = false` is the no-cloud-config and no-active-list cases —
/// every other field is `None`. The CLI uses that single bit to decide
/// whether to print the section at all.
#[derive(Debug, Clone, Serialize)]
pub struct TeamPricingStatus {
    pub active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub org_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list_version: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub defaults: Option<TeamPricingDefaults>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_recompute: Option<RecomputeAuditRow>,
    /// Sum of `cost_cents_ingested - cost_cents_effective` over the last
    /// 30 days (positive numbers mean the org saved money vs list prices).
    /// `None` when no recompute has ever run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub savings_last_30d_cents: Option<f64>,
}

impl TeamPricingStatus {
    pub fn inactive() -> Self {
        Self {
            active: false,
            org_id: None,
            list_version: None,
            effective_from: None,
            effective_to: None,
            defaults: None,
            last_recompute: None,
            savings_last_30d_cents: None,
        }
    }
}

/// Build the team-pricing status payload from the in-memory snapshot +
/// the local audit table. `conn` is a read-only handle on the analytics
/// DB; this function never writes.
///
/// Returns `inactive()` when no list is installed, mirroring the spec's
/// "Team pricing (cloud): not active" rendering.
pub fn build_status(conn: &Connection) -> Result<TeamPricingStatus> {
    let snap = snapshot();
    let last = latest_audit_row(conn)?;
    let savings = savings_last_30d_cents(conn)?;

    let Some(pricing) = snap else {
        // No active list — but if a previous run recorded an audit row,
        // surface its savings figure so the operator still sees the
        // historical delta.
        return Ok(TeamPricingStatus {
            active: false,
            last_recompute: last,
            savings_last_30d_cents: savings,
            ..TeamPricingStatus::inactive()
        });
    };

    Ok(TeamPricingStatus {
        active: true,
        org_id: Some(pricing.org_id.clone()),
        list_version: Some(pricing.list_version),
        effective_from: Some(pricing.effective_from.clone()),
        effective_to: pricing.effective_to.clone(),
        defaults: Some(pricing.defaults.clone()),
        last_recompute: last,
        savings_last_30d_cents: savings,
    })
}

fn latest_audit_row(conn: &Connection) -> Result<Option<RecomputeAuditRow>> {
    let mut stmt = conn.prepare(
        "SELECT started_at, finished_at, list_version,
                rows_processed, rows_changed,
                before_total_cents, after_total_cents
           FROM recalculation_runs_local
          ORDER BY id DESC
          LIMIT 1",
    )?;
    let mut rows = stmt.query([])?;
    if let Some(r) = rows.next()? {
        Ok(Some(RecomputeAuditRow {
            started_at: r.get(0)?,
            finished_at: r.get(1)?,
            list_version: {
                let v: i64 = r.get(2)?;
                v.max(0) as u32
            },
            rows_processed: r.get(3)?,
            rows_changed: r.get(4)?,
            before_total_cents: r.get(5)?,
            after_total_cents: r.get(6)?,
        }))
    } else {
        Ok(None)
    }
}

fn savings_last_30d_cents(conn: &Connection) -> Result<Option<f64>> {
    let cutoff = (chrono::Utc::now() - chrono::Duration::days(30)).to_rfc3339();
    let savings: f64 = conn.query_row(
        "SELECT COALESCE(SUM(cost_cents_ingested - cost_cents_effective), 0.0)
           FROM messages
          WHERE timestamp >= ?1",
        rusqlite::params![cutoff],
        |r| r.get(0),
    )?;
    Ok(Some(savings))
}

/// Recompute `messages.cost_cents_effective` from `pricing`. Passing `None`
/// resets `_effective := _ingested` everywhere — used when the cloud
/// returns 404 (no active list) after a previous tick had installed one.
///
/// The existing `trg_messages_rollup_update` cascades the per-row delta into
/// `message_rollups_hourly` / `message_rollups_daily` so downstream reads
/// stay coherent without a second pass.
pub fn recompute_messages(
    conn: &Connection,
    pricing: Option<&TeamPricing>,
) -> Result<RecomputeSummary> {
    let before_total: f64 = conn.query_row(
        "SELECT COALESCE(SUM(cost_cents_effective), 0.0) FROM messages",
        [],
        |r| r.get(0),
    )?;
    let rows_processed: i64 = conn.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?;

    let mut summary = RecomputeSummary {
        list_version: pricing.map(|p| p.list_version).unwrap_or(0),
        rows_processed,
        rows_changed: 0,
        before_total_cents: before_total,
        after_total_cents: before_total,
    };

    let Some(pricing) = pricing else {
        let changed = conn.execute(
            "UPDATE messages
                SET cost_cents_effective = cost_cents_ingested
              WHERE cost_cents_effective != cost_cents_ingested",
            [],
        )?;
        summary.rows_changed = changed as i64;
        summary.after_total_cents = conn.query_row(
            "SELECT COALESCE(SUM(cost_cents_effective), 0.0) FROM messages",
            [],
            |r| r.get(0),
        )?;
        return Ok(summary);
    };

    // Pull the per-row inputs into Rust, compute the new effective, then
    // write back inside one transaction. A SQL-only UPDATE would need to
    // express the pattern-glob model resolver in SQLite, which is far less
    // readable than this 30-line loop and pulls no additional dependencies.
    let mut stmt = conn.prepare(
        "SELECT id, model, provider,
                input_tokens, output_tokens,
                cache_creation_tokens, cache_read_tokens,
                cost_cents_ingested, cost_cents_effective
           FROM messages",
    )?;
    let mut updates: Vec<(String, f64)> = Vec::new();
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let id: String = r.get(0)?;
        let model: Option<String> = r.get(1)?;
        let provider: Option<String> = r.get(2)?;
        let input_t: i64 = r.get(3)?;
        let output_t: i64 = r.get(4)?;
        let cache_creation_t: i64 = r.get(5)?;
        let cache_read_t: i64 = r.get(6)?;
        let ingested: f64 = r.get(7)?;
        let current_eff: f64 = r.get(8)?;

        let new_eff = if let Some(model_str) = model.as_deref() {
            let provider_str = provider.as_deref().unwrap_or("");
            match pricing.resolve(model_str, provider_str, None) {
                Some(rates) => compute_cost_cents(
                    &rates,
                    input_t,
                    output_t,
                    cache_creation_t,
                    cache_read_t,
                    ingested,
                ),
                None => ingested,
            }
        } else {
            ingested
        };

        if (new_eff - current_eff).abs() > 1e-9 {
            updates.push((id, new_eff));
        }
    }
    drop(rows);
    drop(stmt);

    let tx = conn.unchecked_transaction()?;
    {
        let mut update_stmt =
            tx.prepare("UPDATE messages SET cost_cents_effective = ?1 WHERE id = ?2")?;
        for (id, new_eff) in &updates {
            update_stmt.execute(rusqlite::params![new_eff, id])?;
        }
    }
    tx.commit()?;

    summary.rows_changed = updates.len() as i64;
    summary.after_total_cents = conn.query_row(
        "SELECT COALESCE(SUM(cost_cents_effective), 0.0) FROM messages",
        [],
        |r| r.get(0),
    )?;
    Ok(summary)
}

/// `tokens × rate / 1M` per token type, returned in cents (the storage
/// unit on `cost_cents_*`). When any token type has tokens but no rate in
/// the active list, fall back to the row's `cost_cents_ingested` so a
/// partial price list doesn't underprice rows — ADR-0094 §7 `coalesce`.
fn compute_cost_cents(
    rates: &TeamRates,
    input: i64,
    output: i64,
    cache_creation: i64,
    cache_read: i64,
    fallback_cents: f64,
) -> f64 {
    let mut dollars = 0.0;
    if input > 0 {
        match rates.input {
            Some(r) => dollars += (input as f64) * r / 1_000_000.0,
            None => return fallback_cents,
        }
    }
    if output > 0 {
        match rates.output {
            Some(r) => dollars += (output as f64) * r / 1_000_000.0,
            None => return fallback_cents,
        }
    }
    if cache_creation > 0 {
        match rates.cache_write {
            Some(r) => dollars += (cache_creation as f64) * r / 1_000_000.0,
            None => return fallback_cents,
        }
    }
    if cache_read > 0 {
        match rates.cache_read {
            Some(r) => dollars += (cache_read as f64) * r / 1_000_000.0,
            None => return fallback_cents,
        }
    }
    dollars * 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_pricing() -> TeamPricing {
        TeamPricing {
            org_id: "org_test".to_string(),
            list_version: 3,
            effective_from: "2026-04-01".to_string(),
            effective_to: None,
            defaults: TeamPricingDefaults {
                platform: "bedrock".to_string(),
                region: "us".to_string(),
            },
            rows: vec![
                TeamPricingRow {
                    platform: "bedrock".to_string(),
                    model_pattern: "claude-sonnet-4-5-*".to_string(),
                    region: "us".to_string(),
                    token_type: "input".to_string(),
                    sale_usd_per_mtok: 2.91,
                    list_usd_per_mtok: Some(3.00),
                },
                TeamPricingRow {
                    platform: "bedrock".to_string(),
                    model_pattern: "claude-sonnet-4-5-*".to_string(),
                    region: "us".to_string(),
                    token_type: "output".to_string(),
                    sale_usd_per_mtok: 14.55,
                    list_usd_per_mtok: Some(15.00),
                },
            ],
            generated_at: Some("2026-05-11T18:14:02Z".to_string()),
        }
    }

    #[test]
    fn resolve_matches_glob_pattern_and_returns_rates() {
        let p = sample_pricing();
        let rates = p
            .resolve("claude-sonnet-4-5-20251002", "claude_code", None)
            .expect("model should match the glob row");
        assert_eq!(rates.input, Some(2.91));
        assert_eq!(rates.output, Some(14.55));
        assert_eq!(rates.cache_read, None);
        assert_eq!(rates.cache_write, None);
    }

    #[test]
    fn resolve_misses_unrelated_model() {
        let p = sample_pricing();
        assert!(p.resolve("gpt-5", "openai", None).is_none());
    }

    #[test]
    fn compute_cost_falls_back_when_a_token_type_is_unpriced() {
        let rates = TeamRates {
            input: Some(2.91),
            // No output rate, but row has output tokens → fall back to ingested.
            output: None,
            cache_read: None,
            cache_write: None,
        };
        let result = compute_cost_cents(&rates, 1000, 1000, 0, 0, 99.0);
        assert_eq!(result, 99.0);
    }

    #[test]
    fn compute_cost_in_cents_matches_dollars_x_100() {
        let rates = TeamRates {
            input: Some(3.00),
            output: Some(15.00),
            cache_read: None,
            cache_write: None,
        };
        // 1M input tokens * $3/MTok = $3.00 = 300 cents.
        // 1M output tokens * $15/MTok = $15.00 = 1500 cents.
        let result = compute_cost_cents(&rates, 1_000_000, 1_000_000, 0, 0, 0.0);
        assert!((result - 1800.0).abs() < 1e-6, "got {result}");
    }

    #[test]
    fn install_and_snapshot_roundtrip() {
        install(Some(sample_pricing()));
        let snap = snapshot().expect("snapshot should return installed pricing");
        assert_eq!(snap.list_version, 3);
        install(None);
        assert!(snapshot().is_none());
    }

    #[test]
    fn recompute_with_no_pricing_resets_effective_to_ingested() {
        let conn = crate::analytics::open_db_with_migration(std::path::Path::new(":memory:"))
            .expect("open db");
        // Seed two messages whose `_effective` deliberately disagrees with
        // `_ingested` (as if a previous list had been applied).
        conn.execute(
            "INSERT INTO messages (id, role, timestamp, model, provider,
                                   input_tokens, output_tokens,
                                   cost_cents_ingested, cost_cents_effective)
             VALUES ('m1','assistant','2026-05-11T00:00:00Z','claude-sonnet-4-5-x','claude_code',
                     1000, 2000, 5.0, 3.0),
                    ('m2','assistant','2026-05-11T00:01:00Z','gpt-5','openai',
                     0, 100, 4.0, 4.0)",
            [],
        )
        .expect("seed messages");

        let summary = recompute_messages(&conn, None).expect("recompute");
        assert_eq!(summary.rows_changed, 1, "only m1 needed reset");
        let m1_eff: f64 = conn
            .query_row(
                "SELECT cost_cents_effective FROM messages WHERE id='m1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(m1_eff, 5.0);
    }

    #[test]
    fn recompute_with_active_pricing_rewrites_effective() {
        let conn = crate::analytics::open_db_with_migration(std::path::Path::new(":memory:"))
            .expect("open db");
        conn.execute(
            "INSERT INTO messages (id, role, timestamp, model, provider,
                                   input_tokens, output_tokens,
                                   cost_cents_ingested, cost_cents_effective)
             VALUES ('m1','assistant','2026-05-11T00:00:00Z','claude-sonnet-4-5-x','claude_code',
                     1000000, 1000000, 100.0, 100.0)",
            [],
        )
        .expect("seed message");
        let summary =
            recompute_messages(&conn, Some(&sample_pricing())).expect("recompute with pricing");
        assert_eq!(summary.rows_changed, 1);
        let eff: f64 = conn
            .query_row("SELECT cost_cents_effective FROM messages", [], |r| {
                r.get(0)
            })
            .unwrap();
        // 1M input × $2.91/MTok + 1M output × $14.55/MTok = $17.46 = 1746 cents.
        assert!((eff - 1746.0).abs() < 1e-6, "got {eff}");
    }

    #[test]
    fn build_status_returns_inactive_when_no_list_installed() {
        install(None);
        let conn = crate::analytics::open_db_with_migration(std::path::Path::new(":memory:"))
            .expect("open db");
        let status = build_status(&conn).expect("build_status");
        assert!(!status.active);
        assert!(status.org_id.is_none());
        assert!(status.list_version.is_none());
        assert!(status.last_recompute.is_none());
        // No messages → savings is 0, not None.
        assert_eq!(status.savings_last_30d_cents, Some(0.0));
    }

    #[test]
    fn build_status_surfaces_active_list_and_latest_audit_row() {
        let conn = crate::analytics::open_db_with_migration(std::path::Path::new(":memory:"))
            .expect("open db");
        // Seed a message + an audit row.
        let recent_ts = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO messages (id, role, timestamp, model, provider,
                                   input_tokens, output_tokens,
                                   cost_cents_ingested, cost_cents_effective)
             VALUES ('m1','assistant',?1,'claude-sonnet-4-5-x','claude_code',
                     0, 0, 500.0, 425.0)",
            rusqlite::params![recent_ts],
        )
        .expect("seed message");
        conn.execute(
            "INSERT INTO recalculation_runs_local
                (started_at, finished_at, list_version,
                 rows_processed, rows_changed,
                 before_total_cents, after_total_cents)
             VALUES ('2026-05-11T11:14:02Z','2026-05-11T11:14:02Z',3,
                     12000, 2103, 481520.0, 352777.0)",
            [],
        )
        .expect("seed audit row");

        install(Some(sample_pricing()));
        let status = build_status(&conn).expect("build_status");
        install(None);

        assert!(status.active);
        assert_eq!(status.org_id.as_deref(), Some("org_test"));
        assert_eq!(status.list_version, Some(3));
        assert_eq!(status.effective_from.as_deref(), Some("2026-04-01"));
        let audit = status.last_recompute.expect("audit row present");
        assert_eq!(audit.list_version, 3);
        assert_eq!(audit.rows_changed, 2103);
        // savings = 500 - 425 = 75 cents.
        let savings = status.savings_last_30d_cents.expect("savings present");
        assert!((savings - 75.0).abs() < 1e-6, "got {savings}");
    }

    #[test]
    fn cache_roundtrip() {
        let path = std::env::temp_dir().join(format!(
            "budi-team-pricing-test-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let p = sample_pricing();
        write_cache(&path, &p).expect("write_cache");
        let loaded = load_cache(&path)
            .expect("load_cache")
            .expect("cache file should now exist");
        assert_eq!(loaded.list_version, p.list_version);
        assert_eq!(loaded.rows.len(), p.rows.len());
        clear_cache(&path).expect("clear_cache");
        assert!(load_cache(&path).expect("load_cache").is_none());
    }
}
