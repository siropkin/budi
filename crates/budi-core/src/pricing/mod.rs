//! Pricing manifest — single source of truth for model cost calculation.
//!
//! See [ADR-0091] "Model Pricing via Embedded Baseline + LiteLLM Runtime
//! Refresh" for the governing design. This module replaces the four
//! hand-maintained `*_pricing_for_model()` functions that lived in
//! `providers/*.rs` through 8.2 — those were deleted in #377 and every
//! caller now goes through [`lookup`].
//!
//! # Lookup contract (ADR-0091 §2)
//!
//! [`lookup`] resolves a `(model_id, provider)` pair through three layers:
//!
//! 1. **On-disk cache** — the last successful runtime fetch written
//!    atomically to [`pricing_cache_path`]
//!    (`~/.local/share/budi/pricing.json` on Linux/macOS,
//!    `%LOCALAPPDATA%\budi\pricing.json` on Windows).
//! 2. **Embedded baseline** — a vendored snapshot of LiteLLM's
//!    `model_prices_and_context_window.json`, included at build time via
//!    [`EMBEDDED_BASELINE_JSON`].
//! 3. **Hard-fail to [`PricingOutcome::Unknown`]** — the caller writes
//!    `cost_cents = 0`, `cost_confidence = "estimated_unknown_model"`, and
//!    `pricing_source = "unknown"`. [`warn_once_unknown`] emits one
//!    structured `warn` per `(provider, model_id)` per daemon run.
//!
//! There is no silent per-provider default fallback. Unknown is visible;
//! the dashboard surfaces it; a later refresh that resolves the model
//! triggers [`backfill_unknown_rows`] (Rule A in ADR-0091 §5) — the only
//! automatic rewrite of historical cost data.
//!
//! [ADR-0091]: https://github.com/siropkin/budi/blob/main/docs/adr/0091-model-pricing-manifest-source-of-truth.md

pub mod display;

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, RwLock};

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::provider::ModelPricing;

// ---------------------------------------------------------------------------
// Public types (ADR-0091 §4)
// ---------------------------------------------------------------------------

/// Provenance tag written to `messages.pricing_source`.
///
/// Variant ↔ column-value mapping:
///
/// | Variant                | Column serialization      |
/// |------------------------|---------------------------|
/// | `Manifest { v }`       | `manifest:vNNN`           |
/// | `ManifestAlias { v }`  | `manifest:vNNN:alias`     |
/// | `Backfill { v }`       | `backfilled:vNNN`         |
/// | `EmbeddedBaseline`     | `embedded:vBUILD`         |
/// | `LegacyPreManifest`    | `legacy:pre-manifest`     |
///
/// The `:alias` suffix on `ManifestAlias` (8.4.2 / #680) marks a row whose
/// `model_id` did not directly match a manifest key but resolved through
/// the alias overlay (surface-form → canonical-key). The version stays
/// the same — the alias resolved against the same manifest snapshot.
///
/// Three further column literals (`"unknown"`, `"upstream:api"`,
/// `"unpriced:no_tokens"`) are intentionally *not* variants of this enum:
/// - `"unknown"` is the absence of a source, produced when
///   [`lookup`] returns [`PricingOutcome::Unknown`]. Callers serialize
///   that literal directly.
/// - `"upstream:api"` is written by the Cursor ingest path for rows
///   whose `cost_cents` came from Cursor's Usage API (not from our
///   manifest). See [ADR-0091] §1 commentary on `cost_confidence =
///   "exact"` rows and the decision note in #376.
/// - `"unpriced:no_tokens"` is written by `CostEnricher` for rows that
///   will never have a priceable token cost — user messages, tool-result
///   messages, or any row ingested with zero tokens. Prevents these
///   rows from falling through to the DB's `"legacy:pre-manifest"`
///   DEFAULT, which is reserved for genuinely pre-migration rows (#533).
///
/// `parse_column` returns `None` for all three literals so callers handle
/// them explicitly rather than via a silent fallback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PricingSource {
    Manifest {
        version: u32,
    },
    /// 8.4.2 / #680: same manifest snapshot as `Manifest { version }`,
    /// but the row's `model_id` only matched a manifest entry through
    /// the alias overlay (surface-form → canonical key). Surfaced in
    /// the column with a trailing `:alias` so an operator can audit
    /// alias hits with a `LIKE 'manifest:%:alias'` query.
    ManifestAlias {
        version: u32,
    },
    Backfill {
        version: u32,
    },
    EmbeddedBaseline,
    LegacyPreManifest,
}

/// Column literal written for `PricingOutcome::Unknown` rows.
pub const COLUMN_VALUE_UNKNOWN: &str = "unknown";

/// Column literal written for Cursor exact-cost rows (Usage API). Not a
/// [`PricingSource`] variant — there is no in-manifest provenance for
/// these rows.
pub const COLUMN_VALUE_UPSTREAM_API: &str = "upstream:api";

/// Column literal written by the schema migration for pre-manifest rows.
pub const COLUMN_VALUE_LEGACY: &str = "legacy:pre-manifest";

/// Column literal for rows that will never have a priceable token cost — user
/// messages, tool-result messages, any row ingested with zero tokens. Written
/// by `CostEnricher` so these rows don't fall through to the DB's
/// `"legacy:pre-manifest"` DEFAULT, which is reserved for genuinely
/// pre-migration rows (#533). Not a [`PricingSource`] variant — follows the
/// same sentinel-literal pattern as `"unknown"` / `"upstream:api"`.
pub const COLUMN_VALUE_UNPRICED_NO_TOKENS: &str = "unpriced:no_tokens";

impl PricingSource {
    /// Serialize to the exact string stored in `messages.pricing_source`.
    pub fn as_column_value(&self) -> String {
        match self {
            PricingSource::Manifest { version } => format!("manifest:v{version}"),
            PricingSource::ManifestAlias { version } => format!("manifest:v{version}:alias"),
            PricingSource::Backfill { version } => format!("backfilled:v{version}"),
            PricingSource::EmbeddedBaseline => format!("embedded:v{EMBEDDED_BASELINE_BUILD}"),
            PricingSource::LegacyPreManifest => COLUMN_VALUE_LEGACY.to_string(),
        }
    }

    /// Parse `messages.pricing_source` back into a variant, or `None` for
    /// the literals `"unknown"` / `"upstream:api"` / `"unpriced:no_tokens"`,
    /// or anything malformed.
    pub fn parse_column(value: &str) -> Option<Self> {
        if value == COLUMN_VALUE_LEGACY {
            return Some(PricingSource::LegacyPreManifest);
        }
        if value == COLUMN_VALUE_UNKNOWN
            || value == COLUMN_VALUE_UPSTREAM_API
            || value == COLUMN_VALUE_UNPRICED_NO_TOKENS
        {
            return None;
        }
        // All other valid shapes are `<prefix>:v<rest>`. Colon is ASCII (1
        // byte), so splitting on it is char-boundary-safe regardless of
        // what `rest` contains — model ids never appear in these strings.
        let (prefix, rest) = value.split_once(":v")?;
        // 8.4.2 / #680: split a trailing `:alias` flag off the version
        // tail. Manifest/backfilled versions are pure unsigned ints, so
        // any `:` in `rest` is an alias suffix (or malformed input).
        let (rest, alias) = match rest.split_once(':') {
            Some((v, "alias")) => (v, true),
            Some(_) => return None,
            None => (rest, false),
        };
        match prefix {
            "manifest" => rest.parse::<u32>().ok().map(|v| {
                if alias {
                    PricingSource::ManifestAlias { version: v }
                } else {
                    PricingSource::Manifest { version: v }
                }
            }),
            "backfilled" if !alias => rest
                .parse::<u32>()
                .ok()
                .map(|v| PricingSource::Backfill { version: v }),
            // `embedded:vBUILD` — we don't round-trip the build tag into an
            // enum field (ADR §4 locks the enum shape). Any `embedded:v*`
            // becomes the variant; the build tag is recovered from
            // `EMBEDDED_BASELINE_BUILD` at serialize time.
            "embedded" if !alias => Some(PricingSource::EmbeddedBaseline),
            _ => None,
        }
    }
}

/// The result of a single [`lookup`] call.
#[derive(Debug, Clone)]
pub enum PricingOutcome {
    Known {
        pricing: ModelPricing,
        source: PricingSource,
    },
    Unknown {
        model_id: String,
    },
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Resolve `(model_id, provider)` against the in-memory manifest.
///
/// This is the single entry point replacing `provider::pricing_for_model`
/// (ADR-0091 §1). `CostEnricher` calls it directly.
///
/// On first call, the in-memory table is auto-populated from the embedded
/// baseline so `budi-core` callers (unit tests, `budi db import`) work
/// without a daemon. The daemon overwrites the table with the disk cache
/// at startup and then with each successful refresh via
/// [`install_manifest`].
///
/// Unknown outcomes emit a structured `warn` once per
/// `(provider, model_id)` per process via [`warn_once_unknown`].
///
/// # UTF-8 safety
///
/// `model_id` is looked up as a whole `&str` via `HashMap::get` — no
/// slicing or `split_at` occurs, so any valid UTF-8 input (including
/// multi-byte-character ids) resolves safely without panicking. Zero-
/// length input returns [`PricingOutcome::Unknown`].
pub fn lookup(model_id: &str, provider: &str) -> PricingOutcome {
    let guard = state().read().expect("pricing state RwLock poisoned");
    if let Some(entry) = guard.manifest.entries.get(model_id) {
        return PricingOutcome::Known {
            pricing: entry.to_model_pricing(),
            source: guard.source.clone(),
        };
    }
    // 8.4.2 / #680: alias overlay. When the surface form a provider
    // persists doesn't directly match a manifest key (e.g. Copilot
    // Chat persists `claude-haiku-4.5` while LiteLLM keys are
    // `claude-haiku-4-5*`), resolve through the curated alias map
    // before falling back to Unknown. Aliases live on the manifest
    // (per ADR-0091) so every provider benefits without a parser
    // change. ADR-0092 §2.4.1 explicitly names this overlay as the
    // long-term home for surface-form normalization.
    if let Some(canonical) = guard.manifest.aliases.get(model_id)
        && let Some(entry) = guard.manifest.entries.get(canonical.as_str())
    {
        let source = match &guard.source {
            PricingSource::Manifest { version } => {
                PricingSource::ManifestAlias { version: *version }
            }
            // Embedded path: there's no version to embed in the
            // alias-tagged form. Surface as ManifestAlias { v: 0 }
            // to keep the column shape uniform; v=0 is the
            // documented embedded sentinel.
            PricingSource::EmbeddedBaseline => PricingSource::ManifestAlias { version: 0 },
            // Other sources (Backfill / LegacyPreManifest) shouldn't
            // be the live install source, but if they are we still
            // tag the row as ManifestAlias with the carried version
            // (or 0 for legacy) so callers can rely on the alias
            // signal regardless of the boot path.
            PricingSource::Backfill { version } => {
                PricingSource::ManifestAlias { version: *version }
            }
            PricingSource::ManifestAlias { version } => {
                PricingSource::ManifestAlias { version: *version }
            }
            PricingSource::LegacyPreManifest => PricingSource::ManifestAlias { version: 0 },
        };
        return PricingOutcome::Known {
            pricing: entry.to_model_pricing(),
            source,
        };
    }
    drop(guard);
    warn_once_unknown(provider, model_id);
    PricingOutcome::Unknown {
        model_id: model_id.to_string(),
    }
}

/// Snapshot of the current in-memory manifest for `GET /pricing/status`
/// and `budi pricing status`. Shape is golden-file tested (#376 gate 9).
///
/// `rejected_upstream_rows` (8.3.1+, #483 / ADR-0091 §2 amendment) lists
/// rows the most-recent refresh tick filtered out because they failed
/// per-row sanity checks (NaN, negative, or > $1,000/M). Serialized with
/// `skip_serializing_if = "Vec::is_empty"` so older-client JSON
/// consumers that predate 8.3.1 still see the pre-amendment shape.
#[derive(Debug, Clone, Serialize)]
pub struct PricingState {
    pub source_label: String,
    pub manifest_version: Option<u32>,
    pub fetched_at: Option<String>,
    pub next_refresh_at: Option<String>,
    pub known_model_count: usize,
    pub embedded_baseline_build: String,
    pub unknown_models: Vec<UnknownModelEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rejected_upstream_rows: Vec<RejectedUpstreamRow>,
    /// Active surface-form → canonical-key alias entries (8.4.2 / #680).
    /// Skipped when empty so older `--format json` consumers that
    /// predate the overlay still see the pre-8.4.2 shape.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_aliases: Vec<ModelAliasEntry>,
}

/// One entry in the `model_aliases` overlay surfaced on
/// `GET /pricing/status` and `budi pricing status`. 8.4.2 / #680.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelAliasEntry {
    pub surface_form: String,
    pub canonical: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct UnknownModelEntry {
    pub provider: String,
    pub model_id: String,
    pub first_seen_at: String,
    pub message_count: u64,
}

/// A row the most-recent refresh tick skipped because it failed
/// per-row sanity (NaN price, negative price, or rate over the sanity
/// ceiling). Surfaced on `GET /pricing/status` so an operator can see
/// which upstream rows were dropped without reading the daemon log.
///
/// ADR-0091 §2 amendment (#483 / 8.3.1): `RejectedUpstreamRow` replaces
/// the pre-8.3.1 whole-payload rejection — one bad LiteLLM row no
/// longer blocks the entire manifest refresh.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RejectedUpstreamRow {
    pub model_id: String,
    pub reason: String,
}

/// Clone the currently-authoritative [`Manifest`] for validation of a
/// freshly-fetched payload (see [`validate_payload`]). Under the read
/// lock for as long as the clone takes; the HashMap of ~3,000 entries
/// clones in <10 ms so this is fine for the 24 h refresh cadence.
pub fn current_manifest_snapshot() -> Manifest {
    state()
        .read()
        .expect("pricing state RwLock poisoned")
        .manifest
        .clone()
}

/// Snapshot the current manifest state. Cheap — clones lightweight fields
/// under the read lock and returns. Does no I/O.
///
/// `next_refresh_at` is `None` from the core layer; the daemon fills it in
/// at the HTTP route seam using its own scheduler clock.
pub fn current_state() -> PricingState {
    let guard = state().read().expect("pricing state RwLock poisoned");
    let source_label = match &guard.source {
        PricingSource::Manifest { .. } => "disk cache",
        PricingSource::ManifestAlias { .. } => "disk cache",
        PricingSource::Backfill { .. } => "disk cache",
        PricingSource::EmbeddedBaseline => "embedded baseline",
        PricingSource::LegacyPreManifest => "legacy",
    }
    .to_string();
    let manifest_version = match &guard.source {
        PricingSource::Manifest { version }
        | PricingSource::ManifestAlias { version }
        | PricingSource::Backfill { version } => Some(*version),
        _ => None,
    };
    let mut model_aliases: Vec<ModelAliasEntry> = guard
        .manifest
        .aliases
        .iter()
        .map(|(surface, canonical)| ModelAliasEntry {
            surface_form: surface.clone(),
            canonical: canonical.clone(),
        })
        .collect();
    // Deterministic order so the JSON shape and text view render
    // identically across runs (HashMap iteration is randomized).
    model_aliases.sort_by(|a, b| a.surface_form.cmp(&b.surface_form));
    PricingState {
        source_label,
        manifest_version,
        fetched_at: Some(guard.manifest.fetched_at.clone()),
        next_refresh_at: None,
        known_model_count: guard.manifest.entries.len(),
        embedded_baseline_build: EMBEDDED_BASELINE_BUILD.to_string(),
        unknown_models: snapshot_unknowns(),
        rejected_upstream_rows: guard.rejected_upstream_rows.clone(),
        model_aliases,
    }
}

/// Replace the cached rejected-upstream-rows list. Called by the
/// refresh worker after each successful partition/install tick so
/// `GET /pricing/status` reflects the most recent run. Passing an empty
/// vec clears the list — used when a tick succeeds with no rejections.
pub fn install_rejected_upstream_rows(rows: Vec<RejectedUpstreamRow>) {
    let mut guard = state().write().expect("pricing state RwLock poisoned");
    guard.rejected_upstream_rows = rows;
}

// ---------------------------------------------------------------------------
// Platform-aware cache path
// ---------------------------------------------------------------------------

/// Returns the on-disk cache path for the pricing manifest.
///
/// Resolves under [`crate::config::budi_home_dir`] so `$BUDI_HOME`
/// overrides propagate from the e2e suite.
pub fn pricing_cache_path() -> Result<PathBuf> {
    Ok(crate::config::budi_home_dir()?.join("pricing.json"))
}

// ---------------------------------------------------------------------------
// Manifest types (parsed LiteLLM shape)
// ---------------------------------------------------------------------------

/// Parsed manifest held in memory and written to disk.
///
/// LiteLLM prices are per-token; [`ManifestEntry::to_model_pricing`]
/// converts to per-million-token rates for [`ModelPricing`].
#[derive(Debug, Clone)]
pub struct Manifest {
    pub version: u32,
    pub entries: HashMap<String, ManifestEntry>,
    /// Surface-form → canonical-manifest-key map (8.4.2 / #680).
    ///
    /// LiteLLM keys are dashed and frequently dated
    /// (`claude-haiku-4-5-20251001`); providers persist their own
    /// surface forms (Copilot Chat persists dotted
    /// `claude-haiku-4.5`; older Cursor persists transposed
    /// `claude-4.5-opus-high`). [`lookup`] consults this map after
    /// a direct miss and tags the result as
    /// [`PricingSource::ManifestAlias`].
    ///
    /// Per ADR-0092 §2.4.1, the alias overlay is the architectural
    /// home for surface-form normalization (Option C) — replacing
    /// per-provider inline tables and lookup-time string munging.
    pub aliases: HashMap<String, String>,
    pub fetched_at: String,
}

/// One per-model entry. Mirrors the subset of LiteLLM fields Budi reads —
/// ignored fields (`max_tokens`, `mode`, tool-support booleans, etc.) are
/// dropped at parse time.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ManifestEntry {
    #[serde(rename = "input_cost_per_token", default)]
    pub input_cost_per_token: f64,
    #[serde(rename = "output_cost_per_token", default)]
    pub output_cost_per_token: f64,
    #[serde(rename = "cache_creation_input_token_cost", default)]
    pub cache_creation_input_token_cost: Option<f64>,
    #[serde(rename = "cache_read_input_token_cost", default)]
    pub cache_read_input_token_cost: Option<f64>,
    #[serde(rename = "litellm_provider", default)]
    pub litellm_provider: Option<String>,
}

impl ManifestEntry {
    /// Convert per-token LiteLLM rates into per-million-token
    /// [`ModelPricing`]. Missing cache fields default to `0.0`.
    pub fn to_model_pricing(&self) -> ModelPricing {
        let to_per_million = |x: f64| x * 1_000_000.0;
        ModelPricing {
            input: to_per_million(self.input_cost_per_token),
            output: to_per_million(self.output_cost_per_token),
            cache_write: to_per_million(self.cache_creation_input_token_cost.unwrap_or(0.0)),
            cache_read: to_per_million(self.cache_read_input_token_cost.unwrap_or(0.0)),
        }
    }
}

/// Parse the raw LiteLLM payload into `(model_id -> entry)`.
///
/// - Drops the `"sample_spec"` sentinel.
/// - Drops entries that fail to deserialize into [`ManifestEntry`] (rare;
///   happens when LiteLLM stores non-LLM bucket metadata keyed like a model).
/// - Keeps entries that parse but have both input and output cost = 0.
///   Those are valid LiteLLM entries (e.g. free-tier models) and should
///   still resolve to `Known { cost = 0 }` rather than `Unknown`.
pub fn parse_entries(bytes: &[u8]) -> Result<HashMap<String, ManifestEntry>> {
    let raw: HashMap<String, serde_json::Value> =
        serde_json::from_slice(bytes).context("parse LiteLLM manifest JSON")?;
    let mut entries = HashMap::with_capacity(raw.len());
    for (model_id, value) in raw {
        if model_id == "sample_spec" {
            continue;
        }
        if let Ok(entry) = serde_json::from_value::<ManifestEntry>(value) {
            entries.insert(model_id, entry);
        }
    }
    Ok(entries)
}

// ---------------------------------------------------------------------------
// Embedded baseline
// ---------------------------------------------------------------------------

/// Curated surface-form → canonical-manifest-key alias overlay
/// (8.4.2 / #680).
///
/// LiteLLM ships no aliases table; this list is Budi-owned. Each
/// entry maps a model id Budi has actually observed in the wild
/// (Copilot Chat persisting `claude-haiku-4.5`, older Cursor
/// persisting `claude-4.5-opus-high`) to the canonical LiteLLM
/// key it should price against. Keep entries in this list ONLY
/// when:
///
/// - The surface form has been seen in a real session (so we don't
///   bloat the map with hypothetical aliases).
/// - The canonical key exists in [`EMBEDDED_BASELINE_JSON`] so the
///   alias resolves the same way day-zero (offline) and post-refresh.
///
/// Per ADR-0092 §2.4.1, this overlay is the long-term home for
/// per-provider surface-form normalization. Adding a new entry does
/// not require a parser change in any provider.
pub const EMBEDDED_ALIASES: &[(&str, &str)] = &[
    // Anthropic: dotted marketing form (Copilot Chat / GitHub
    // changelog) → canonical dashed form. Verified surface form on
    // real Copilot Chat sessions during 8.4.1 verification (#680).
    ("claude-haiku-4.5", "claude-haiku-4-5"),
    ("claude-sonnet-4.5", "claude-sonnet-4-5"),
    ("claude-sonnet-4.6", "claude-sonnet-4-6"),
    ("claude-opus-4.5", "claude-opus-4-5"),
    ("claude-opus-4.6", "claude-opus-4-6"),
    ("claude-opus-4.7", "claude-opus-4-7"),
    // Cursor (older transposed form): `claude-<major>.<minor>-<tier>-<effort>`
    // → canonical `claude-<tier>-<major>-<minor>`. Effort suffix is
    // stripped on the alias key — pricing is per-model-tier, not
    // per-effort. Effort still surfaces in `budi stats --models` via
    // the display::resolve overlay.
    ("claude-4.5-opus-high", "claude-opus-4-5"),
    ("claude-4.5-opus-high-thinking", "claude-opus-4-5"),
    ("claude-4.6-opus-high", "claude-opus-4-6"),
    ("claude-4.6-opus-high-thinking", "claude-opus-4-6"),
    ("claude-4.7-opus-high", "claude-opus-4-7"),
    ("claude-4.7-opus-high-thinking", "claude-opus-4-7"),
];

/// Build the alias map from [`EMBEDDED_ALIASES`]. Cheap (≤20
/// entries) — call sites can rebuild fresh per Manifest install.
pub fn embedded_aliases() -> HashMap<String, String> {
    EMBEDDED_ALIASES
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

/// Vendored snapshot of LiteLLM's `model_prices_and_context_window.json`,
/// refreshed by `scripts/pricing/sync_baseline.sh` per ADR-0091 §10.
pub const EMBEDDED_BASELINE_JSON: &str = include_str!("manifest.embedded.json");

/// Serialized into `embedded:vBUILD`. Uses the crate version; the release
/// pipeline can override via a build-script if finer provenance is needed.
pub const EMBEDDED_BASELINE_BUILD: &str = env!("CARGO_PKG_VERSION");

/// Parse [`EMBEDDED_BASELINE_JSON`] into a [`Manifest`].
///
/// Version is a sentinel `0` — the refresh worker overwrites it to `>= 1`
/// via [`install_manifest`] once the `pricing_manifests` row is written.
pub fn load_embedded_manifest() -> Result<Manifest> {
    let entries = parse_entries(EMBEDDED_BASELINE_JSON.as_bytes())?;
    Ok(Manifest {
        version: 0,
        entries,
        aliases: embedded_aliases(),
        fetched_at: Utc::now().to_rfc3339(),
    })
}

// ---------------------------------------------------------------------------
// Disk cache I/O
// ---------------------------------------------------------------------------

/// Read and parse the on-disk cache at `path`. Returns `Ok(None)` when
/// absent. Version is filled in by the caller from `pricing_manifests`.
pub fn load_disk_cache(path: &Path) -> Result<Option<HashMap<String, ManifestEntry>>> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read pricing cache {}", path.display())),
    };
    parse_entries(&bytes).map(Some)
}

/// Atomically write `bytes` to `path`: write to a sibling temp file,
/// `fsync`, then `rename`. See ADR-0091 §3.
pub fn atomic_write_cache(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create pricing cache dir {}", parent.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = std::fs::File::create(&tmp)
            .with_context(|| format!("create pricing temp {}", tmp.display()))?;
        f.write_all(bytes)
            .with_context(|| format!("write pricing temp {}", tmp.display()))?;
        f.sync_all()
            .with_context(|| format!("fsync pricing temp {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename pricing temp into {}", path.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// In-memory hot-swap (RwLock-guarded)
// ---------------------------------------------------------------------------

struct ManifestState {
    manifest: Manifest,
    source: PricingSource,
    /// Rows dropped by the most recent refresh tick per ADR-0091 §2
    /// amendment (8.3.1 / #483). Empty in the pre-refresh state and
    /// after a clean refresh. Serialized under `rejected_upstream_rows`
    /// on `GET /pricing/status`.
    rejected_upstream_rows: Vec<RejectedUpstreamRow>,
}

fn state() -> &'static RwLock<ManifestState> {
    static STATE: OnceLock<RwLock<ManifestState>> = OnceLock::new();
    STATE.get_or_init(|| {
        let manifest = load_embedded_manifest().unwrap_or_else(|e| {
            // A broken embedded baseline is a build-time bug (#376 §10 CI
            // guard), but in production we still want a lookup that
            // returns Unknown rather than panicking ingestion.
            tracing::error!(error = %e, "failed to parse embedded baseline; starting with empty table");
            Manifest {
                version: 0,
                entries: HashMap::new(),
                aliases: embedded_aliases(),
                fetched_at: Utc::now().to_rfc3339(),
            }
        });
        RwLock::new(ManifestState {
            manifest,
            source: PricingSource::EmbeddedBaseline,
            rejected_upstream_rows: Vec::new(),
        })
    })
}

/// Install `manifest` as the in-memory authority under a writer lock.
///
/// Called by the daemon on startup (disk cache with
/// `source = Manifest { version }`) and by the refresh worker after
/// [`validate_payload`] succeeds.
///
/// Concurrent readers in [`lookup`] see either the old or new table —
/// no partial state is observable.
pub fn install_manifest(manifest: Manifest, source: PricingSource) {
    let mut guard = state().write().expect("pricing state RwLock poisoned");
    guard.manifest = manifest;
    guard.source = source;
}

// ---------------------------------------------------------------------------
// Validation guards (ADR-0091 §3)
// ---------------------------------------------------------------------------

/// Sanity ceiling: per-million-token rate. $1,000 / M guards against a
/// stray decimal point upstream.
const PRICE_CEILING_PER_MILLION: f64 = 1000.0;

/// Payload size cap for an upstream fetch.
pub const MAX_PAYLOAD_BYTES: usize = 10 * 1024 * 1024;

/// Retention floor: integer percent of previously-known models that must
/// still be present in a fetched payload. Expressed as a percent (not a
/// fraction) to avoid `f64` rounding drift at the boundary.
const RETENTION_FLOOR_PERCENT: u64 = 95;

/// Whole-payload validation outcomes. Pre-8.3.1 the `NegativePrice`
/// and `SanityCeilingExceeded` variants were returned when any single
/// row failed per-row sanity. 8.3.1 (ADR-0091 §2 amendment / #483)
/// changed row-level sanity failures into non-fatal rejections
/// surfaced as [`RejectedUpstreamRow`]; those two variants are kept
/// so [`ValidationError::Display`] stays stable for any external log
/// scraper, but [`validate_payload`] no longer produces them.
#[derive(Debug, Clone, PartialEq)]
pub enum ValidationError {
    ParseFailed(String),
    NegativePrice { model_id: String },
    SanityCeilingExceeded { model_id: String, per_million: f64 },
    RetentionBelowFloor { kept: usize, required: usize },
    PayloadTooLarge { bytes: usize },
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValidationError::ParseFailed(m) => write!(f, "payload did not parse: {m}"),
            ValidationError::NegativePrice { model_id } => {
                write!(f, "model {model_id} has a negative or NaN price")
            }
            ValidationError::SanityCeilingExceeded {
                model_id,
                per_million,
            } => write!(
                f,
                "model {model_id} price ${per_million:.2}/M exceeds sanity ceiling ${:.0}/M",
                PRICE_CEILING_PER_MILLION
            ),
            ValidationError::RetentionBelowFloor { kept, required } => write!(
                f,
                "only {kept} of required {required} previously-known models retained"
            ),
            ValidationError::PayloadTooLarge { bytes } => write!(
                f,
                "payload {bytes} bytes exceeds {MAX_PAYLOAD_BYTES}-byte cap"
            ),
        }
    }
}

impl std::error::Error for ValidationError {}

/// Run all guards from ADR-0091 §3, including the §2 row-level
/// rejection amendment (8.3.1 / #483).
///
/// `new.entries` is mutated in place: rows that fail per-row sanity
/// (NaN, negative price, or rate over [`PRICE_CEILING_PER_MILLION`])
/// are removed and returned in the `Ok(Vec<RejectedUpstreamRow>)` arm
/// so callers can log them and surface on `GET /pricing/status`. One
/// bad upstream row no longer blocks the whole manifest refresh; pre-
/// 8.3.1 the LiteLLM `wandb/Qwen3-Coder-480B-A35B-Instruct` entry at
/// $100,000/M would `Err(SanityCeilingExceeded)` the whole payload and
/// DoS every user's refresh until upstream patched.
///
/// Whole-payload hard failures still apply:
/// - `raw_bytes_len > MAX_PAYLOAD_BYTES` → `Err(PayloadTooLarge)`.
/// - kept-row retention vs `previous` below
///   [`RETENTION_FLOOR_PERCENT`] → `Err(RetentionBelowFloor)`. The
///   floor runs against KEPT rows (post-partition), so a mass upstream
///   regression still triggers the fail-safe.
///
/// `raw_bytes_len` is checked first so oversized payloads are rejected
/// before any parsing cost.
pub fn validate_payload(
    new: &mut Manifest,
    previous: Option<&Manifest>,
    raw_bytes_len: usize,
) -> std::result::Result<Vec<RejectedUpstreamRow>, ValidationError> {
    if raw_bytes_len > MAX_PAYLOAD_BYTES {
        return Err(ValidationError::PayloadTooLarge {
            bytes: raw_bytes_len,
        });
    }
    let rejected = partition_rows_by_sanity(&mut new.entries);
    if let Some(prev) = previous {
        let prev_total = prev.entries.len();
        if prev_total > 0 {
            let kept = prev
                .entries
                .keys()
                .filter(|id| new.entries.contains_key(id.as_str()))
                .count();
            let required = (prev_total as u64)
                .saturating_mul(RETENTION_FLOOR_PERCENT)
                .div_ceil(100) as usize;
            if kept < required {
                return Err(ValidationError::RetentionBelowFloor { kept, required });
            }
        }
    }
    Ok(rejected)
}

/// Drop per-row sanity failures from `entries` in place and return
/// them. Pure — no state mutation beyond the passed-in map. Broken
/// out from [`validate_payload`] so callers can run row-level
/// partitioning without the size + retention-floor checks. The
/// daemon's `warm_load_disk_cache` calls this directly so a restart
/// re-runs sanity against whatever the disk cache holds.
///
/// A row is rejected if any of its four price fields (input, output,
/// cache-creation, cache-read) is NaN, negative, or exceeds
/// [`PRICE_CEILING_PER_MILLION`] in per-million terms. The reason
/// string is human-readable and surfaced verbatim on
/// `GET /pricing/status` and in the daemon log.
pub fn partition_rows_by_sanity(
    entries: &mut HashMap<String, ManifestEntry>,
) -> Vec<RejectedUpstreamRow> {
    let mut rejected: Vec<RejectedUpstreamRow> = Vec::new();
    entries.retain(|model_id, entry| {
        for price in [
            entry.input_cost_per_token,
            entry.output_cost_per_token,
            entry.cache_creation_input_token_cost.unwrap_or(0.0),
            entry.cache_read_input_token_cost.unwrap_or(0.0),
        ] {
            if price.is_nan() || price < 0.0 {
                rejected.push(RejectedUpstreamRow {
                    model_id: model_id.clone(),
                    reason: "negative or NaN price".to_string(),
                });
                return false;
            }
            let per_million = price * 1_000_000.0;
            if per_million > PRICE_CEILING_PER_MILLION {
                rejected.push(RejectedUpstreamRow {
                    model_id: model_id.clone(),
                    reason: format!(
                        "${per_million:.2}/M exceeds sanity ceiling ${:.0}/M",
                        PRICE_CEILING_PER_MILLION
                    ),
                });
                return false;
            }
        }
        true
    });
    // Deterministic order by model_id so the daemon log and
    // `pricing status` render identically across runs.
    rejected.sort_by(|a, b| a.model_id.cmp(&b.model_id));
    rejected
}

// ---------------------------------------------------------------------------
// Rule A: unknown → backfilled:vNNN
// ---------------------------------------------------------------------------

/// Rewrite every `pricing_source = 'unknown'` row whose `(provider, model)`
/// is now resolvable against the just-installed manifest `version`.
///
/// Implements Rule A from ADR-0091 §5 — the only automatic rewrite of
/// historical cost data. Called by the refresh worker after a successful
/// [`install_manifest`].
///
/// Idempotent: a second call with the same `version` after every unknown
/// row has been resolved is a no-op. Explicitly does not touch rows with
/// any other `pricing_source` value (Rules B and C).
///
/// `cache_creation_1h_tokens`, `speed`, and `web_search_requests` are not
/// columns on `messages` (they live only on the transient `ParsedMessage`
/// at ingest time), so the recompute uses `0` / `None` for them. This may
/// slightly understate the cost of backfilled fast-mode or web-search
/// rows — acceptable because backfill is bounded recovery of a value that
/// was $0 before, not a reprice.
pub fn backfill_unknown_rows(conn: &Connection, version: u32) -> Result<usize> {
    let unknown_rows: Vec<(String, String, String, u64, u64, u64, u64)> = {
        let mut stmt = conn.prepare(
            "SELECT id,
                    COALESCE(model, ''),
                    COALESCE(provider, 'claude_code'),
                    COALESCE(input_tokens, 0),
                    COALESCE(output_tokens, 0),
                    COALESCE(cache_creation_tokens, 0),
                    COALESCE(cache_read_tokens, 0)
             FROM messages
             WHERE pricing_source = ?1",
        )?;
        stmt.query_map(params![COLUMN_VALUE_UNKNOWN], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?.max(0) as u64,
                row.get::<_, i64>(4)?.max(0) as u64,
                row.get::<_, i64>(5)?.max(0) as u64,
                row.get::<_, i64>(6)?.max(0) as u64,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?
    };

    if unknown_rows.is_empty() {
        return Ok(0);
    }

    let tx = conn.unchecked_transaction()?;
    let source_value = PricingSource::Backfill { version }.as_column_value();
    let mut updated = 0usize;
    for (id, model, provider, inp, out, cc, cr) in unknown_rows {
        if model.is_empty() {
            continue;
        }
        if let PricingOutcome::Known { pricing, .. } = lookup(&model, &provider) {
            let cost = pricing.calculate_cost_cents(inp, out, cc, cr, 0, None, 0);
            tx.execute(
                "UPDATE messages
                    SET cost_cents = ?1,
                        cost_confidence = 'estimated',
                        pricing_source = ?2
                  WHERE id = ?3
                    AND pricing_source = ?4",
                params![cost, source_value, id, COLUMN_VALUE_UNKNOWN],
            )?;
            updated += 1;
        }
    }
    tx.commit()?;
    Ok(updated)
}

// ---------------------------------------------------------------------------
// Warn-once dedup for Unknown outcomes
// ---------------------------------------------------------------------------

struct UnknownStat {
    first_seen_at: String,
    message_count: u64,
}

fn unknown_cache() -> &'static Mutex<HashMap<(String, String), UnknownStat>> {
    static CACHE: OnceLock<Mutex<HashMap<(String, String), UnknownStat>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Emit a structured `warn` on the first hit for a `(provider, model_id)`
/// pair. Subsequent hits increment [`UnknownModelEntry::message_count`]
/// without logging.
pub fn warn_once_unknown(provider: &str, model_id: &str) {
    let mut guard = unknown_cache()
        .lock()
        .expect("unknown cache Mutex poisoned");
    let key = (provider.to_string(), model_id.to_string());
    use std::collections::hash_map::Entry;
    match guard.entry(key) {
        Entry::Occupied(mut e) => {
            e.get_mut().message_count += 1;
        }
        Entry::Vacant(e) => {
            e.insert(UnknownStat {
                first_seen_at: Utc::now().to_rfc3339(),
                message_count: 1,
            });
            tracing::warn!(
                provider = provider,
                model_id = model_id,
                "unknown model — not in pricing manifest; cost set to 0 (ADR-0091 §2)"
            );
        }
    }
}

fn snapshot_unknowns() -> Vec<UnknownModelEntry> {
    let guard = unknown_cache()
        .lock()
        .expect("unknown cache Mutex poisoned");
    let mut out: Vec<UnknownModelEntry> = guard
        .iter()
        .map(|((p, m), stat)| UnknownModelEntry {
            provider: p.clone(),
            model_id: m.clone(),
            first_seen_at: stat.first_seen_at.clone(),
            message_count: stat.message_count,
        })
        .collect();
    out.sort_by(|a, b| {
        b.message_count
            .cmp(&a.message_count)
            .then_with(|| a.model_id.cmp(&b.model_id))
    });
    out
}

// ---------------------------------------------------------------------------
// Test-only helpers
// ---------------------------------------------------------------------------

/// Reset the in-memory state and unknown-warn cache to defaults. Test
/// scaffolding only — the state container is process-global and tests
/// that exercise [`install_manifest`] or [`warn_once_unknown`] need a way
/// to reset it between cases.
#[cfg(test)]
pub(crate) fn reset_state_for_test() {
    let mut guard = state().write().expect("pricing state RwLock poisoned");
    guard.manifest = load_embedded_manifest().unwrap_or_else(|_| Manifest {
        version: 0,
        entries: HashMap::new(),
        aliases: HashMap::new(),
        fetched_at: Utc::now().to_rfc3339(),
    });
    guard.source = PricingSource::EmbeddedBaseline;
    guard.rejected_upstream_rows.clear();
    drop(guard);
    unknown_cache()
        .lock()
        .expect("unknown cache Mutex poisoned")
        .clear();
}

#[cfg(test)]
pub(crate) fn install_for_test(entries: HashMap<String, ManifestEntry>, source: PricingSource) {
    let manifest = Manifest {
        version: match &source {
            PricingSource::Manifest { version } | PricingSource::Backfill { version } => *version,
            _ => 0,
        },
        entries,
        aliases: HashMap::new(),
        fetched_at: Utc::now().to_rfc3339(),
    };
    install_manifest(manifest, source);
}

// ---------------------------------------------------------------------------
// Test gates for #376 / ADR-0091 Promotion Criteria
// ---------------------------------------------------------------------------

#[cfg(test)]
mod pricing_tests {
    use super::*;
    use rusqlite::Connection;

    // ---- shared fixtures ---------------------------------------------------

    /// Serial mutex for tests that touch the process-global pricing state.
    /// `install_manifest` and `warn_once_unknown` write to module-level
    /// OnceLock + RwLock / Mutex; interleaving test runs would produce
    /// flakes. Every state-mutating test in this module acquires this
    /// lock before touching the state.
    fn serial() -> &'static std::sync::Mutex<()> {
        static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    fn entry(input: f64, output: f64) -> ManifestEntry {
        ManifestEntry {
            input_cost_per_token: input,
            output_cost_per_token: output,
            cache_creation_input_token_cost: None,
            cache_read_input_token_cost: None,
            litellm_provider: Some("anthropic".to_string()),
        }
    }

    fn in_memory_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::migration::migrate(&conn).unwrap();
        conn
    }

    fn insert_row(
        conn: &Connection,
        id: &str,
        model: &str,
        provider: &str,
        cost_cents: f64,
        cost_confidence: &str,
        pricing_source: &str,
    ) {
        conn.execute(
            "INSERT INTO messages
                (id, role, timestamp, model, provider,
                 input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
                 cost_cents, cost_confidence, pricing_source)
             VALUES (?1, 'assistant', '2026-04-20T00:00:00Z', ?2, ?3,
                     100, 50, 0, 0, ?4, ?5, ?6)",
            rusqlite::params![
                id,
                model,
                provider,
                cost_cents,
                cost_confidence,
                pricing_source
            ],
        )
        .unwrap();
    }

    fn cost_of(conn: &Connection, id: &str) -> f64 {
        conn.query_row(
            "SELECT cost_cents FROM messages WHERE id = ?1",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn source_of(conn: &Connection, id: &str) -> String {
        conn.query_row(
            "SELECT pricing_source FROM messages WHERE id = ?1",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .unwrap()
    }

    // ---- Gate 1: manifest:vNNN rows never recomputed -----------------------

    /// ADR-0091 §5 Rule B: when a refresh lands a new price for a model,
    /// rows already tagged `manifest:vNNN` stay at their original cost
    /// and source tag. `backfill_unknown_rows` is the only automatic
    /// rewrite path and it scopes to `pricing_source = 'unknown'`.
    #[test]
    fn gate_1_manifest_rows_never_recomputed_on_refresh() {
        let _g = serial().lock().unwrap();
        reset_state_for_test();
        let conn = in_memory_db();

        // Pretend a v1 manifest priced this row at 100c.
        insert_row(
            &conn,
            "row-manifest",
            "gate-model",
            "claude_code",
            100.0,
            "estimated",
            "manifest:v1",
        );

        // Install a v2 manifest that reprices the same model 10x higher.
        let mut entries = HashMap::new();
        entries.insert("gate-model".to_string(), entry(0.00001, 0.00005));
        install_for_test(entries, PricingSource::Manifest { version: 2 });

        // Simulate the worker's post-install backfill.
        let updated = backfill_unknown_rows(&conn, 2).unwrap();
        assert_eq!(updated, 0, "manifest rows must not be touched");
        assert_eq!(cost_of(&conn, "row-manifest"), 100.0);
        assert_eq!(source_of(&conn, "row-manifest"), "manifest:v1");
    }

    // ---- Gate 2: legacy:pre-manifest rows never recomputed -----------------

    /// ADR-0091 §5 Rule C: pre-migration rows are forever frozen. The
    /// buggy Opus 4.7 rows stay buggy; users see the step change at the
    /// migration date and interpret it via the release-notes banner.
    #[test]
    fn gate_2_legacy_pre_manifest_rows_never_recomputed() {
        let _g = serial().lock().unwrap();
        reset_state_for_test();
        let conn = in_memory_db();

        insert_row(
            &conn,
            "row-legacy",
            "claude-opus-4-7",
            "claude_code",
            999.0, // the pre-manifest buggy 3× cost
            "estimated",
            "legacy:pre-manifest",
        );

        // Install any manifest with a matching model id.
        let mut entries = HashMap::new();
        entries.insert("claude-opus-4-7".to_string(), entry(0.000005, 0.000025));
        install_for_test(entries, PricingSource::Manifest { version: 3 });

        let updated = backfill_unknown_rows(&conn, 3).unwrap();
        assert_eq!(updated, 0, "legacy rows must not be touched");
        assert_eq!(cost_of(&conn, "row-legacy"), 999.0);
        assert_eq!(source_of(&conn, "row-legacy"), "legacy:pre-manifest");
    }

    // ---- Gate 3: unknown → backfilled:vNNN on refresh ----------------------

    /// ADR-0091 §5 Rule A: when a refresh resolves a previously-unknown
    /// model, backfill_unknown_rows rewrites the $0 row with the real
    /// cost and tags it `backfilled:vNNN`.
    #[test]
    fn gate_3_unknown_rows_backfill_to_resolved_cost() {
        let _g = serial().lock().unwrap();
        reset_state_for_test();
        let conn = in_memory_db();

        insert_row(
            &conn,
            "row-unknown",
            "new-grok-model",
            "claude_code",
            0.0,
            "estimated_unknown_model",
            COLUMN_VALUE_UNKNOWN,
        );

        // The refresh lands a manifest that now knows about this model.
        let mut entries = HashMap::new();
        // 1 per-M input, 5 per-M output (per-token: 1e-6 / 5e-6)
        entries.insert("new-grok-model".to_string(), entry(0.000001, 0.000005));
        install_for_test(entries, PricingSource::Manifest { version: 7 });

        let updated = backfill_unknown_rows(&conn, 7).unwrap();
        assert_eq!(updated, 1, "unknown row should be backfilled exactly once");

        // 100 input tokens * $1/M = $0.0001 = 0.01c
        // 50 output tokens * $5/M = $0.00025 = 0.025c
        // Total ≈ 0.035c
        let cost = cost_of(&conn, "row-unknown");
        assert!(
            (cost - 0.035).abs() < 0.0001,
            "expected ~0.035c, got {cost}"
        );
        assert_eq!(source_of(&conn, "row-unknown"), "backfilled:v7");

        // Idempotent: a second call does nothing.
        let again = backfill_unknown_rows(&conn, 7).unwrap();
        assert_eq!(again, 0);
    }

    // ---- Gate 4: UTF-8 boundary safety -------------------------------------

    /// Model ids pass through `lookup` as arbitrary UTF-8. The lookup
    /// uses `HashMap::get(&str)` which is boundary-safe; this test pins
    /// that no slicing / `split_at` site sneaks in later.
    #[test]
    fn gate_4_utf8_boundary_safe_lookup() {
        let _g = serial().lock().unwrap();
        reset_state_for_test();

        // Multi-byte model ids with 4-byte chars (🦀 + assorted CJK).
        let multibyte_ids = [
            "",               // zero-length
            "🦀-model",       // leading 4-byte emoji
            "model-🦀-4",     // embedded 4-byte emoji
            "模型-opus",      // CJK bytes
            "δοκιμή-πρότυπο", // multi-byte Greek
        ];
        let mut entries = HashMap::new();
        for id in multibyte_ids {
            entries.insert(id.to_string(), entry(0.000001, 0.000002));
        }
        install_for_test(entries, PricingSource::Manifest { version: 1 });

        // Lookup every one without panic, and a guaranteed-unknown one.
        for id in multibyte_ids {
            match lookup(id, "claude_code") {
                PricingOutcome::Known { pricing, .. } => {
                    assert!(pricing.input >= 0.0);
                }
                PricingOutcome::Unknown { model_id } => {
                    // Zero-length id or unexpected mismatch — must still
                    // round-trip the id as-is without slicing corruption.
                    assert_eq!(model_id, id);
                }
            }
        }
        // Unknown id with multi-byte content must not panic.
        match lookup("🚀-unknown-🎯", "claude_code") {
            PricingOutcome::Unknown { model_id } => {
                assert_eq!(model_id, "🚀-unknown-🎯");
            }
            PricingOutcome::Known { .. } => panic!("should be Unknown"),
        }
    }

    // ---- Gate 5: <95%-retention guard rejects a wiped payload --------------

    /// ADR-0091 §3: the validator rejects payloads that drop more than 5%
    /// of previously-known models. Guards against an accidental upstream
    /// wipe, supply-chain tampering, or a mid-rewrite commit.
    ///
    /// 8.3.1 (#483): the floor applies to the kept rows after per-row
    /// sanity partitioning runs, so a payload that would otherwise pass
    /// but has enough per-row rejections to drop below the floor still
    /// hard-fails.
    #[test]
    fn gate_5_retention_floor_rejects_wiped_payload() {
        // Build a previous manifest of 100 known models.
        let mut prev_entries = HashMap::new();
        for i in 0..100 {
            prev_entries.insert(format!("prev-model-{i}"), entry(0.000001, 0.000002));
        }
        let previous = Manifest {
            version: 5,
            entries: prev_entries,
            aliases: HashMap::new(),
            fetched_at: Utc::now().to_rfc3339(),
        };

        // New payload has only 80 of those models — 80% retention, below
        // the 95% floor.
        let mut new_entries = HashMap::new();
        for i in 0..80 {
            new_entries.insert(format!("prev-model-{i}"), entry(0.000001, 0.000002));
        }
        let mut candidate = Manifest {
            version: 6,
            entries: new_entries,
            aliases: HashMap::new(),
            fetched_at: Utc::now().to_rfc3339(),
        };

        let err = validate_payload(&mut candidate, Some(&previous), 10_000).unwrap_err();
        assert!(
            matches!(
                err,
                ValidationError::RetentionBelowFloor {
                    kept: 80,
                    required: 95
                }
            ),
            "expected RetentionBelowFloor(80, 95), got {err:?}"
        );

        // A 95+ payload passes.
        let mut pass = HashMap::new();
        for i in 0..96 {
            pass.insert(format!("prev-model-{i}"), entry(0.000001, 0.000002));
        }
        let mut candidate = Manifest {
            version: 6,
            entries: pass,
            aliases: HashMap::new(),
            fetched_at: Utc::now().to_rfc3339(),
        };
        let rejected = validate_payload(&mut candidate, Some(&previous), 10_000)
            .expect("96% retention must pass");
        assert!(
            rejected.is_empty(),
            "clean payload should have no rejected rows"
        );
    }

    // ---- Gate 6 (8.3.1 amendment): one insane row is rejected, rest kept ---

    /// ADR-0091 §2 amendment (8.3.1 / #483): a row exceeding the
    /// $1,000/M sanity ceiling is filtered out of the manifest and
    /// surfaced in `RejectedUpstreamRow`, but the rest of the payload
    /// still refreshes. Pre-8.3.1 the same input would hard-fail via
    /// `ValidationError::SanityCeilingExceeded`, DoSing the refresher
    /// until the bad row was patched upstream (the 2026-04-22
    /// `wandb/Qwen3-Coder-480B-A35B-Instruct` $100,000/M incident).
    #[test]
    fn gate_6_sanity_ceiling_rejects_row_keeps_rest() {
        // Enough good rows so the retention floor (no previous here) is
        // not the bottleneck. $0.001 per token = $1,000/M, right at the
        // ceiling — stays below, kept. $0.002 per token = $2,000/M,
        // 2x over the ceiling, rejected.
        let mut entries = HashMap::new();
        entries.insert("mispriced".to_string(), entry(0.002, 0.002));
        for i in 0..50 {
            entries.insert(format!("ok-model-{i}"), entry(0.000001, 0.000002));
        }
        let mut candidate = Manifest {
            version: 1,
            entries,
            aliases: HashMap::new(),
            fetched_at: Utc::now().to_rfc3339(),
        };
        let rejected = validate_payload(&mut candidate, None, 10_000)
            .expect("row-level rejection must not fail payload");
        assert_eq!(rejected.len(), 1, "exactly one row should be rejected");
        assert_eq!(rejected[0].model_id, "mispriced");
        assert!(
            rejected[0].reason.contains("sanity ceiling"),
            "reason should mention the sanity ceiling, got {:?}",
            rejected[0].reason
        );
        assert_eq!(
            candidate.entries.len(),
            50,
            "the 50 ok rows must remain after partitioning"
        );
        assert!(
            !candidate.entries.contains_key("mispriced"),
            "the mispriced row must be dropped from the kept set"
        );
    }

    /// 8.3.1 / #483: row-level rejection drops enough rows that the
    /// kept count falls below the retention floor. Whole payload must
    /// still hard-fail so a mass upstream mispricing incident can't
    /// sneak through.
    #[test]
    fn row_level_rejection_still_triggers_retention_floor() {
        let mut prev_entries = HashMap::new();
        for i in 0..100 {
            prev_entries.insert(format!("m-{i}"), entry(0.000001, 0.000002));
        }
        let previous = Manifest {
            version: 5,
            entries: prev_entries,
            aliases: HashMap::new(),
            fetched_at: Utc::now().to_rfc3339(),
        };
        // 100 entries in the new payload — same ids as previous — but
        // 50 of them are over the sanity ceiling. 50 kept out of 100
        // previously-known = 50% retention, below the 95% floor.
        let mut new_entries = HashMap::new();
        for i in 0..100 {
            let e = if i < 50 {
                entry(0.002, 0.002) // over ceiling — rejected
            } else {
                entry(0.000001, 0.000002)
            };
            new_entries.insert(format!("m-{i}"), e);
        }
        let mut candidate = Manifest {
            version: 6,
            entries: new_entries,
            aliases: HashMap::new(),
            fetched_at: Utc::now().to_rfc3339(),
        };
        let err = validate_payload(&mut candidate, Some(&previous), 10_000).unwrap_err();
        assert!(
            matches!(err, ValidationError::RetentionBelowFloor { kept: 50, .. }),
            "row-level rejection must still trip the retention floor, got {err:?}"
        );
    }

    // ---- Gate 8 (partial): schema migration idempotent for pricing rows ----

    /// ADR-0091 §7: the migration is idempotent. Running it twice must
    /// not duplicate `pricing_manifests` rows, must not double-add the
    /// `pricing_source` column, and must not touch row data. The
    /// cross-cutting reconcile path is exercised in
    /// `migration::tests::repair_is_idempotent`; this test pins the
    /// pricing-specific invariants.
    #[test]
    fn gate_8_pricing_migration_is_idempotent() {
        let conn = in_memory_db();

        let count_v0: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pricing_manifests WHERE version = 0",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let count_v1: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pricing_manifests WHERE version = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count_v0, 1, "v0 pre-manifest anchor should exist");
        assert_eq!(count_v1, 1, "v1 embedded row should exist");

        // Run the reconcile path a second time — idempotent.
        crate::migration::repair(&conn).unwrap();

        let count_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM pricing_manifests", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            count_after, 2,
            "repair() must not duplicate pricing_manifests rows"
        );

        // Column wasn't double-added.
        let pricing_source_cols: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('messages')
                 WHERE name = 'pricing_source'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pricing_source_cols, 1);
    }

    // ---- Gate 9: budi pricing status --json shape ---------------------------

    /// Pins the JSON shape `budi pricing status --json` emits. Key names
    /// and types are asserted against the committed contract; values are
    /// not pinned so this test is stable across releases (fetched_at
    /// and embedded_baseline_build naturally drift).
    ///
    /// 8.3.1 / #483: `rejected_upstream_rows` is `skip_serializing_if =
    /// "Vec::is_empty"`, so a clean snapshot does NOT include the key
    /// (preserves pre-8.3.1 client compat). The populated shape is
    /// exercised in `pricing_status_surfaces_rejected_rows_when_populated`.
    #[test]
    fn gate_9_pricing_status_json_shape_is_stable() {
        let _g = serial().lock().unwrap();
        reset_state_for_test();
        // Seed an unknown so the unknown_models array has at least one
        // entry whose shape we can check.
        warn_once_unknown("claude_code", "ghost-model-1");

        let state = current_state();
        let j = serde_json::to_value(&state).unwrap();
        let obj = j.as_object().expect("top-level should be object");

        // Pin the exact set of top-level keys on the clean (empty-
        // rejected-rows) snapshot — stable across older clients.
        // 8.4.2 / #680: `model_aliases` is present whenever the
        // installed manifest carries the curated overlay (the
        // embedded baseline always does), so it appears here even
        // on a clean snapshot.
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "embedded_baseline_build",
                "fetched_at",
                "known_model_count",
                "manifest_version",
                "model_aliases",
                "next_refresh_at",
                "source_label",
                "unknown_models",
            ]
        );

        // Types.
        assert!(obj["source_label"].is_string());
        assert!(obj["manifest_version"].is_u64() || obj["manifest_version"].is_null());
        assert!(obj["fetched_at"].is_string() || obj["fetched_at"].is_null());
        assert!(obj["next_refresh_at"].is_string() || obj["next_refresh_at"].is_null());
        assert!(obj["known_model_count"].is_u64());
        assert!(obj["embedded_baseline_build"].is_string());

        // Unknown-models array shape: each entry is an object with the
        // four documented keys and correct types.
        let unknowns = obj["unknown_models"]
            .as_array()
            .expect("unknown_models must be array");
        assert!(
            !unknowns.is_empty(),
            "warn_once_unknown should have added one"
        );
        for entry in unknowns {
            let e = entry.as_object().unwrap();
            let mut ek: Vec<&str> = e.keys().map(String::as_str).collect();
            ek.sort();
            assert_eq!(
                ek,
                vec!["first_seen_at", "message_count", "model_id", "provider"]
            );
            assert!(e["provider"].is_string());
            assert!(e["model_id"].is_string());
            assert!(e["first_seen_at"].is_string());
            assert!(e["message_count"].is_u64());
        }
    }

    // ---- Extra: column-value round-trip for PricingSource ------------------

    #[test]
    fn pricing_source_column_value_round_trip() {
        for src in [
            PricingSource::Manifest { version: 1 },
            PricingSource::Manifest { version: 99 },
            PricingSource::Backfill { version: 14 },
            PricingSource::LegacyPreManifest,
        ] {
            let s = src.as_column_value();
            let parsed =
                PricingSource::parse_column(&s).unwrap_or_else(|| panic!("failed to parse {s:?}"));
            assert_eq!(parsed, src);
        }
        // Embedded round-trips to the variant regardless of build tag.
        let embedded = PricingSource::EmbeddedBaseline.as_column_value();
        assert!(embedded.starts_with("embedded:v"));
        assert_eq!(
            PricingSource::parse_column(&embedded),
            Some(PricingSource::EmbeddedBaseline)
        );
        // Unknown + upstream:api + unpriced:no_tokens round-trip to None
        // (sentinel literals, not enum variants).
        assert_eq!(PricingSource::parse_column(COLUMN_VALUE_UNKNOWN), None);
        assert_eq!(PricingSource::parse_column(COLUMN_VALUE_UPSTREAM_API), None);
        // #533: new sentinel for zero-token / user-role rows.
        assert_eq!(
            PricingSource::parse_column(COLUMN_VALUE_UNPRICED_NO_TOKENS),
            None,
        );
        assert_eq!(PricingSource::parse_column("garbage"), None);
    }

    /// 8.3.1 / #483: when the last refresh tick dropped rows, they
    /// surface on `GET /pricing/status` under `rejected_upstream_rows`.
    /// Each entry has `model_id` + `reason`.
    #[test]
    fn pricing_status_surfaces_rejected_rows_when_populated() {
        let _g = serial().lock().unwrap();
        reset_state_for_test();
        install_rejected_upstream_rows(vec![
            RejectedUpstreamRow {
                model_id: "wandb/Qwen/Qwen3-Coder-480B-A35B-Instruct".to_string(),
                reason: "$100000.00/M exceeds sanity ceiling $1000/M".to_string(),
            },
            RejectedUpstreamRow {
                model_id: "broken/nan-price".to_string(),
                reason: "negative or NaN price".to_string(),
            },
        ]);

        let state = current_state();
        let j = serde_json::to_value(&state).unwrap();
        let obj = j.as_object().unwrap();
        let rejected = obj["rejected_upstream_rows"]
            .as_array()
            .expect("rejected_upstream_rows must be array when populated");
        assert_eq!(rejected.len(), 2);
        for entry in rejected {
            let e = entry.as_object().unwrap();
            let mut ek: Vec<&str> = e.keys().map(String::as_str).collect();
            ek.sort();
            assert_eq!(ek, vec!["model_id", "reason"]);
            assert!(e["model_id"].is_string());
            assert!(e["reason"].is_string());
        }

        // Clearing mid-run (clean refresh tick) removes the key again.
        install_rejected_upstream_rows(Vec::new());
        let state = current_state();
        let j = serde_json::to_value(&state).unwrap();
        let obj = j.as_object().unwrap();
        assert!(
            !obj.contains_key("rejected_upstream_rows"),
            "empty list must be skipped (back-compat with older clients)"
        );
    }

    // ---- 8.4.2 / #680: model_aliases overlay -------------------------------

    /// Helper: install a manifest with explicit entries + aliases so
    /// alias-path tests are deterministic regardless of what the
    /// embedded baseline carries.
    fn install_with_aliases(
        entries: HashMap<String, ManifestEntry>,
        aliases: HashMap<String, String>,
        source: PricingSource,
    ) {
        let manifest = Manifest {
            version: match &source {
                PricingSource::Manifest { version }
                | PricingSource::ManifestAlias { version }
                | PricingSource::Backfill { version } => *version,
                _ => 0,
            },
            entries,
            aliases,
            fetched_at: Utc::now().to_rfc3339(),
        };
        install_manifest(manifest, source);
    }

    /// Acceptance: dotted surface form (Copilot Chat persists
    /// `claude-haiku-4.5`) resolves through the alias overlay to a
    /// dashed canonical key (`claude-haiku-4-5`) and the row is
    /// tagged `manifest:vNNN:alias`.
    #[test]
    fn alias_resolves_dotted_to_dashed_with_alias_tagged_source() {
        let _g = serial().lock().unwrap();
        reset_state_for_test();

        let mut entries = HashMap::new();
        // Per-token rates → 1/M input, 5/M output (matches haiku 4.5).
        entries.insert("claude-haiku-4-5".to_string(), entry(0.000001, 0.000005));

        let mut aliases = HashMap::new();
        aliases.insert(
            "claude-haiku-4.5".to_string(),
            "claude-haiku-4-5".to_string(),
        );

        install_with_aliases(entries, aliases, PricingSource::Manifest { version: 14 });

        let outcome = lookup("claude-haiku-4.5", "copilot_chat");
        match outcome {
            PricingOutcome::Known { pricing, source } => {
                assert!(
                    pricing.input > 0.0,
                    "alias hit must carry the canonical pricing, got {pricing:?}"
                );
                assert_eq!(
                    source,
                    PricingSource::ManifestAlias { version: 14 },
                    "alias hit must tag the row source as ManifestAlias",
                );
                assert_eq!(source.as_column_value(), "manifest:v14:alias");
            }
            PricingOutcome::Unknown { .. } => {
                panic!("dotted form should resolve via alias overlay");
            }
        }
    }

    /// Direct hit on a canonical key still returns the un-aliased
    /// `Manifest { version }` source (alias path is fallback only).
    #[test]
    fn alias_overlay_does_not_affect_direct_hits() {
        let _g = serial().lock().unwrap();
        reset_state_for_test();

        let mut entries = HashMap::new();
        entries.insert("claude-sonnet-4-5".to_string(), entry(0.000003, 0.000015));
        let mut aliases = HashMap::new();
        aliases.insert(
            "claude-sonnet-4.5".to_string(),
            "claude-sonnet-4-5".to_string(),
        );
        install_with_aliases(entries, aliases, PricingSource::Manifest { version: 14 });

        let outcome = lookup("claude-sonnet-4-5", "claude_code");
        match outcome {
            PricingOutcome::Known { source, .. } => {
                assert_eq!(
                    source,
                    PricingSource::Manifest { version: 14 },
                    "direct hit must NOT be tagged via-alias",
                );
            }
            PricingOutcome::Unknown { .. } => panic!("canonical key should hit directly"),
        }
    }

    /// Negative path: a model id that is neither a manifest key nor
    /// an alias still emits `Unknown` (no silent per-provider
    /// default; ADR-0091 §2 invariant preserved).
    #[test]
    fn alias_overlay_falls_through_to_unknown_when_no_match() {
        let _g = serial().lock().unwrap();
        reset_state_for_test();

        let mut entries = HashMap::new();
        entries.insert("claude-haiku-4-5".to_string(), entry(0.000001, 0.000005));
        let mut aliases = HashMap::new();
        aliases.insert(
            "claude-haiku-4.5".to_string(),
            "claude-haiku-4-5".to_string(),
        );
        install_with_aliases(entries, aliases, PricingSource::Manifest { version: 14 });

        match lookup("totally-unknown-model", "copilot_chat") {
            PricingOutcome::Unknown { model_id } => {
                assert_eq!(model_id, "totally-unknown-model");
            }
            PricingOutcome::Known { .. } => {
                panic!("unaliased unknown id should NOT hit through the overlay");
            }
        }
    }

    /// Cursor's transposed older surface form
    /// `claude-4.5-opus-high` resolves to the canonical
    /// `claude-opus-4-5` via the alias overlay.
    #[test]
    fn alias_resolves_cursor_transposed_form() {
        let _g = serial().lock().unwrap();
        reset_state_for_test();

        let mut entries = HashMap::new();
        entries.insert("claude-opus-4-5".to_string(), entry(0.000005, 0.000025));
        let mut aliases = HashMap::new();
        aliases.insert(
            "claude-4.5-opus-high".to_string(),
            "claude-opus-4-5".to_string(),
        );
        install_with_aliases(entries, aliases, PricingSource::Manifest { version: 14 });

        match lookup("claude-4.5-opus-high", "cursor") {
            PricingOutcome::Known { pricing, source } => {
                assert!(pricing.input > 0.0);
                assert_eq!(source, PricingSource::ManifestAlias { version: 14 });
            }
            PricingOutcome::Unknown { .. } => {
                panic!("Cursor transposed form should resolve via alias overlay")
            }
        }
    }

    /// Round-trip: `manifest:vNNN:alias` parses back into
    /// `PricingSource::ManifestAlias { version }`.
    #[test]
    fn pricing_source_column_value_round_trip_includes_alias_variant() {
        for src in [
            PricingSource::ManifestAlias { version: 1 },
            PricingSource::ManifestAlias { version: 14 },
            PricingSource::ManifestAlias { version: 99 },
        ] {
            let s = src.as_column_value();
            assert!(
                s.ends_with(":alias"),
                "alias-tagged column must end with `:alias`, got {s:?}",
            );
            let parsed =
                PricingSource::parse_column(&s).unwrap_or_else(|| panic!("failed to parse {s:?}"));
            assert_eq!(parsed, src);
        }
        // Malformed alias trailers still parse to None (defensive).
        assert_eq!(PricingSource::parse_column("manifest:v1:bogus"), None);
        // Backfilled rows do NOT support an `:alias` suffix today —
        // backfill provenance is its own class.
        assert_eq!(PricingSource::parse_column("backfilled:v1:alias"), None);
    }

    /// `current_state` surfaces the active alias overlay as a
    /// `model_aliases` array. Each entry has `surface_form` +
    /// `canonical`; the array is sorted by surface_form for stable
    /// rendering.
    #[test]
    fn current_state_surfaces_model_aliases_when_populated() {
        let _g = serial().lock().unwrap();
        reset_state_for_test();

        let mut entries = HashMap::new();
        entries.insert("claude-haiku-4-5".to_string(), entry(0.000001, 0.000005));
        entries.insert("claude-opus-4-5".to_string(), entry(0.000005, 0.000025));
        let mut aliases = HashMap::new();
        aliases.insert(
            "claude-haiku-4.5".to_string(),
            "claude-haiku-4-5".to_string(),
        );
        aliases.insert(
            "claude-4.5-opus-high".to_string(),
            "claude-opus-4-5".to_string(),
        );
        install_with_aliases(entries, aliases, PricingSource::Manifest { version: 14 });

        let state = current_state();
        let j = serde_json::to_value(&state).unwrap();
        let arr = j["model_aliases"]
            .as_array()
            .expect("model_aliases must be present when overlay is populated");
        assert_eq!(arr.len(), 2);

        // Sorted by surface_form, so `claude-4.5-opus-high` comes first.
        assert_eq!(
            arr[0]["surface_form"].as_str().unwrap(),
            "claude-4.5-opus-high",
        );
        assert_eq!(arr[0]["canonical"].as_str().unwrap(), "claude-opus-4-5");
        assert_eq!(arr[1]["surface_form"].as_str().unwrap(), "claude-haiku-4.5");
        assert_eq!(arr[1]["canonical"].as_str().unwrap(), "claude-haiku-4-5");

        // Each entry has exactly the documented two keys.
        for entry in arr {
            let mut keys: Vec<&str> = entry
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            keys.sort();
            assert_eq!(keys, vec!["canonical", "surface_form"]);
        }
    }

    /// Empty alias overlay is skip-serialized so older
    /// `--format json` clients still see the pre-8.4.2 shape.
    #[test]
    fn current_state_skips_model_aliases_when_empty() {
        let _g = serial().lock().unwrap();
        reset_state_for_test();
        // Force-install a manifest with empty aliases; bypasses the
        // embedded overlay so we can pin the empty-skip behavior.
        install_with_aliases(
            HashMap::new(),
            HashMap::new(),
            PricingSource::Manifest { version: 1 },
        );
        let state = current_state();
        let j = serde_json::to_value(&state).unwrap();
        assert!(
            !j.as_object().unwrap().contains_key("model_aliases"),
            "empty overlay must be skip-serialized for back-compat",
        );
    }

    /// The curated `EMBEDDED_ALIASES` list is internally consistent:
    /// every canonical key it points at exists in the embedded
    /// LiteLLM baseline, so day-zero offline installs price every
    /// alias correctly.
    #[test]
    fn embedded_aliases_all_resolve_against_embedded_baseline() {
        let manifest = load_embedded_manifest().expect("embedded baseline must parse");
        for (surface, canonical) in EMBEDDED_ALIASES {
            assert!(
                manifest.entries.contains_key(*canonical),
                "alias `{surface}` → `{canonical}` points at a key not in the embedded baseline",
            );
            assert_eq!(
                manifest.aliases.get(*surface).map(String::as_str),
                Some(*canonical),
                "alias `{surface}` not loaded from embedded list",
            );
        }
    }
}
