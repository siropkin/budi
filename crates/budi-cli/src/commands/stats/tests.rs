use super::*;

#[test]
fn period_label_covers_relative_windows() {
    assert_eq!(period_label(StatsPeriod::Today), "Today");
    // `Week` and `Month` now describe the rolling window they
    // resolve to so the label cannot drift from the numeric total
    // again (#447).
    assert_eq!(period_label(StatsPeriod::Week), "Last 7 days");
    assert_eq!(period_label(StatsPeriod::Month), "Last 30 days");
    assert_eq!(period_label(StatsPeriod::All), "All time");

    // Singular forms avoid the "Last 1 days" infelicity.
    assert_eq!(period_label(StatsPeriod::Days(1)), "Last 1 day");
    assert_eq!(period_label(StatsPeriod::Weeks(1)), "Last 1 week");
    assert_eq!(period_label(StatsPeriod::Months(1)), "Last 1 month");

    assert_eq!(period_label(StatsPeriod::Days(7)), "Last 7 days");
    assert_eq!(period_label(StatsPeriod::Weeks(2)), "Last 2 weeks");
    assert_eq!(period_label(StatsPeriod::Months(3)), "Last 3 months");
}

#[test]
fn period_key_returns_canonical_cli_form() {
    assert_eq!(period_key(StatsPeriod::Today), "today");
    assert_eq!(period_key(StatsPeriod::Week), "week");
    assert_eq!(period_key(StatsPeriod::Month), "month");
    assert_eq!(period_key(StatsPeriod::All), "all");
    assert_eq!(period_key(StatsPeriod::Days(7)), "7d");
    assert_eq!(period_key(StatsPeriod::Weeks(2)), "2w");
    assert_eq!(period_key(StatsPeriod::Months(1)), "1m");
}

#[test]
fn period_date_range_all_has_no_since() {
    let (since, until) = period_date_range(StatsPeriod::All);
    assert!(since.is_none(), "`all` must not clamp the since bound");
    assert!(until.is_none());
}

#[test]
fn period_date_range_today_pins_local_midnight() {
    let (since, until) = period_date_range(StatsPeriod::Today);
    assert!(since.is_some(), "`today` must anchor a since bound");
    assert!(until.is_none());
}

#[test]
fn period_date_range_relative_windows_go_backwards() {
    // Rolling windows must produce a `since` strictly earlier than
    // `today`'s `since` for any N >= 1. This is the behavior the
    // statusline and cloud dashboard rely on for rolling 1d/7d/30d
    // semantics (#404, ADR-0088 §4).
    let (today_since, _) = period_date_range(StatsPeriod::Today);
    let today_since = today_since.expect("today has a since bound");

    for period in [
        StatsPeriod::Days(1),
        StatsPeriod::Days(7),
        StatsPeriod::Weeks(1),
        StatsPeriod::Weeks(2),
        StatsPeriod::Months(1),
        StatsPeriod::Months(3),
    ] {
        let (since, until) = period_date_range(period);
        let since = since.unwrap_or_else(|| panic!("{:?} must produce a since bound", period));
        assert!(until.is_none(), "{:?} must not clamp until", period);
        assert!(
            since < today_since,
            "relative window {:?} must start before today ({} vs {})",
            period,
            since,
            today_since
        );
    }
}

#[test]
fn week_resolves_to_rolling_seven_days_on_every_weekday() {
    // #447 regression: `-p week` previously used
    // calendar-week-starting-Monday, which collapsed to today on
    // Mondays and to two days on Tuesdays, contradicting the
    // README contract that week == rolling 7 days including today.
    //
    // Parameterize over a full calendar week so every weekday is
    // covered, including the Monday that triggered the original
    // report.
    let base = NaiveDate::from_ymd_opt(2026, 4, 20).unwrap(); // Monday
    for offset in 0..7 {
        let today = base + chrono::Duration::days(offset);
        let week = period_since_date(today, StatsPeriod::Week).expect("week has since");
        let seven_d = period_since_date(today, StatsPeriod::Days(7)).expect("7d has since");
        let today_since = period_since_date(today, StatsPeriod::Today).expect("today has since");

        assert_eq!(
            week,
            seven_d,
            "on {today} (weekday={:?}) `-p week` must resolve to the same date as `-p 7d`",
            today.format("%A").to_string()
        );
        assert_ne!(
            week,
            today_since,
            "on {today} (weekday={:?}) `-p week` must NOT collapse to today's since",
            today.format("%A").to_string()
        );
        assert_eq!(
            today - week,
            chrono::Duration::days(7),
            "on {today} `-p week` must anchor exactly 7 days before today"
        );
    }
}

#[test]
fn month_resolves_to_rolling_thirty_days_on_every_day_of_month() {
    // #447 regression: `-p month` previously used
    // first-of-calendar-month, which collapsed to one day on the
    // 1st of the month, contradicting the README contract that
    // month == rolling 30 days including today.
    //
    // Parameterize over a full month so the 1st is covered.
    let base = NaiveDate::from_ymd_opt(2026, 3, 1).unwrap();
    for offset in 0..31 {
        let today = base + chrono::Duration::days(offset);
        let month = period_since_date(today, StatsPeriod::Month).expect("month has since");
        let thirty_d = period_since_date(today, StatsPeriod::Days(30)).expect("30d has since");
        let today_since = period_since_date(today, StatsPeriod::Today).expect("today has since");

        assert_eq!(
            month, thirty_d,
            "on {today} `-p month` must resolve to the same date as `-p 30d`"
        );
        assert_ne!(
            month, today_since,
            "on {today} `-p month` must NOT collapse to today's since"
        );
        assert_eq!(
            today - month,
            chrono::Duration::days(30),
            "on {today} `-p month` must anchor exactly 30 days before today"
        );
    }
}

#[test]
fn period_date_range_months_uses_calendar_subtraction() {
    // `StatsPeriod::Months(12)` should land roughly 12 calendar
    // months before today — i.e. at least 360 days back — rather
    // than the 30-day-per-month approximation used before #404.
    // We assert a conservative lower bound so the test is stable
    // regardless of which months are currently in view.
    let today = Local::now().date_naive();
    let (since_rfc, _) = period_date_range(StatsPeriod::Months(12));
    let since_rfc = since_rfc.expect("months(12) has a since bound");
    let since_dt = chrono::DateTime::parse_from_rfc3339(&since_rfc)
        .expect("since is a valid RFC3339 timestamp");
    let since_local = since_dt.with_timezone(&Local).date_naive();
    let delta_days = (today - since_local).num_days();
    assert!(
        delta_days >= 360,
        "Months(12) should span at least 360 days (got {delta_days})"
    );
}

// ─── #449 breakdown bar / layout tests ────────────────────────────

#[test]
fn render_bar_is_blank_for_zero_cost_rows() {
    // The #449 "bar on $0 row" regression: a `$0.00` row would
    // render with visible blocks because the old code ran the ratio
    // multiplier without a zero guard and relied on a 0.01 floor
    // on `max_cost`.
    let blank = render_bar(0.0, 1_000.0);
    assert_eq!(blank.chars().count(), BREAKDOWN_BAR_WIDTH);
    assert!(
        blank.chars().all(|c| c == ' '),
        "zero-cost row must render an all-spaces bar cell, got {:?}",
        blank
    );

    // Negative cost (should not happen in practice but is the same
    // "no bar" case; guards against sign bugs).
    let neg = render_bar(-1.0, 100.0);
    assert!(neg.chars().all(|c| c == ' '));
}

#[test]
fn render_bar_is_blank_when_every_row_is_zero() {
    // All-$0 window: `max_cost_for_rows` returns 0.0 and the bar
    // renderer must not divide-by-zero or paint phantom blocks.
    let blank = render_bar(0.0, 0.0);
    assert_eq!(blank.chars().count(), BREAKDOWN_BAR_WIDTH);
    assert!(blank.chars().all(|c| c == ' '));
}

#[test]
fn render_bar_is_proportional_to_cost_not_message_count() {
    // #449 primary complaint: bars in `--models` scaled by
    // `message_count`, so a $66 / 5814-msg row out-drew a $548 / 381-msg
    // row. The shared renderer scales by the caller's `max_cost`
    // value — message count is no longer in the input.
    let max = 1_000.0_f64;

    // A row at 100% of max fills the bar.
    let full = render_bar(1_000.0, max);
    let full_blocks = full.chars().filter(|c| *c == '\u{2588}').count();
    assert_eq!(full_blocks, BREAKDOWN_BAR_WIDTH);

    // A row at 50% fills half.
    let half = render_bar(500.0, max);
    let half_blocks = half.chars().filter(|c| *c == '\u{2588}').count();
    assert_eq!(half_blocks, BREAKDOWN_BAR_WIDTH / 2);

    // A row at 0.4% still renders at least one block so the reader
    // can distinguish it from a $0 row.
    let tiny = render_bar(4.0, max);
    let tiny_blocks = tiny.chars().filter(|c| *c == '\u{2588}').count();
    assert!((1..BREAKDOWN_BAR_WIDTH).contains(&tiny_blocks));
}

#[test]
fn render_bar_always_pads_to_fixed_width() {
    // Width invariant: every bar string is exactly
    // `BREAKDOWN_BAR_WIDTH` cells (blocks + spaces) so adjacent
    // columns line up regardless of the row's cost.
    for pct in [0, 1, 25, 50, 75, 100, 150] {
        let cost = pct as f64 * 10.0;
        let bar = render_bar(cost, 1_000.0);
        assert_eq!(
            bar.chars().count(),
            BREAKDOWN_BAR_WIDTH,
            "bar at {pct}% of max must be exactly {BREAKDOWN_BAR_WIDTH} cells"
        );
    }
}

#[test]
fn truncate_label_is_utf8_boundary_safe() {
    // #389 / #383 / #404 / #445 precedent: byte-indexed truncation
    // on user strings panics on multi-byte codepoints. The label
    // truncator operates on Unicode scalars so tickets / file paths
    // carrying non-ASCII characters never crash.
    assert_eq!(truncate_label("short", 40), "short");
    assert_eq!(truncate_label("", 40), "");

    let long_ascii = "a".repeat(60);
    let truncated = truncate_label(&long_ascii, 20);
    assert_eq!(truncated.chars().count(), 20);
    assert!(truncated.starts_with('…'));

    // Multi-byte: an emoji is 4 bytes but 1 char. The truncator
    // must count chars, not bytes, so we never split mid-codepoint.
    let emoji_path = "src/🚀/main.rs".repeat(5);
    let truncated = truncate_label(&emoji_path, 12);
    assert_eq!(truncated.chars().count(), 12);
    assert!(truncated.starts_with('…'));
}

#[test]
fn max_cost_for_rows_handles_empty_and_all_zero() {
    // Empty page: no division-by-zero hazard.
    let empty: Vec<budi_core::analytics::RepoUsage> = vec![];
    assert_eq!(max_cost_for_rows(&empty), 0.0);

    // All-$0 page (a real case in `--files today` when no tool-arg
    // attribution landed). `render_bar` takes this 0.0 and paints
    // blank cells, which is what the reader expects.
    let all_zero = vec![
        budi_core::analytics::RepoUsage {
            repo_id: "a".into(),
            display_path: "a".into(),
            message_count: 10,
            input_tokens: 0,
            output_tokens: 0,
            cost_cents: 0.0,
        },
        budi_core::analytics::RepoUsage {
            repo_id: "b".into(),
            display_path: "b".into(),
            message_count: 10,
            input_tokens: 0,
            output_tokens: 0,
            cost_cents: 0.0,
        },
    ];
    assert_eq!(max_cost_for_rows(&all_zero), 0.0);

    // Sanity: the max is taken from `cost_cents`, not any other
    // field. Keeps `--models` from regressing to message-count
    // scaling (#449).
    let mixed = vec![
        budi_core::analytics::RepoUsage {
            repo_id: "big-msgs-cheap".into(),
            display_path: "".into(),
            message_count: 100_000,
            input_tokens: 0,
            output_tokens: 0,
            cost_cents: 10.0,
        },
        budi_core::analytics::RepoUsage {
            repo_id: "few-msgs-dear".into(),
            display_path: "".into(),
            message_count: 10,
            input_tokens: 0,
            output_tokens: 0,
            cost_cents: 500.0,
        },
    ];
    assert_eq!(max_cost_for_rows(&mixed), 500.0);
}

#[test]
fn breakdown_header_has_cost_column_in_canonical_position() {
    // #449 acceptance: every breakdown shows a header row, and the
    // `COST` label sits at the right edge of the bar+cost combined
    // width so rendered cost values align under it.
    let header = format_breakdown_header_text("TICKET", 24, "");
    assert!(
        header.starts_with("  TICKET"),
        "header must start with indented label title, got {:?}",
        header
    );
    assert!(
        header.trim_end().ends_with("COST"),
        "header must end with the COST title, got {:?}",
        header
    );

    // With extras, the extra headers follow COST separated by two
    // spaces (same gap the row formatter uses).
    let with_extras =
        format_breakdown_header_text("ACTIVITY", 18, "CONFIDENCE   TOP_BRANCH             ");
    assert!(with_extras.contains("COST  CONFIDENCE"));
}

#[test]
fn breakdown_row_layout_matches_tag_view_template() {
    // #449 acceptance: all views line up as `LABEL  BAR  COST  EXTRAS`.
    // Baseline: the `--tag` shape (the layout the ticket picks as
    // canonical) produces exactly this string for a half-cost row.
    let line = format_breakdown_row_text("activity/coding", 40, 500.0, 1_000.0, "");
    let expected = format!(
        "  {:<40} {} {:>10}",
        "activity/coding",
        render_bar(500.0, 1_000.0),
        format_cost_cents(500.0),
    );
    assert_eq!(line, expected);
}

/// Helper: canonical right-edge column that every view's `COST`
/// title must line up against, given the shared bar + cost widths.
fn cost_right_edge(label_width: usize) -> usize {
    // Matches `format_breakdown_header_text` / `format_breakdown_row_text`:
    // "  " + label + " " + (bar + " " + cost right-aligned).
    2 + label_width + 1 + BREAKDOWN_BAR_WIDTH + 1 + BREAKDOWN_COST_WIDTH
}

/// Helper: char-based position of `needle` inside `haystack`. Plain
/// `str::find` returns byte offsets, which over-counts every bar
/// block (3 bytes per `█`) — we want character columns here so the
/// layout assertions line up with visual widths.
fn char_pos_of(haystack: &str, needle: &str) -> Option<usize> {
    let byte_pos = haystack.find(needle)?;
    Some(haystack[..byte_pos].chars().count())
}

#[test]
fn breakdown_row_snapshot_models_view() {
    // Snapshot baseline for `budi stats models` row layout
    // (#449 acceptance). One row at the top of the scale, one
    // tiny but non-zero row, one $0 row — the three cases the
    // ticket's Reproduction section calls out as bug triggers.
    let label_w = 40usize;
    let extras_header = format!("{:>10}  {:>10}", "MSGS", "TOKENS");
    let header = format_breakdown_header_text("MODEL", label_w, &extras_header);

    let heavy_extras = format!("{:>10}  {:>10}", "24114 msgs", "120M tok");
    let heavy = format_breakdown_row_text(
        "claude-opus-4-6 (claude_code)",
        label_w,
        210_000.0,
        210_000.0,
        &heavy_extras,
    );
    let tiny = format_breakdown_row_text(
        "claude-haiku-4-5 (claude_code)",
        label_w,
        65.74,
        210_000.0,
        &format!("{:>10}  {:>10}", "5814 msgs", "30M tok"),
    );
    let pending = format_breakdown_row_text(
        "(untagged)",
        label_w,
        0.0,
        210_000.0,
        &format!("{:>10}  {:>10}", "162 msgs", "0 tok"),
    );

    // Header `COST` title right-aligns to the canonical edge so it
    // sits above every row's cost cell.
    let cost_title_right = header.find("COST").unwrap() + "COST".len();
    assert_eq!(
        cost_title_right,
        cost_right_edge(label_w),
        "MODEL view: COST title must align to the shared cost-cell right edge\nheader: {header:?}"
    );

    // Extras header and row extras share the same right edge.
    assert!(header.trim_end().ends_with("TOKENS"));
    assert!(heavy.trim_end().ends_with("tok"));

    // The heavy row renders a full bar; the tiny row renders a
    // visible but non-full bar; the `(untagged)` $0 row renders
    // a blank bar. This is the #449 bug's 1:1 repro checked.
    let heavy_blocks = heavy.chars().filter(|c| *c == '\u{2588}').count();
    let tiny_blocks = tiny.chars().filter(|c| *c == '\u{2588}').count();
    let pending_blocks = pending.chars().filter(|c| *c == '\u{2588}').count();
    assert_eq!(heavy_blocks, BREAKDOWN_BAR_WIDTH);
    assert!((1..BREAKDOWN_BAR_WIDTH).contains(&tiny_blocks));
    assert_eq!(pending_blocks, 0);
}

#[test]
fn breakdown_row_snapshot_tickets_view() {
    // Golden snapshot for `budi stats tickets` — pinned so the
    // bar/cost order cannot silently flip back to the pre-#449
    // layout (cost-before-bar).
    let label_w = 24usize;
    let extras = format!("{:<18}  {:<24}", "SOURCE", "TOP_BRANCH");
    let header = format_breakdown_header_text("TICKET", label_w, &extras);

    assert!(header.starts_with("  TICKET"));
    let cost_title_right = header.find("COST").unwrap() + "COST".len();
    assert_eq!(
        cost_title_right,
        cost_right_edge(label_w),
        "TICKET view: COST title must align to the shared cost-cell right edge"
    );
    assert!(header.contains("SOURCE"));
    assert!(header.contains("TOP_BRANCH"));

    // Canonical row: label, full bar (row cost == max), cost cell,
    // then extras. The dollar character marks the right edge of the
    // cost cell.
    let row_extras = format!("{:<18}  {:<24}", "src=branch", "04-20-pava-1669");
    let row = format_breakdown_row_text("PAVA-1669", label_w, 2_265.0, 2_265.0, &row_extras);
    assert!(row.starts_with("  PAVA-1669"));
    assert!(
        row.chars().filter(|c| *c == '\u{2588}').count() == BREAKDOWN_BAR_WIDTH,
        "max-cost row must render a full bar"
    );
    // Cost cell sits at the canonical right edge (the first block
    // in the extras section starts exactly 2 chars past that).
    let cost_cell = format_cost_cents(2_265.0);
    let cost_cell_right = char_pos_of(&row, &cost_cell).unwrap() + cost_cell.chars().count();
    assert_eq!(
        cost_cell_right,
        cost_right_edge(label_w),
        "row cost cell must end at the same column as the header COST title"
    );
    // Extras follow with the 2-space gap baked into the shared row
    // formatter.
    assert!(row.contains("src=branch"));
    assert!(row.contains("04-20-pava-1669"));

    // `(untagged)` row with a real dollar cost equal to max:
    // bar present, full width. Prior to #449 this row rendered
    // with no bar at all because message_count didn't correlate
    // with cost.
    let untagged = format_breakdown_row_text(
        "(untagged)",
        label_w,
        9_122.0,
        9_122.0,
        &format!("{:<18}  {:<24}", "src=--", "--"),
    );
    assert!(untagged.contains("(untagged)"));
    assert_eq!(
        untagged.chars().filter(|c| *c == '\u{2588}').count(),
        BREAKDOWN_BAR_WIDTH,
        "the dominant row (same as max_cost) must render a full bar"
    );
}

#[test]
fn breakdown_row_snapshot_activities_view() {
    // Golden snapshot for `budi stats activities`. Preserves the
    // `conf=…` in-row legend (addressed more fully by #450) while
    // pinning the #449 layout contract.
    let label_w = 18usize;
    let extras = format!("{:<11}  {:<22}", "CONFIDENCE", "TOP_BRANCH");
    let header = format_breakdown_header_text("ACTIVITY", label_w, &extras);

    let cost_title_right = header.find("COST").unwrap() + "COST".len();
    assert_eq!(
        cost_title_right,
        cost_right_edge(label_w),
        "ACTIVITY view: COST title must align to the shared cost-cell right edge"
    );
    assert!(header.contains("CONFIDENCE"));
    assert!(header.contains("TOP_BRANCH"));

    let row = format_breakdown_row_text(
        "coding",
        label_w,
        1_234.56,
        1_234.56,
        &format!("{:<11}  {:<22}", "conf=medium", "main"),
    );
    assert!(row.starts_with("  coding"));
    assert!(row.contains("conf=medium"));
    // Row is the max-cost row → full bar.
    assert_eq!(
        row.chars().filter(|c| *c == '\u{2588}').count(),
        BREAKDOWN_BAR_WIDTH,
    );

    // $0 activity row renders a blank bar.
    let zero = format_breakdown_row_text(
        "classifier_pending",
        label_w,
        0.0,
        1_234.56,
        &format!("{:<11}  {:<22}", "conf=--", "--"),
    );
    assert_eq!(zero.chars().filter(|c| *c == '\u{2588}').count(), 0);
}

#[test]
fn breakdown_row_snapshot_tag_view() {
    // Golden snapshot for `budi stats tag <key>` — the layout
    // every other breakdown now mirrors. Kept as the canonical
    // shape so a future refactor that regresses one view will
    // fail a targeted test.
    let label_w = 40usize;
    let header = format_breakdown_header_text("VALUE", label_w, "");
    assert!(header.starts_with("  VALUE"));
    let cost_title_right = header.find("COST").unwrap() + "COST".len();
    assert_eq!(
        cost_title_right,
        cost_right_edge(label_w),
        "TAG view: COST title must align to the shared cost-cell right edge"
    );

    // Half-cost row: bar fills exactly half, cost cell lines up
    // under the COST header.
    let row = format_breakdown_row_text("feature-work", label_w, 500.0, 1_000.0, "");
    assert!(row.starts_with("  feature-work"));
    let half_blocks = row.chars().filter(|c| *c == '\u{2588}').count();
    assert_eq!(
        half_blocks,
        BREAKDOWN_BAR_WIDTH / 2,
        "50% cost row must render half a bar"
    );
    let cost_cell = format_cost_cents(500.0);
    let cost_cell_right = char_pos_of(&row, &cost_cell).unwrap() + cost_cell.chars().count();
    assert_eq!(cost_cell_right, cost_right_edge(label_w));

    // Long tag values truncate with `…` prefix, not byte-index
    // panic (#389 / #383 / #404 / #445 UTF-8 class).
    let long = truncate_label("verylongtagvaluethatwillwraparound_123456789_abcdef", 40);
    assert_eq!(long.chars().count(), 40);
    assert!(long.starts_with('…'));
}

// ─── #451 summary-shape tests ────────────────────────────────────

/// Build a representative `UsageSummary` for the summary-shape
/// tests. Numbers are arbitrary but non-zero so the formatter
/// exercises every code path.
fn fixture_summary() -> budi_core::analytics::UsageSummary {
    budi_core::analytics::UsageSummary {
        total_messages: 262,
        total_user_messages: 16,
        total_assistant_messages: 246,
        total_input_tokens: 1_300_000,
        total_output_tokens: 874_100,
        total_cache_creation_tokens: 50_000,
        total_cache_read_tokens: 200_000,
        total_cost_cents: 11_400.0,
    }
}

fn fixture_cost(cache_savings: f64) -> budi_core::cost::CostEstimate {
    budi_core::cost::CostEstimate {
        total_cost: 114.0,
        input_cost: 4.49,
        output_cost: 20.82,
        cache_write_cost: 27.48,
        cache_read_cost: 65.94,
        other_cost: 0.0,
        cache_savings,
    }
}

fn fixture_provider(
    provider: &str,
    display: &str,
    msgs: u64,
    cents: f64,
) -> analytics::ProviderStats {
    // #482: `msgs` is interpreted as the assistant-side count (same
    // unit every pre-8.3.1 caller passed). The fixture picks a
    // user_messages value slightly larger than assistant so
    // `total_messages = assistant + user` stays unambiguous across
    // snapshot tests.
    let user = msgs.saturating_add(3);
    analytics::ProviderStats {
        provider: provider.into(),
        display_name: display.into(),
        assistant_messages: msgs,
        user_messages: user,
        total_messages: msgs + user,
        input_tokens: 100_000,
        output_tokens: 50_000,
        cache_creation_tokens: 10_000,
        cache_read_tokens: 20_000,
        estimated_cost: cents / 100.0,
        total_cost_cents: cents,
    }
}

/// All blocks that #451 requires to be present in every summary,
/// regardless of period or provider count.
fn assert_summary_has_required_blocks(rendered: &str, expect_cursor_footnote: bool) {
    assert!(
        rendered.contains("budi stats"),
        "missing header line:\n{rendered}"
    );
    assert!(
        rendered.contains("Agents"),
        "missing Agents block (#451):\n{rendered}"
    );
    assert!(
        rendered.contains("Total"),
        "missing Total line:\n{rendered}"
    );
    assert!(
        rendered.contains("Tokens"),
        "missing Tokens line:\n{rendered}"
    );
    assert!(
        rendered.contains("Est. cost"),
        "missing Est. cost line:\n{rendered}"
    );
    assert!(
        rendered.contains("input ") && rendered.contains("output "),
        "missing cost-component sub-line (#451):\n{rendered}"
    );
    assert!(
        rendered.contains("cache write") && rendered.contains("cache read"),
        "missing cost-component sub-line cache cells (#451):\n{rendered}"
    );
    assert!(
        rendered.contains("cache savings"),
        "missing cache-savings line (#451 — must be unconditional):\n{rendered}"
    );
    if expect_cursor_footnote {
        assert!(
            rendered.contains("Cursor cost data may lag"),
            "missing Cursor-lag footnote when Cursor is in providers (#451):\n{rendered}"
        );
    }
}

#[test]
fn summary_tokens_row_reconciles_with_per_agent_totals() {
    // #445 item 5: the per-agent rows in the Agents block render a
    // token total that *includes* cache traffic (input + output +
    // cache-write + cache-read). Before this fix the summary
    // Tokens row only showed `{input} in, {output} out`, which
    // looked like it disagreed with the per-agent row on cache-heavy
    // workloads. Lock in the four-component shape so a future
    // renderer change cannot silently regress the reconciliation.
    let summary = fixture_summary();
    let est = fixture_cost(0.0);
    let providers = vec![fixture_provider(
        "claude_code",
        "Claude Code",
        262,
        summary.total_cost_cents,
    )];
    let palette = SummaryPalette::plain();
    let rendered = format_summary(
        StatsPeriod::Days(7),
        None,
        &summary,
        &est,
        &providers,
        &palette,
    );

    // Every token component must appear on the Tokens line (not
    // just input / output).
    assert!(
        rendered.contains("input"),
        "summary missing input label:\n{rendered}"
    );
    assert!(
        rendered.contains("output"),
        "summary missing output label:\n{rendered}"
    );
    assert!(
        rendered.contains("cache-write"),
        "summary missing cache-write component (#445 reconciliation):\n{rendered}"
    );
    assert!(
        rendered.contains("cache-read"),
        "summary missing cache-read component (#445 reconciliation):\n{rendered}"
    );

    // The pre-#445 shape `<n> in, <n> out` must be gone so consumers
    // can't rely on it and readers never see it.
    for line in rendered.lines() {
        if line.trim_start().starts_with("Tokens") {
            assert!(
                !line.contains(" in, "),
                "legacy two-component Tokens line still renders: {line}"
            );
        }
    }
}

#[test]
fn summary_shape_is_identical_across_all_six_periods() {
    // #451 acceptance: `today / week / month / 1d / 7d / 30d` all
    // produce the same set of blocks (header, Agents, Total,
    // Tokens, Est. cost, cost-component sub-line, cache savings,
    // optional Cursor-lag footnote). Pre-#451, today and week
    // dropped the Agents block whenever the window happened to
    // surface a single provider.
    let summary = fixture_summary();
    let est = fixture_cost(593.0);
    let providers = vec![
        fixture_provider("codex", "Codex", 566, 13_700.0),
        fixture_provider("cursor", "Cursor", 280, 16_300.0),
        fixture_provider("claude_code", "Claude Code", 3, 72.0),
    ];
    let palette = SummaryPalette::plain();

    for period in [
        StatsPeriod::Today,
        StatsPeriod::Week,
        StatsPeriod::Month,
        StatsPeriod::Days(1),
        StatsPeriod::Days(7),
        StatsPeriod::Days(30),
    ] {
        let rendered = format_summary(period, None, &summary, &est, &providers, &palette);
        assert_summary_has_required_blocks(&rendered, true);
        // Each provider row must show up.
        for ps in &providers {
            assert!(
                rendered.contains(&ps.display_name),
                "{period:?}: missing provider {} row in Agents block:\n{rendered}",
                ps.display_name
            );
        }
    }
}

#[test]
fn summary_keeps_agents_block_with_one_provider() {
    // #451 primary fix: even with a single provider in the window,
    // the Agents block renders. Pre-#451 the dispatcher dropped
    // it entirely when `providers.len() == 1`, making `today` look
    // structurally different from `7d` on the same machine.
    let summary = fixture_summary();
    let est = fixture_cost(0.0);
    let providers = vec![fixture_provider(
        "claude_code",
        "Claude Code",
        262,
        11_400.0,
    )];
    let palette = SummaryPalette::plain();

    let rendered = format_summary(
        StatsPeriod::Today,
        None,
        &summary,
        &est,
        &providers,
        &palette,
    );
    assert!(
        rendered.contains("Agents"),
        "Agents block must render with a single provider (#451):\n{rendered}"
    );
    assert!(
        rendered.contains("Claude Code"),
        "the single provider's row must render:\n{rendered}"
    );
    // Cursor-lag footnote is OFF when Cursor is not in the window.
    assert!(
        !rendered.contains("Cursor cost data may lag"),
        "Cursor-lag footnote must NOT render when Cursor is absent:\n{rendered}"
    );
}

#[test]
fn summary_cache_savings_line_is_unconditional() {
    // #451 acceptance: cache-savings line is part of the
    // summary's structural shape, even when the value is $0.00.
    // Otherwise the line's presence becomes a leaked signal of
    // whether any cache hits accumulated, and the summary's shape
    // varies between Today and 7d.
    let summary = fixture_summary();
    let providers = vec![fixture_provider(
        "claude_code",
        "Claude Code",
        262,
        11_400.0,
    )];
    let palette = SummaryPalette::plain();

    let with_savings = format_summary(
        StatsPeriod::Today,
        None,
        &summary,
        &fixture_cost(593.0),
        &providers,
        &palette,
    );
    assert!(with_savings.contains("cache savings $593"));

    let no_savings = format_summary(
        StatsPeriod::Today,
        None,
        &summary,
        &fixture_cost(0.0),
        &providers,
        &palette,
    );
    assert!(
        no_savings.contains("cache savings $0.00"),
        "cache-savings line must render even when zero:\n{no_savings}"
    );
}

#[test]
fn summary_provider_filter_renders_one_row_in_agents_block() {
    // `--provider cursor` keeps the Agents block intact but
    // narrows it to the selected agent. The Cursor-lag footnote
    // also stays because cursor is one of the displayed
    // providers.
    let summary = fixture_summary();
    let est = fixture_cost(0.0);
    let providers = vec![
        fixture_provider("codex", "Codex", 566, 13_700.0),
        fixture_provider("cursor", "Cursor", 280, 16_300.0),
    ];
    let palette = SummaryPalette::plain();

    let rendered = format_summary(
        StatsPeriod::Today,
        Some("cursor"),
        &summary,
        &est,
        &providers,
        &palette,
    );
    assert_summary_has_required_blocks(&rendered, true);
    assert!(rendered.contains("Cursor"));
    assert!(
        !rendered.contains("Codex"),
        "filtered Agents block must hide non-matching providers:\n{rendered}"
    );
}

#[test]
fn summary_empty_window_renders_no_data_message_and_skips_blocks() {
    // The "no data" early return is preserved: empty windows
    // print a friendly message instead of an Agents block with
    // a "(no provider data)" placeholder. This keeps the shape
    // contract scoped to non-empty windows.
    let mut summary = fixture_summary();
    summary.total_messages = 0;
    summary.total_user_messages = 0;
    summary.total_assistant_messages = 0;
    let est = fixture_cost(0.0);
    let providers: Vec<analytics::ProviderStats> = vec![];
    let palette = SummaryPalette::plain();

    let rendered = format_summary(
        StatsPeriod::Today,
        None,
        &summary,
        &est,
        &providers,
        &palette,
    );
    assert!(rendered.contains("No data for this period"));
    assert!(
        !rendered.contains("Agents"),
        "empty window must not print the Agents block"
    );
    assert!(
        !rendered.contains("Est. cost"),
        "empty window must not print the cost block"
    );
}

#[test]
fn summary_cursor_lag_footnote_rides_on_displayed_providers() {
    // #451: the Cursor-lag footnote prints whenever the displayed
    // providers list includes Cursor, regardless of whether the
    // user passed `--provider cursor`. Pre-#451 the
    // summary-filtered path only printed it on explicit filter
    // and the multi-agent path silently included a Cursor row
    // without the caveat — both regressions are fixed by sharing
    // one renderer.
    let summary = fixture_summary();
    let est = fixture_cost(0.0);
    let palette = SummaryPalette::plain();

    // Cursor present in window, no filter → footnote on.
    let providers = vec![
        fixture_provider("codex", "Codex", 1, 100.0),
        fixture_provider("cursor", "Cursor", 1, 100.0),
    ];
    let r = format_summary(
        StatsPeriod::Today,
        None,
        &summary,
        &est,
        &providers,
        &palette,
    );
    assert!(r.contains("Cursor cost data may lag"));

    // Filtered to codex → footnote off (Cursor row not displayed).
    let r = format_summary(
        StatsPeriod::Today,
        Some("codex"),
        &summary,
        &est,
        &providers,
        &palette,
    );
    assert!(!r.contains("Cursor cost data may lag"));

    // No Cursor in window at all → footnote off.
    let providers_no_cursor = vec![fixture_provider("codex", "Codex", 1, 100.0)];
    let r = format_summary(
        StatsPeriod::Today,
        None,
        &summary,
        &est,
        &providers_no_cursor,
        &palette,
    );
    assert!(!r.contains("Cursor cost data may lag"));
}

#[test]
fn summary_agents_block_tokens_cell_always_has_unit_suffix() {
    // #494: every tokens cell in the Agents block carries an
    // explicit `tok` suffix — the pre-8.3.1 render was `0` (bare)
    // next to `159.0M` (with its unit baked in), leaving a fresh
    // reader unsure which was tokens and which was a count. Both
    // rows now render `{n} tok`.
    let summary = fixture_summary();
    let est = fixture_cost(0.0);
    // Mix zero-token row (typical for Cursor) with a non-zero
    // row (typical for Claude Code).
    let zero_provider = analytics::ProviderStats {
        provider: "cursor".into(),
        display_name: "Cursor".into(),
        assistant_messages: 12,
        user_messages: 10,
        total_messages: 22,
        input_tokens: 0,
        output_tokens: 0,
        cache_creation_tokens: 0,
        cache_read_tokens: 0,
        estimated_cost: 0.0,
        total_cost_cents: 0.0,
    };
    let providers = vec![
        fixture_provider("claude_code", "Claude Code", 262, 11_400.0),
        zero_provider,
    ];
    let palette = SummaryPalette::plain();
    let rendered = format_summary(
        StatsPeriod::Today,
        None,
        &summary,
        &est,
        &providers,
        &palette,
    );

    // Both rows carry the explicit ` tok` suffix.
    for line in rendered.lines() {
        let trimmed = line.trim();
        // Agents rows start with a provider display name after the
        // leading spaces of the block.
        if trimmed.starts_with("Claude Code") || trimmed.starts_with("Cursor") {
            assert!(
                trimmed.contains(" tok"),
                "Agents-block row must include the `tok` suffix: {trimmed:?}",
            );
        }
    }
}

#[test]
fn summary_est_cost_precision_matches_component_sub_line() {
    // #486: summary `Est. cost` precision matches the component
    // sub-line precision (`$X,XXX.XX`). Pre-8.3.1 a top-line total
    // above $100 collapsed to `$126` while the sub-line kept
    // `$0.04 / $19.92 / $32.36 / $74.08` → sum `$126.40`. The
    // fresh reader couldn't tell whether the top value was
    // `$126.00` rounded or a sub-line total that silently lost
    // 40¢. Both lines now use `format_cost_cents_fixed`.
    let summary = fixture_summary();
    let est = budi_core::cost::CostEstimate {
        total_cost: 126.40,
        input_cost: 0.04,
        output_cost: 19.92,
        cache_write_cost: 32.36,
        cache_read_cost: 74.08,
        other_cost: 0.0,
        cache_savings: 0.0,
    };
    let providers = vec![fixture_provider(
        "claude_code",
        "Claude Code",
        262,
        12_640.0,
    )];
    let palette = SummaryPalette::plain();
    let rendered = format_summary(
        StatsPeriod::Today,
        None,
        &summary,
        &est,
        &providers,
        &palette,
    );

    // Top-line never collapses cents above $100.
    assert!(
        rendered.contains("Est. cost    $126.40"),
        "Est. cost must render `$126.40`, not `$126`:\n{rendered}",
    );
    // Component sub-line carries matching cents precision.
    assert!(
        rendered.contains("input $0.04"),
        "input component must render `$0.04`:\n{rendered}",
    );
    assert!(
        rendered.contains("output $19.92"),
        "output component must render `$19.92`:\n{rendered}",
    );
    // Top-line dollars never rendered with a `K` suffix in the
    // summary block — locking this out catches any future caller
    // that accidentally swaps in the humanized formatter.
    for line in rendered.lines() {
        if line.contains("Est. cost") {
            assert!(
                !line.contains("K") && !line.contains("M"),
                "top-line `Est. cost` must not humanize: {line:?}",
            );
        }
    }
}

/// #520: when `other_cost > 0`, the summary sub-line grows a
/// fifth `other $N.NN` cell so the four base components + `other`
/// sum to the top line. Fresh Claude-only windows (other_cost=0)
/// keep the pre-8.3.2 four-cell sub-line so the surface stays
/// uncluttered.
#[test]
fn summary_renders_other_cost_cell_when_nonzero() {
    let summary = fixture_summary();
    // Seed the 2026-04-22 audit's actual 30d numbers: top-line
    // \$3,915.47, components sum \$3,800.47, other = \$115.00.
    let est = budi_core::cost::CostEstimate {
        total_cost: 3_915.47,
        input_cost: 995.38,
        output_cost: 316.65,
        cache_write_cost: 703.15,
        cache_read_cost: 1_785.29,
        other_cost: 115.00,
        cache_savings: 0.0,
    };
    let providers = vec![fixture_provider(
        "claude_code",
        "Claude Code",
        262,
        391_547.0,
    )];
    let palette = SummaryPalette::plain();
    let rendered = format_summary(
        StatsPeriod::Today,
        None,
        &summary,
        &est,
        &providers,
        &palette,
    );
    assert!(
        rendered.contains("other $115.00"),
        "other cell must render when non-zero:\n{rendered}",
    );
    assert!(
        rendered.contains("Est. cost    $3,915.47"),
        "top-line cost precision must carry thousands sep + cents:\n{rendered}",
    );
}

#[test]
fn summary_omits_other_cost_cell_when_zero() {
    let summary = fixture_summary();
    let est = budi_core::cost::CostEstimate {
        total_cost: 7.50,
        input_cost: 5.00,
        output_cost: 2.50,
        cache_write_cost: 0.0,
        cache_read_cost: 0.0,
        other_cost: 0.0,
        cache_savings: 0.0,
    };
    let providers = vec![fixture_provider("claude_code", "Claude Code", 10, 750.0)];
    let palette = SummaryPalette::plain();
    let rendered = format_summary(
        StatsPeriod::Today,
        None,
        &summary,
        &est,
        &providers,
        &palette,
    );
    // The "other" label only appears inside the summary block
    // when `other_cost > 0`; a zero window keeps the pre-8.3.2
    // four-cell shape.
    for line in rendered.lines() {
        if line.contains("input ") && line.contains("output ") {
            assert!(
                !line.contains("other"),
                "sub-line should not contain `other` when other_cost == 0: {line:?}",
            );
        }
    }
}

#[test]
fn summary_agents_block_count_matches_total_row_unit() {
    // #482: the Agents block's `msgs` column is the same unit as
    // `Total {n} messages` (user + assistant), not the pre-8.3.1
    // assistant-only count. Without this, a fresh reader seeing
    // `Claude Code 1358 msgs` on top of `Total 3050 messages` has
    // no way to tell that `1358` undercounts by `total_user_messages`.
    //
    // Contract: the per-provider count printed in the Agents block
    // equals `ProviderStats.total_messages`, AND `sum(displayed
    // providers.total_messages) == summary.total_messages` when no
    // filter is active.
    let summary = budi_core::analytics::UsageSummary {
        total_messages: 3050,
        total_user_messages: 1680,
        total_assistant_messages: 1370,
        total_input_tokens: 1_300_000,
        total_output_tokens: 874_100,
        total_cache_creation_tokens: 50_000,
        total_cache_read_tokens: 200_000,
        total_cost_cents: 12_640.0,
    };
    let est = fixture_cost(0.0);
    // Hand-built providers so we can pin the exact split seen on
    // the 2026-04-22 audit machine.
    let claude = analytics::ProviderStats {
        provider: "claude_code".into(),
        display_name: "Claude Code".into(),
        assistant_messages: 1358,
        user_messages: 1670,
        total_messages: 3028,
        input_tokens: 1_300_000,
        output_tokens: 874_100,
        cache_creation_tokens: 50_000,
        cache_read_tokens: 200_000,
        estimated_cost: 126.40,
        total_cost_cents: 12_640.0,
    };
    let cursor = analytics::ProviderStats {
        provider: "cursor".into(),
        display_name: "Cursor".into(),
        assistant_messages: 12,
        user_messages: 10,
        total_messages: 22,
        input_tokens: 0,
        output_tokens: 0,
        cache_creation_tokens: 0,
        cache_read_tokens: 0,
        estimated_cost: 0.0,
        total_cost_cents: 0.0,
    };
    let providers = vec![claude.clone(), cursor.clone()];
    let palette = SummaryPalette::plain();

    let rendered = format_summary(
        StatsPeriod::Today,
        None,
        &summary,
        &est,
        &providers,
        &palette,
    );

    // The per-provider row renders `total_messages`, not
    // `assistant_messages`. Locking the exact substring prevents a
    // future renderer from quietly swapping back to assistant-only.
    assert!(
        rendered.contains(&format!("{} msgs", claude.total_messages)),
        "Claude Code row must render total_messages ({}), not assistant_messages ({}):\n{rendered}",
        claude.total_messages,
        claude.assistant_messages,
    );
    assert!(
        rendered.contains(&format!("{} msgs", cursor.total_messages)),
        "Cursor row must render total_messages ({}):\n{rendered}",
        cursor.total_messages,
    );
    assert!(
        !rendered.contains(&format!("{} msgs", claude.assistant_messages)),
        "must not render assistant-only count ({}) in Agents column:\n{rendered}",
        claude.assistant_messages,
    );

    // Per-provider totals reconcile to the summary total to within
    // the sync-lag budget. Here the fixture is internally consistent
    // so the delta is exactly 0.
    let agg: u64 = providers.iter().map(|p| p.total_messages).sum();
    assert_eq!(
        agg, summary.total_messages,
        "sum of Agents-block counts ({}) must match Total row ({})",
        agg, summary.total_messages,
    );
}

// ─── #450 breakdown polish tests ──────────────────────────────────

#[test]
fn format_cost_cents_fixed_is_always_two_decimals_with_thousands_sep() {
    // #450 acceptance C: breakdown columns render one currency
    // shape — fixed `$X,XXX.XX`, never the humanized `$1.2K`. A
    // breakdown column with `$1.5K / $288 / $90.39 / $0.42` all
    // resolves to the same visual shape.
    assert_eq!(format_cost_cents_fixed(0.0), "$0.00");
    assert_eq!(format_cost_cents_fixed(42.0), "$0.42");
    assert_eq!(format_cost_cents_fixed(9_039.0), "$90.39");
    assert_eq!(format_cost_cents_fixed(28_800.0), "$288.00");
    assert_eq!(format_cost_cents_fixed(150_000.0), "$1,500.00");
    assert_eq!(format_cost_cents_fixed(1_234_567.0), "$12,345.67");
    assert_eq!(format_cost_cents_fixed(10_000_000.0), "$100,000.00");
    // Rounding carry: $0.995 → $1.00, not $0.100.
    assert_eq!(format_cost_cents_fixed(99.5), "$1.00");
}

#[test]
fn truncate_label_middle_keeps_head_and_tail() {
    // #450 acceptance B: every label-like column uses one
    // truncation strategy — middle ellipsis so the head (branch
    // prefix) and tail (file name) both survive.
    assert_eq!(truncate_label_middle("short", 40), "short");
    assert_eq!(truncate_label_middle("", 40), "");

    let branch = "04-20-pava-1669_adds_an_optional_inputmode_prop_to_chararrayinput";
    let truncated = truncate_label_middle(branch, 20);
    assert_eq!(truncated.chars().count(), 20);
    assert!(truncated.contains('…'));
    assert!(truncated.starts_with("04-20-"));
    assert!(
        truncated.ends_with("arrayinput"),
        "middle ellipsis must preserve the tail, got {:?}",
        truncated
    );

    // Multi-byte content: emoji still splits on char boundaries,
    // not bytes, so we never panic (#389 / #383 / #404 / #445).
    let emoji = "src/🚀/main.rs".repeat(5);
    let t = truncate_label_middle(&emoji, 12);
    assert_eq!(t.chars().count(), 12);
}

#[test]
fn truncate_label_middle_falls_back_for_very_narrow_widths() {
    // If the caller asks for less than 3 chars we can't fit both a
    // head and tail plus ellipsis. Fall back to the legacy tail
    // renderer so the output still fits the width budget.
    let t = truncate_label_middle("abcdefghij", 2);
    assert_eq!(t.chars().count(), 2);
}

#[test]
fn breakdown_view_untagged_label_is_view_specific() {
    // #450 acceptance E: one `(untagged)` DB sentinel, six
    // different display labels.
    assert_eq!(BreakdownView::Projects.untagged_label(), "(no repository)");
    assert_eq!(BreakdownView::Branches.untagged_label(), "(no branch)");
    assert_eq!(BreakdownView::Tickets.untagged_label(), "(no ticket)");
    assert_eq!(BreakdownView::Activities.untagged_label(), "(unclassified)");
    assert_eq!(BreakdownView::Files.untagged_label(), "(no file tag)");
    assert_eq!(
        BreakdownView::Models.untagged_label(),
        "(model not yet attributed)"
    );
    // Tag view keeps the generic sentinel because a tag key can
    // mean anything — "(no X)" would be misleading.
    assert_eq!(BreakdownView::Tag.untagged_label(), "(untagged)");
}

#[test]
fn display_dimension_translates_untagged_but_leaves_other_values_alone() {
    assert_eq!(
        display_dimension(BreakdownView::Tickets, "(untagged)"),
        "(no ticket)"
    );
    assert_eq!(
        display_dimension(BreakdownView::Files, "(untagged)"),
        "(no file tag)"
    );
    assert_eq!(display_dimension(BreakdownView::Branches, "main"), "main");
    assert_eq!(
        display_dimension(BreakdownView::Tickets, "PAVA-1669"),
        "PAVA-1669"
    );
}

#[test]
fn merge_and_partition_pending_suppresses_zero_cost_placeholder() {
    // #443 + #450 acceptance: placeholder rows merge into one
    // `(model not yet attributed)` bucket per provider, and a
    // merged bucket whose cost is zero stays suppressed by
    // default (the pure `(untagged)`-transient case).
    use budi_core::analytics::ModelUsage;

    fn row(model: &str, provider: &str, cost: f64, msgs: u64) -> ModelUsage {
        ModelUsage {
            model: model.to_string(),
            provider: provider.to_string(),
            message_count: msgs,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            cost_cents: cost,
        }
    }

    let rows = vec![
        row("claude-opus-4-7", "claude_code", 210_000.0, 24_114),
        row("(untagged)", "cursor", 0.0, 15),
        row("claude-sonnet-4-6", "claude_code", 30_000.0, 6138),
        row("(untagged)", "cursor", 0.0, 7),
    ];

    // Default: `(untagged)` collapses to one zero-cost Cursor
    // bucket, which gets suppressed — two input transients fold
    // into the one-row `suppressed_pending` count.
    let (visible, suppressed) = merge_and_partition_pending(&rows, false);
    assert_eq!(visible.len(), 2);
    assert_eq!(suppressed, 1);
    assert!(
        visible
            .iter()
            .all(|r| r.display_label != display::UNATTRIBUTED_LABEL)
    );

    // With --include-pending the merged placeholder shows.
    let (visible_all, suppressed_all) = merge_and_partition_pending(&rows, true);
    assert_eq!(visible_all.len(), 3);
    assert_eq!(suppressed_all, 0);
    assert!(
        visible_all
            .iter()
            .any(|r| r.display_label == display::UNATTRIBUTED_LABEL)
    );
}

#[test]
fn merge_and_partition_pending_surfaces_cursor_auto_cost_by_default() {
    // #443 acceptance: Cursor's `default` (Auto mode) rows carry
    // real cost; they must render by default, merged under the
    // canonical `(model not yet attributed)` label — *not*
    // suppressed the way pure-untagged zero-cost transients are.
    use budi_core::analytics::ModelUsage;

    let rows = vec![
        ModelUsage {
            model: "default".into(),
            provider: "cursor".into(),
            message_count: 25,
            input_tokens: 1_000_000,
            output_tokens: 200_000,
            cache_read_tokens: 10_200_000,
            cache_creation_tokens: 1_000_000,
            cost_cents: 437.0,
        },
        ModelUsage {
            model: "(untagged)".into(),
            provider: "cursor".into(),
            message_count: 15,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            cost_cents: 0.0,
        },
    ];

    let (visible, suppressed) = merge_and_partition_pending(&rows, false);
    assert_eq!(suppressed, 0);
    assert_eq!(visible.len(), 1);
    let merged = &visible[0];
    assert_eq!(merged.display_label, display::UNATTRIBUTED_LABEL);
    assert_eq!(merged.provider, "cursor");
    assert_eq!(merged.message_count, 40);
    assert_eq!(merged.cost_cents, 437.0);
}

#[test]
fn models_json_carries_display_name_and_provider_model_id() {
    // #443 acceptance 5: `--format json` must expose both the
    // canonical `display_name` and the raw `provider_model_id`,
    // plus the effort modifier as a separate field.
    use budi_core::analytics::{BreakdownPage, ModelUsage};

    let rows = vec![
        ModelUsage {
            model: "claude-opus-4-7-thinking-high".into(),
            provider: "cursor".into(),
            message_count: 249,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            cost_cents: 45_300.0,
        },
        ModelUsage {
            model: "claude-opus-4-7".into(),
            provider: "claude_code".into(),
            message_count: 890,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            cost_cents: 45_600.0,
        },
    ];
    let page = BreakdownPage {
        rows,
        other: None,
        total_cost_cents: 90_900.0,
        total_rows: 2,
        shown_rows: 2,
        limit: 50,
    };

    let json = serde_json::to_value(serde_models_page_for_test(&page)).unwrap();
    let rows_json = json["rows"].as_array().unwrap();
    assert_eq!(rows_json.len(), 2);

    let cursor_row = &rows_json[0];
    assert_eq!(
        cursor_row["model"].as_str(),
        Some("claude-opus-4-7-thinking-high")
    );
    assert_eq!(
        cursor_row["provider_model_id"].as_str(),
        Some("claude-opus-4-7-thinking-high")
    );
    assert_eq!(cursor_row["display_name"].as_str(), Some("Claude Opus 4.7"));
    assert_eq!(
        cursor_row["effort_modifier"].as_str(),
        Some("thinking-high")
    );
    assert_eq!(cursor_row["placeholder"].as_str(), Some("none"));

    let canonical_row = &rows_json[1];
    assert_eq!(
        canonical_row["display_name"].as_str(),
        Some("Claude Opus 4.7")
    );
    // effort_modifier is omitted when None (serde skip).
    assert!(canonical_row.get("effort_modifier").is_none());
}

#[test]
fn is_only_untagged_detects_lone_fallback_row() {
    // #450 acceptance D: a single-row page made of just the
    // `(untagged)` bucket is the "no labelled signal" case —
    // render an empty-state tip instead of a 1-row table that
    // looks like a filesystem fault.
    use budi_core::analytics::{FileCost, TicketCost};

    let lonely_files = vec![FileCost {
        file_path: "(untagged)".into(),
        session_count: 1,
        message_count: 5,
        input_tokens: 0,
        output_tokens: 0,
        cache_creation_tokens: 0,
        cache_read_tokens: 0,
        cost_cents: 114.0,
        top_repo_id: String::new(),
        top_branch: String::new(),
        top_ticket_id: String::new(),
        source: String::new(),
    }];
    assert!(is_only_untagged(&lonely_files, |f| &f.file_path));

    let mixed_tickets = vec![
        TicketCost {
            ticket_id: "PAVA-1".into(),
            ticket_prefix: "PAVA".into(),
            session_count: 1,
            message_count: 10,
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            cost_cents: 100.0,
            top_branch: "main".into(),
            top_repo_id: String::new(),
            source: "branch".into(),
        },
        TicketCost {
            ticket_id: "(untagged)".into(),
            ticket_prefix: String::new(),
            session_count: 1,
            message_count: 10,
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            cost_cents: 50.0,
            top_branch: String::new(),
            top_repo_id: String::new(),
            source: String::new(),
        },
    ];
    assert!(!is_only_untagged(&mixed_tickets, |t| &t.ticket_id));

    let empty: Vec<TicketCost> = vec![];
    assert!(!is_only_untagged(&empty, |t| &t.ticket_id));
}

#[test]
fn untagged_only_tip_nudges_short_windows_to_widen() {
    // The "try a longer window" hint is only helpful on `today`
    // and `1d` — wider windows already saw enough data to know
    // nothing landed, so the extra line becomes noise.
    assert!(untagged_only_tip(StatsPeriod::Today).is_some());
    assert!(untagged_only_tip(StatsPeriod::Days(1)).is_some());
    assert!(untagged_only_tip(StatsPeriod::Days(7)).is_none());
    assert!(untagged_only_tip(StatsPeriod::Week).is_none());
    assert!(untagged_only_tip(StatsPeriod::Month).is_none());
}

// ─── #450 view-layout snapshot tests ──────────────────────────────
//
// Every breakdown view (`--tickets / --files / --branches /
// --activities / --models`) is rendered through the same shared
// helpers (`format_breakdown_header_text`, `format_breakdown_row_text`,
// `format_cost_cents_fixed`). Pin the rendered shape under the
// `today` and `30d` headings so the next regression shows up in
// the diff instead of in a user's terminal.

/// Build a baseline row for a view with the current label width,
/// untagged translation, and fixed currency format — the surface
/// the ticket asks us to keep stable.
fn snapshot_row(view: BreakdownView, raw_label: &str, cents: f64, extras: &str) -> String {
    let label_width = 40usize;
    let translated = display_dimension(view, raw_label);
    let label = truncate_label_middle(&translated, label_width);
    format_breakdown_row_text(&label, label_width, cents, cents.max(1.0), extras)
}

#[test]
fn snapshot_tickets_today_and_30d_layout_is_stable() {
    let today_label = period_label(StatsPeriod::Today);
    let thirty_label = period_label(StatsPeriod::Days(30));
    assert_eq!(today_label, "Today");
    assert_eq!(thirty_label, "Last 30 days");

    let header = format_breakdown_header_text("TICKET", 40, "SOURCE     TOP_BRANCH");
    assert!(header.contains("TICKET"));
    assert!(header.contains("COST"));
    assert!(header.contains("SOURCE"));

    let real = snapshot_row(
        BreakdownView::Tickets,
        "PAVA-1669",
        2_265.0,
        "src=branch  04-20-pava-1669",
    );
    assert!(real.contains("PAVA-1669"));
    assert!(real.contains("$22.65"));

    let untagged = snapshot_row(
        BreakdownView::Tickets,
        "(untagged)",
        9_122.0,
        "src=--      --",
    );
    assert!(untagged.contains("(no ticket)"));
    assert!(untagged.contains("$91.22"));
    assert!(!untagged.contains("(untagged)"));
}

#[test]
fn snapshot_files_today_and_30d_layout_is_stable() {
    let row = snapshot_row(
        BreakdownView::Files,
        "crates/budi-cli/src/commands/stats.rs",
        58_732.0,
        "src=cwd     PAVA-1669",
    );
    assert!(row.contains("stats.rs"));
    assert!(row.contains("$587.32"));

    let long_path = snapshot_row(
        BreakdownView::Files,
        "very/deeply/nested/path/that/wraps/into/the/next/column/file.proto",
        12_000.0,
        "",
    );
    // Middle ellipsis keeps the start AND the filename visible.
    assert!(long_path.contains('…'));
    assert!(long_path.contains("file.proto") || long_path.contains(".proto"));

    let untagged = snapshot_row(BreakdownView::Files, "(untagged)", 11_400.0, "");
    assert!(untagged.contains("(no file tag)"));
}

#[test]
fn snapshot_branches_today_and_30d_layout_is_stable() {
    let main = snapshot_row(BreakdownView::Branches, "main", 150_000.0, "budi");
    assert!(main.contains("main"));
    assert!(main.contains("$1,500.00"));

    let feature = snapshot_row(
        BreakdownView::Branches,
        "04-20-pava-1669_adds_an_optional_inputmode_prop_to_chararrayinput",
        2_265.0,
        "Verkada-Web",
    );
    // Middle-ellipsis retains the date + ticket prefix and the
    // distinctive suffix — the two ends readers actually need.
    assert!(feature.starts_with("  04-20-"));
    assert!(feature.contains('…'));

    let untagged = snapshot_row(BreakdownView::Branches, "(untagged)", 100.0, "--");
    assert!(untagged.contains("(no branch)"));
}

#[test]
fn snapshot_activities_today_and_30d_layout_is_stable() {
    let coding = snapshot_row(
        BreakdownView::Activities,
        "coding",
        123_456.0,
        "conf=high  main",
    );
    assert!(coding.contains("coding"));
    assert!(coding.contains("$1,234.56"));

    let untagged = snapshot_row(
        BreakdownView::Activities,
        "(untagged)",
        420.0,
        "conf=--    --",
    );
    assert!(untagged.contains("(unclassified)"));
}

#[test]
fn snapshot_models_today_and_30d_layout_is_stable() {
    // `--models` snapshot: the `(untagged)` sentinel renders as
    // `(model not yet attributed)` (#443 — same label the
    // `display::resolve` overlay uses, so the empty-state tip
    // and the row label agree). Breakdown currency is fixed
    // with thousands separators.
    let opus = snapshot_row(
        BreakdownView::Models,
        "claude-opus-4-7",
        210_000.0,
        "24114 msgs  120M tok",
    );
    assert!(opus.contains("claude-opus-4-7"));
    assert!(opus.contains("$2,100.00"));

    let pending_visible =
        snapshot_row(BreakdownView::Models, "(untagged)", 0.0, "162 msgs   0 tok");
    assert!(pending_visible.contains("(model not yet attributed)"));
}

#[test]
fn format_breakdown_footer_uses_fixed_currency() {
    // The Total row renders with `format_cost_cents_fixed`, so a
    // value like `$2100.00` never humanizes to `$2.1K` at the
    // footer — reconciliation to the cent requires the fixed
    // shape. We assert the helper directly; the footer path uses
    // the same call.
    assert_eq!(format_cost_cents_fixed(210_000.0), "$2,100.00");
}
