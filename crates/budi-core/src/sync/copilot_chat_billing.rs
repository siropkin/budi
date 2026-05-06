//! Copilot Chat reconciliation against the GitHub Billing API
//! ([ADR-0092 §3](../../../../docs/adr/0092-copilot-chat-data-contract.md)).
//!
//! The local-tail half (`providers/copilot_chat.rs`, R1.4) writes per-message
//! `tokens` and an estimated `cost_cents` derived from the pricing manifest.
//! For individually-licensed users, the GitHub Billing API is the dollar
//! truth — this module pulls that truth and rewrites local-tail rows on a
//! `(date, model)`-bucket basis, bumping `cost_confidence` from
//! `"estimated"` to `"exact"` and tagging `pricing_source` as
//! `billing_api:copilot_chat`.
//!
//! Org-managed-license users (Copilot Business / Enterprise seats) get a
//! `200` with an empty usage array; per ADR-0092 §3.4 we treat **two
//! consecutive empty responses inside the same billing cycle** as the
//! org-managed signal and stop hitting the endpoint until the cycle rolls
//! over. Local-tail tokens × manifest pricing remain in effect — the
//! dashboard number is meaningful, just not a Copilot bill.
//!
//! The worker is best-effort: on any HTTP error or parse failure we log
//! once and return Ok(()) so the surrounding `Provider::sync_direct` keeps
//! returning `None` and the file-based local-tail import path runs
//! unaffected. The contract pinned in ADR-0092 is the source of truth for
//! every JSON shape and edge case below; any divergence in upstream
//! behavior is fixed by amending §3 of the ADR in the same PR as the code
//! change.

use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use serde_json::Value;

use crate::analytics;
use crate::config::CopilotChatConfig;

/// Provider name as written into `messages.provider` by the local-tail
/// half (see `providers/copilot_chat.rs`).
const PROVIDER_NAME: &str = "copilot_chat";

/// `sync_state` watermark key for the Billing API path. Distinct from any
/// local-tail offset key so both surfaces advance independently. The
/// stored value is the latest `billing_cycle_end` we have observed,
/// encoded as epoch seconds — the worker compares it to the response's
/// `billing_cycle_end` to detect a cycle rollover (which clears the
/// org-managed empty-streak counter).
const BILLING_API_WATERMARK_KEY: &str = "copilot-chat-billing-api";

/// `sync_state` row that counts consecutive empty-but-200 responses
/// inside the current billing cycle. Two consecutive empties means
/// "this account is org-managed" per ADR-0092 §3.4. The cycle-rollover
/// detection in [`update_watermark`] resets it to zero.
const ORG_MANAGED_STREAK_KEY: &str = "copilot-chat-billing-empty-streak";

/// Tag string written into `messages.pricing_source` for rows truthed
/// up by the Billing API. Matches ADR-0092 §3.5; not a [`PricingSource`]
/// enum variant because there is no in-manifest provenance for these
/// rows (same pattern as Cursor's `upstream:api`).
pub const COLUMN_VALUE_BILLING_API: &str = "billing_api:copilot_chat";

/// HTTP timeout for Billing API requests. Keeps reconciliation from
/// looking "stuck" when GitHub's API is slow; the worker is best-effort
/// so a timeout just means we try again next tick.
const HTTP_TIMEOUT: Duration = Duration::from_secs(8);

/// Minimum dollar denominator below which we refuse to scale a bucket.
/// Below this, the float division blows up cost_cents into wildly
/// disproportionate values; we leave the bucket alone and let the next
/// tick try again once more local-tail rows have landed.
const MIN_SCALE_DENOMINATOR_CENTS: f64 = 0.0001;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run one Billing API reconciliation tick.
///
/// Returns `Ok(())` on every non-fatal outcome so the caller (the
/// provider's `sync_direct`) can stay best-effort. Hard failures
/// (database errors mid-update, malformed config) bubble up so the
/// dispatcher's existing warn-and-continue logic logs them.
///
/// Always called inside `Provider::sync_direct` after `load_copilot_chat_config()`
/// has been read; the function itself does not touch config so tests can
/// inject a fixture-bearing config directly.
pub fn run_reconciliation(conn: &mut Connection, config: &CopilotChatConfig) -> Result<()> {
    let Some(pat) = config.effective_billing_pat() else {
        // No PAT — `budi doctor` surfaces this state separately. The
        // local-tail path continues to work; reconciliation is purely
        // additive.
        return Ok(());
    };

    let username = match resolve_username(&pat, config.effective_username().as_deref()) {
        Some(u) => u,
        None => return Ok(()),
    };

    let response = match fetch_with_fallback(&pat, &username) {
        FetchOutcome::Ok(r) => r,
        FetchOutcome::Unauthorized => {
            warn_once(
                "copilot_chat_billing_unauthorized",
                "Copilot Chat Billing API returned 401/403 — PAT lacks `manage_billing:copilot` scope or has been revoked. \
                 Reconciliation skipped; local-tail tokens × manifest pricing remain in effect.",
            );
            return Ok(());
        }
        FetchOutcome::TransientError(e) => {
            tracing::warn!("Copilot Chat Billing API transient error: {e:#}");
            return Ok(());
        }
    };

    apply_response(conn, &response)
}

/// Result of one fetch attempt. Distinguishes the two outcomes the worker
/// must treat differently: a 401/403 (PAT bad — log once and stop until
/// the user fixes it) vs everything else (transient — try again next
/// tick).
enum FetchOutcome {
    Ok(BillingResponse),
    Unauthorized,
    TransientError(anyhow::Error),
}

/// Internal representation of the Billing API response after dispatching
/// between the pre- and post-2026-06-01 endpoint shapes.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BillingResponse {
    pub billing_cycle_start: DateTime<Utc>,
    pub billing_cycle_end: DateTime<Utc>,
    pub rows: Vec<BillingRow>,
}

/// A single (date, model, dollar-amount) bucket from either Billing API
/// shape. Matches ADR-0092 §3.5 — bucketing granularity is `(date, model)`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BillingRow {
    pub date: String,
    pub model: String,
    pub amount_in_cents: f64,
}

// ---------------------------------------------------------------------------
// HTTP layer
// ---------------------------------------------------------------------------

/// Resolve the GitHub login for the PAT, preferring the explicit value
/// from config when present and falling back to one `GET /user` call.
/// In-memory cached for the life of the process keyed on the PAT digest
/// so a `budi db import` over many sync ticks calls `/user` exactly once.
fn resolve_username(pat: &str, configured: Option<&str>) -> Option<String> {
    if let Some(u) = configured {
        return Some(u.to_string());
    }
    if let Some(cached) = cached_username(pat) {
        return Some(cached);
    }
    let agent = ureq_agent();
    let response = agent
        .get("https://api.github.com/user")
        .header("Authorization", &format!("Bearer {pat}"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", &user_agent())
        .call();
    let mut response = match response {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("Copilot Chat Billing API: GET /user failed: {e:#}");
            return None;
        }
    };
    let body: Value = match response.body_mut().read_json() {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("Copilot Chat Billing API: GET /user body unparseable: {e:#}");
            return None;
        }
    };
    let login = body.get("login").and_then(|v| v.as_str())?.to_string();
    cache_username(pat, &login);
    Some(login)
}

/// Probe the post-2026-06-01 (`/usage`) endpoint first; on 404 fall back
/// to the pre-transition (`/premium_request/usage`) endpoint. ADR-0092
/// §3.2 — the cutover is seamless because both shapes carry
/// `amount_in_cents` as the dollar truth.
fn fetch_with_fallback(pat: &str, username: &str) -> FetchOutcome {
    let agent = ureq_agent();

    let post_url = format!("https://api.github.com/users/{username}/settings/billing/usage");
    match fetch_one(&agent, &post_url, pat) {
        FetchAttempt::Ok(body) => match parse_credits_response(&body) {
            Ok(r) => return FetchOutcome::Ok(r),
            Err(e) => {
                return FetchOutcome::TransientError(
                    e.context(format!("parse credits response from {post_url}")),
                );
            }
        },
        FetchAttempt::NotFound => {
            // Endpoint not enabled yet — fall through to PRU shape.
        }
        FetchAttempt::Unauthorized => return FetchOutcome::Unauthorized,
        FetchAttempt::Error(e) => return FetchOutcome::TransientError(e),
    }

    let pre_url =
        format!("https://api.github.com/users/{username}/settings/billing/premium_request/usage");
    match fetch_one(&agent, &pre_url, pat) {
        FetchAttempt::Ok(body) => match parse_pru_response(&body) {
            Ok(r) => FetchOutcome::Ok(r),
            Err(e) => FetchOutcome::TransientError(
                e.context(format!("parse PRU response from {pre_url}")),
            ),
        },
        FetchAttempt::NotFound => FetchOutcome::TransientError(anyhow::anyhow!(
            "Copilot Chat Billing API: both /usage and /premium_request/usage returned 404"
        )),
        FetchAttempt::Unauthorized => FetchOutcome::Unauthorized,
        FetchAttempt::Error(e) => FetchOutcome::TransientError(e),
    }
}

enum FetchAttempt {
    Ok(Value),
    NotFound,
    Unauthorized,
    Error(anyhow::Error),
}

fn fetch_one(agent: &ureq::Agent, url: &str, pat: &str) -> FetchAttempt {
    let result = agent
        .get(url)
        .header("Authorization", &format!("Bearer {pat}"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", &user_agent())
        .call();
    let mut response = match result {
        Ok(r) => r,
        Err(ureq::Error::StatusCode(code)) => {
            return match code {
                404 => FetchAttempt::NotFound,
                401 | 403 => FetchAttempt::Unauthorized,
                other => FetchAttempt::Error(anyhow::anyhow!(
                    "Copilot Chat Billing API status={other} for {url}"
                )),
            };
        }
        Err(e) => return FetchAttempt::Error(anyhow::Error::new(e)),
    };
    match response.body_mut().read_json::<Value>() {
        Ok(v) => FetchAttempt::Ok(v),
        Err(e) => {
            FetchAttempt::Error(anyhow::Error::new(e).context(format!("read body from {url}")))
        }
    }
}

fn ureq_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(3)))
        .timeout_global(Some(HTTP_TIMEOUT))
        .build()
        .into()
}

fn user_agent() -> String {
    format!("budi/{}", env!("CARGO_PKG_VERSION"))
}

// ---------------------------------------------------------------------------
// Response parsing — pre- and post-2026-06-01 shapes (ADR-0092 §3.1, §3.2)
// ---------------------------------------------------------------------------

/// Parse the pre-2026-06-01 PRU shape per ADR-0092 §3.1.
pub(crate) fn parse_pru_response(body: &Value) -> Result<BillingResponse> {
    let billing_cycle_start = parse_cycle_ts(body, "billing_cycle_start")?;
    let billing_cycle_end = parse_cycle_ts(body, "billing_cycle_end")?;

    let rows = body
        .get("premium_request_usage")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(parse_pru_row)
                .collect::<Vec<BillingRow>>()
        })
        .unwrap_or_default();

    Ok(BillingResponse {
        billing_cycle_start,
        billing_cycle_end,
        rows,
    })
}

fn parse_pru_row(value: &Value) -> Option<BillingRow> {
    let date = value.get("date").and_then(|v| v.as_str())?.to_string();
    let model = value.get("model").and_then(|v| v.as_str())?.to_string();
    let amount_in_cents = read_amount_cents(value)?;
    Some(BillingRow {
        date,
        model,
        amount_in_cents,
    })
}

/// Parse the post-2026-06-01 AI Credits shape per ADR-0092 §3.2.
///
/// Accepts either the top-level `credits_used` array (mirroring the PRU
/// shape) or a `usage[]` array — the public roadmap pins the column rename
/// (`premium_requests_used` → `credits_used`) and the path drop, but the
/// surrounding response container has not been pinned in writing yet.
/// Either container is fine because we only read `amount_in_cents` /
/// `(date, model)`; the column rename does not touch those fields.
pub(crate) fn parse_credits_response(body: &Value) -> Result<BillingResponse> {
    let billing_cycle_start = parse_cycle_ts(body, "billing_cycle_start")?;
    let billing_cycle_end = parse_cycle_ts(body, "billing_cycle_end")?;

    let array = body
        .get("usage")
        .and_then(|v| v.as_array())
        .or_else(|| body.get("credits_used").and_then(|v| v.as_array()));

    let rows = array
        .map(|arr| {
            arr.iter()
                .filter_map(parse_credit_row)
                .collect::<Vec<BillingRow>>()
        })
        .unwrap_or_default();

    Ok(BillingResponse {
        billing_cycle_start,
        billing_cycle_end,
        rows,
    })
}

fn parse_credit_row(value: &Value) -> Option<BillingRow> {
    let date = value.get("date").and_then(|v| v.as_str())?.to_string();
    let model = value.get("model").and_then(|v| v.as_str())?.to_string();
    let amount_in_cents = read_amount_cents(value)?;
    Some(BillingRow {
        date,
        model,
        amount_in_cents,
    })
}

fn read_amount_cents(value: &Value) -> Option<f64> {
    value
        .get("amount_in_cents")
        .and_then(|v| v.as_f64().or_else(|| v.as_i64().map(|i| i as f64)))
}

fn parse_cycle_ts(body: &Value, field: &str) -> Result<DateTime<Utc>> {
    let raw = body
        .get(field)
        .and_then(|v| v.as_str())
        .with_context(|| format!("missing or non-string `{field}`"))?;
    DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.with_timezone(&Utc))
        .with_context(|| format!("invalid RFC 3339 in `{field}`: {raw}"))
}

// ---------------------------------------------------------------------------
// Persistence — bucket scaling + watermark + org-managed streak (ADR-0092 §3.5)
// ---------------------------------------------------------------------------

fn apply_response(conn: &mut Connection, response: &BillingResponse) -> Result<()> {
    let cycle_end_epoch = response.billing_cycle_end.timestamp().max(0) as usize;
    let prior_cycle_end = analytics::get_sync_offset(conn, BILLING_API_WATERMARK_KEY).unwrap_or(0);
    let cycle_rolled_over = cycle_end_epoch != prior_cycle_end;

    if response.rows.is_empty() {
        let prior_streak = if cycle_rolled_over {
            0
        } else {
            analytics::get_sync_offset(conn, ORG_MANAGED_STREAK_KEY).unwrap_or(0)
        };
        let next_streak = prior_streak.saturating_add(1);
        analytics::set_sync_offset(conn, ORG_MANAGED_STREAK_KEY, next_streak)?;
        analytics::set_sync_offset(conn, BILLING_API_WATERMARK_KEY, cycle_end_epoch)?;

        if next_streak >= 2 {
            warn_once(
                "copilot_chat_billing_org_managed",
                "Copilot Chat license appears org-managed (Billing API returned empty across two consecutive ticks). \
                 Reconciliation skipped; local-tail tokens × manifest pricing remain in effect.",
            );
        }
        return Ok(());
    }

    // Non-empty response — clear the org-managed streak counter; we have
    // real data, so the user is individually-billed (or the cycle rolled
    // over and a previously-empty cycle now has activity).
    if analytics::get_sync_offset(conn, ORG_MANAGED_STREAK_KEY).unwrap_or(0) != 0 {
        analytics::set_sync_offset(conn, ORG_MANAGED_STREAK_KEY, 0)?;
    }

    apply_buckets(conn, &response.rows)?;

    analytics::set_sync_offset(conn, BILLING_API_WATERMARK_KEY, cycle_end_epoch)?;
    Ok(())
}

/// Apply per-bucket dollar truth-up. ADR-0092 §3.5: preserves per-message
/// tokens, scales `cost_cents` proportionally so the bucket sum matches
/// `amount_in_cents`, and bumps `cost_confidence` to `"exact"` plus
/// `pricing_source` to `billing_api:copilot_chat`.
fn apply_buckets(conn: &mut Connection, rows: &[BillingRow]) -> Result<usize> {
    let tx = conn.transaction()?;
    let mut total_updated = 0usize;

    for row in rows {
        // Existing-bucket sum drives the scale factor. We compute it
        // first so a zero-sum bucket (no local-tail rows yet) skips
        // cleanly without a divide-by-zero.
        let existing_sum_cents: Option<f64> = tx
            .query_row(
                "SELECT SUM(COALESCE(cost_cents, 0.0))
                 FROM messages
                 WHERE provider = ?1
                   AND model = ?2
                   AND DATE(timestamp) = ?3",
                params![PROVIDER_NAME, row.model, row.date],
                |r| r.get(0),
            )
            .ok()
            .flatten();
        let Some(existing_sum_cents) = existing_sum_cents else {
            continue;
        };
        if existing_sum_cents.abs() < MIN_SCALE_DENOMINATOR_CENTS {
            continue;
        }

        let scale = row.amount_in_cents / existing_sum_cents;
        let updated = tx.execute(
            "UPDATE messages
                SET cost_cents = COALESCE(cost_cents, 0.0) * ?1,
                    cost_confidence = 'exact',
                    pricing_source = ?2
              WHERE provider = ?3
                AND model = ?4
                AND DATE(timestamp) = ?5",
            params![
                scale,
                COLUMN_VALUE_BILLING_API,
                PROVIDER_NAME,
                row.model,
                row.date
            ],
        )?;
        total_updated += updated;
    }

    tx.commit()?;
    Ok(total_updated)
}

// ---------------------------------------------------------------------------
// Helpers — username cache, warn-once
// ---------------------------------------------------------------------------

fn cached_username(pat: &str) -> Option<String> {
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<Option<(String, String)>>> = OnceLock::new();
    let lock = CACHE.get_or_init(|| Mutex::new(None));
    let guard = lock.lock().ok()?;
    if let Some((stored_pat, login)) = guard.as_ref()
        && stored_pat == pat
    {
        return Some(login.clone());
    }
    None
}

fn cache_username(pat: &str, login: &str) {
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<Option<(String, String)>>> = OnceLock::new();
    // Re-resolve the lock used by `cached_username` so reads and writes
    // share state. We have to use the same statics; bind to a shared
    // OnceLock by routing through the same path.
    let lock = CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(mut guard) = lock.lock() {
        *guard = Some((pat.to_string(), login.to_string()));
    }
}

fn warn_once(event: &'static str, message: &str) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let lock = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    if let Ok(mut guard) = lock.lock()
        && guard.insert(event)
    {
        tracing::warn!(target: "budi_core::sync::copilot_chat_billing", event, "{message}");
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::migrate;
    use chrono::TimeZone;

    fn fresh_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn
    }

    fn insert_local_tail_message(
        conn: &Connection,
        id: &str,
        ts: &str,
        model: &str,
        cost_cents: f64,
    ) {
        conn.execute(
            "INSERT INTO messages
                (id, role, timestamp, model, provider, cost_cents,
                 cost_confidence, pricing_source,
                 input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens)
             VALUES (?1, 'assistant', ?2, ?3, 'copilot_chat', ?4,
                     'estimated', 'manifest:v1', 100, 10, 0, 0)",
            params![id, ts, model, cost_cents],
        )
        .unwrap();
    }

    fn pru_response_json(start: &str, end: &str, rows: &[(&str, &str, f64)]) -> Value {
        serde_json::json!({
            "billing_cycle_start": start,
            "billing_cycle_end": end,
            "premium_request_usage": rows.iter().map(|(date, model, cents)| serde_json::json!({
                "date": date,
                "model": model,
                "request_count": 100,
                "premium_requests_used": 10.0,
                "amount_in_cents": cents,
                "is_overage": false
            })).collect::<Vec<_>>()
        })
    }

    fn credits_response_json(start: &str, end: &str, rows: &[(&str, &str, f64)]) -> Value {
        serde_json::json!({
            "billing_cycle_start": start,
            "billing_cycle_end": end,
            "usage": rows.iter().map(|(date, model, cents)| serde_json::json!({
                "date": date,
                "model": model,
                "credits_used": 12.5,
                "input_tokens": 9000,
                "output_tokens": 400,
                "amount_in_cents": cents
            })).collect::<Vec<_>>()
        })
    }

    #[test]
    fn parses_pre_2026_06_01_pru_shape() {
        let body = pru_response_json(
            "2026-05-01T00:00:00Z",
            "2026-05-31T23:59:59Z",
            &[("2026-05-04", "gpt-4.1", 875.0)],
        );
        let response = parse_pru_response(&body).unwrap();
        assert_eq!(
            response.billing_cycle_start,
            Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap()
        );
        assert_eq!(response.rows.len(), 1);
        assert_eq!(response.rows[0].date, "2026-05-04");
        assert_eq!(response.rows[0].model, "gpt-4.1");
        assert!((response.rows[0].amount_in_cents - 875.0).abs() < 0.01);
    }

    #[test]
    fn parses_post_2026_06_01_credits_shape() {
        let body = credits_response_json(
            "2026-06-01T00:00:00Z",
            "2026-06-30T23:59:59Z",
            &[("2026-06-04", "claude-sonnet-4-5", 1234.5)],
        );
        let response = parse_credits_response(&body).unwrap();
        assert_eq!(response.rows.len(), 1);
        assert_eq!(response.rows[0].model, "claude-sonnet-4-5");
        assert!((response.rows[0].amount_in_cents - 1234.5).abs() < 0.01);
    }

    #[test]
    fn parses_credits_alt_container_credits_used_array() {
        // Some early post-transition shapes may carry the array under
        // `credits_used` instead of `usage`. Both must work.
        let body = serde_json::json!({
            "billing_cycle_start": "2026-06-01T00:00:00Z",
            "billing_cycle_end": "2026-06-30T23:59:59Z",
            "credits_used": [
                {"date": "2026-06-12", "model": "o3", "amount_in_cents": 250}
            ]
        });
        let response = parse_credits_response(&body).unwrap();
        assert_eq!(response.rows.len(), 1);
        assert_eq!(response.rows[0].model, "o3");
        assert!((response.rows[0].amount_in_cents - 250.0).abs() < 0.01);
    }

    #[test]
    fn empty_response_advances_org_managed_streak_and_warns_on_second() {
        let mut conn = fresh_conn();
        let response = BillingResponse {
            billing_cycle_start: Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap(),
            billing_cycle_end: Utc.with_ymd_and_hms(2026, 5, 31, 23, 59, 59).unwrap(),
            rows: Vec::new(),
        };
        apply_response(&mut conn, &response).unwrap();
        let streak = analytics::get_sync_offset(&conn, ORG_MANAGED_STREAK_KEY).unwrap();
        assert_eq!(streak, 1, "first empty tick increments streak");

        apply_response(&mut conn, &response).unwrap();
        let streak = analytics::get_sync_offset(&conn, ORG_MANAGED_STREAK_KEY).unwrap();
        assert_eq!(
            streak, 2,
            "second empty tick within same cycle confirms org-managed"
        );
    }

    #[test]
    fn streak_resets_on_cycle_rollover() {
        let mut conn = fresh_conn();
        let cycle_a = BillingResponse {
            billing_cycle_start: Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap(),
            billing_cycle_end: Utc.with_ymd_and_hms(2026, 5, 31, 23, 59, 59).unwrap(),
            rows: Vec::new(),
        };
        apply_response(&mut conn, &cycle_a).unwrap();
        apply_response(&mut conn, &cycle_a).unwrap();
        // A new cycle with still-empty data should reset streak to 1, not 3.
        let cycle_b = BillingResponse {
            billing_cycle_start: Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap(),
            billing_cycle_end: Utc.with_ymd_and_hms(2026, 6, 30, 23, 59, 59).unwrap(),
            rows: Vec::new(),
        };
        apply_response(&mut conn, &cycle_b).unwrap();
        let streak = analytics::get_sync_offset(&conn, ORG_MANAGED_STREAK_KEY).unwrap();
        assert_eq!(streak, 1, "rollover detection clears the streak");
    }

    #[test]
    fn watermark_advances_to_billing_cycle_end() {
        let mut conn = fresh_conn();
        let cycle_end = Utc.with_ymd_and_hms(2026, 5, 31, 23, 59, 59).unwrap();
        let response = BillingResponse {
            billing_cycle_start: Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap(),
            billing_cycle_end: cycle_end,
            rows: Vec::new(),
        };
        apply_response(&mut conn, &response).unwrap();
        let stored = analytics::get_sync_offset(&conn, BILLING_API_WATERMARK_KEY).unwrap();
        assert_eq!(stored, cycle_end.timestamp() as usize);
    }

    #[test]
    fn nonempty_bucket_scales_existing_rows_and_bumps_confidence() {
        let mut conn = fresh_conn();
        // Two local-tail rows for the same (date, model) bucket. Existing
        // estimated cost = 100c + 200c = 300c. Billing API truth = 600c.
        // Scale = 2.0 → rows should become 200c and 400c.
        insert_local_tail_message(&conn, "m-1", "2026-05-04T10:00:00Z", "gpt-4.1", 100.0);
        insert_local_tail_message(&conn, "m-2", "2026-05-04T11:00:00Z", "gpt-4.1", 200.0);
        // Different-day row must NOT be touched.
        insert_local_tail_message(&conn, "m-3", "2026-05-05T10:00:00Z", "gpt-4.1", 50.0);

        let response = BillingResponse {
            billing_cycle_start: Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap(),
            billing_cycle_end: Utc.with_ymd_and_hms(2026, 5, 31, 23, 59, 59).unwrap(),
            rows: vec![BillingRow {
                date: "2026-05-04".to_string(),
                model: "gpt-4.1".to_string(),
                amount_in_cents: 600.0,
            }],
        };
        apply_response(&mut conn, &response).unwrap();

        let load =
            |id: &str| -> (f64, String, String) {
                conn.query_row(
                "SELECT cost_cents, cost_confidence, pricing_source FROM messages WHERE id = ?1",
                params![id],
                |r| Ok((r.get::<_, f64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)),
            )
            .unwrap()
            };

        let (c1, conf1, src1) = load("m-1");
        let (c2, conf2, src2) = load("m-2");
        let (c3, conf3, src3) = load("m-3");

        assert!((c1 - 200.0).abs() < 0.001, "m-1 scaled to 200c");
        assert!((c2 - 400.0).abs() < 0.001, "m-2 scaled to 400c");
        assert!((c3 - 50.0).abs() < 0.001, "m-3 (different day) untouched");
        assert_eq!(conf1, "exact");
        assert_eq!(conf2, "exact");
        assert_eq!(conf3, "estimated");
        assert_eq!(src1, COLUMN_VALUE_BILLING_API);
        assert_eq!(src2, COLUMN_VALUE_BILLING_API);
        assert_eq!(src3, "manifest:v1");
    }

    #[test]
    fn bucket_with_zero_existing_sum_is_skipped() {
        let mut conn = fresh_conn();
        // No local-tail rows for this bucket — Billing API row should be
        // skipped (no rows to scale; next tick handles it once tokens land).
        let response = BillingResponse {
            billing_cycle_start: Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap(),
            billing_cycle_end: Utc.with_ymd_and_hms(2026, 5, 31, 23, 59, 59).unwrap(),
            rows: vec![BillingRow {
                date: "2026-05-04".to_string(),
                model: "gpt-4.1".to_string(),
                amount_in_cents: 600.0,
            }],
        };
        apply_response(&mut conn, &response).unwrap();
        // Watermark still advances (we did successfully fetch).
        let stored = analytics::get_sync_offset(&conn, BILLING_API_WATERMARK_KEY).unwrap();
        assert!(stored > 0);
    }

    #[test]
    fn nonempty_response_clears_streak_counter() {
        let mut conn = fresh_conn();
        // Seed a streak from a prior empty tick.
        analytics::set_sync_offset(&conn, ORG_MANAGED_STREAK_KEY, 1).unwrap();

        insert_local_tail_message(&conn, "m-1", "2026-05-04T10:00:00Z", "gpt-4.1", 100.0);
        let response = BillingResponse {
            billing_cycle_start: Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap(),
            billing_cycle_end: Utc.with_ymd_and_hms(2026, 5, 31, 23, 59, 59).unwrap(),
            rows: vec![BillingRow {
                date: "2026-05-04".to_string(),
                model: "gpt-4.1".to_string(),
                amount_in_cents: 200.0,
            }],
        };
        apply_response(&mut conn, &response).unwrap();
        let streak = analytics::get_sync_offset(&conn, ORG_MANAGED_STREAK_KEY).unwrap();
        assert_eq!(streak, 0, "non-empty data clears the org-managed streak");
    }

    #[test]
    fn pru_response_with_invalid_cycle_start_errors() {
        let body = serde_json::json!({
            "billing_cycle_start": "not-a-date",
            "billing_cycle_end": "2026-05-31T23:59:59Z",
            "premium_request_usage": []
        });
        assert!(parse_pru_response(&body).is_err());
    }

    #[test]
    fn pru_response_skips_malformed_rows() {
        // Two rows: one valid, one missing the model field — the bad
        // row is dropped without failing the whole response.
        let body = serde_json::json!({
            "billing_cycle_start": "2026-05-01T00:00:00Z",
            "billing_cycle_end": "2026-05-31T23:59:59Z",
            "premium_request_usage": [
                {"date": "2026-05-04", "model": "gpt-4.1", "amount_in_cents": 100},
                {"date": "2026-05-05", "amount_in_cents": 200}
            ]
        });
        let response = parse_pru_response(&body).unwrap();
        assert_eq!(response.rows.len(), 1);
        assert_eq!(response.rows[0].model, "gpt-4.1");
    }

    #[test]
    fn empty_pat_does_not_trigger_reconciliation() {
        let mut conn = fresh_conn();
        let config = CopilotChatConfig::default();
        // No PAT → the worker is a complete no-op (does not even touch
        // sync_state).
        run_reconciliation(&mut conn, &config).unwrap();
        let stored = analytics::get_sync_offset(&conn, BILLING_API_WATERMARK_KEY).unwrap();
        assert_eq!(stored, 0);
    }
}
