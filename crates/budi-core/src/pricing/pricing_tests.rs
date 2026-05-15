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
             cost_cents_ingested, cost_cents_effective, cost_confidence, pricing_source)
         VALUES (?1, 'assistant', '2026-04-20T00:00:00Z', ?2, ?3,
                 100, 50, 0, 0, ?4, ?4, ?5, ?6)",
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
        "SELECT cost_cents_effective FROM messages WHERE id = ?1",
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
    let rejected =
        validate_payload(&mut candidate, Some(&previous), 10_000).expect("96% retention must pass");
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
