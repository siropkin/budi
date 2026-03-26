#!/usr/bin/env python3
"""Validate Budi cost calculations against raw JSONL transcript data.

Reads all Claude Code JSONL transcripts, calculates expected costs from tokens,
and compares against what Budi would store. Reports discrepancies.

Usage:
    python3 scripts/validate_costs.py [--since YYYY-MM-DD]
"""

import json
import glob
import os
import sys
from datetime import datetime

# Official Anthropic pricing (per million tokens, USD)
# Source: https://docs.anthropic.com/en/docs/about-claude/pricing
PRICING = {
    # model_contains -> (input, output, cache_write_5m, cache_read)
    "opus-4-6": (5.0, 25.0, 6.25, 0.50),
    "opus-4-5": (5.0, 25.0, 6.25, 0.50),
    "opus-4-1": (15.0, 75.0, 18.75, 1.50),
    "opus-4-0": (15.0, 75.0, 18.75, 1.50),
    "opus-4-2": (15.0, 75.0, 18.75, 1.50),  # future-proof
    "opus-3": (15.0, 75.0, 18.75, 1.50),
    "sonnet": (3.0, 15.0, 3.75, 0.30),
    "haiku-4-5": (1.0, 5.0, 1.25, 0.10),
    "haiku-4": (1.0, 5.0, 1.25, 0.10),
    "3-5-haiku": (0.80, 4.0, 1.0, 0.08),
    "haiku-3-5": (0.80, 4.0, 1.0, 0.08),
    "3-haiku": (0.25, 1.25, 0.30, 0.03),
    "haiku-3": (0.25, 1.25, 0.30, 0.03),
    "haiku": (1.0, 5.0, 1.25, 0.10),
}

# 1-hour cache write multiplier (2x base input vs 1.25x for 5min)
CACHE_1H_PRICING = {
    "opus-4-6": 10.0, "opus-4-5": 10.0,
    "opus-4-1": 30.0, "opus-4-0": 30.0,
    "sonnet": 6.0,
    "haiku-4-5": 2.0, "haiku-4": 2.0,
    "3-5-haiku": 1.6, "haiku-3-5": 1.6,
    "3-haiku": 0.50, "haiku-3": 0.50,
    "haiku": 2.0,
}


def get_pricing(model: str):
    m = (model or "unknown").lower()
    for key, prices in PRICING.items():
        if key in m:
            return prices
    # Default to Sonnet
    return (3.0, 15.0, 3.75, 0.30)


def main():
    since = None
    if "--since" in sys.argv:
        idx = sys.argv.index("--since")
        since = sys.argv[idx + 1]

    home = os.path.expanduser("~")
    files = glob.glob(f"{home}/.claude/projects/*/*.jsonl")

    total_precise_cents = 0.0
    total_rounded_cents = 0.0  # Old behavior (round per message)
    msg_count = 0
    zero_rounded = 0
    model_costs = {}
    has_1h_cache = 0

    for f in files:
        with open(f) as fh:
            for line in fh:
                try:
                    entry = json.loads(line.strip())
                    if entry.get("type") != "assistant":
                        continue

                    ts = entry.get("timestamp", "")
                    if since and ts < since:
                        continue

                    msg = entry.get("message", {})
                    usage = msg.get("usage", {})
                    model = msg.get("model", "unknown")

                    inp, outp, cw, cr = get_pricing(model)

                    input_t = usage.get("input_tokens", 0)
                    output_t = usage.get("output_tokens", 0)
                    cache_create = usage.get("cache_creation_input_tokens", 0)
                    cache_read = usage.get("cache_read_input_tokens", 0)

                    # Check for 1hr cache
                    cache_detail = usage.get("cache_creation", {})
                    eph_1h = cache_detail.get("ephemeral_1h_input_tokens", 0)
                    if eph_1h > 0:
                        has_1h_cache += 1

                    cost_dollars = (
                        input_t * inp / 1_000_000
                        + output_t * outp / 1_000_000
                        + cache_create * cw / 1_000_000
                        + cache_read * cr / 1_000_000
                    )
                    cost_cents = cost_dollars * 100.0
                    rounded_cents = round(cost_cents)

                    total_precise_cents += cost_cents
                    total_rounded_cents += rounded_cents
                    msg_count += 1

                    if rounded_cents == 0 and cost_cents > 0:
                        zero_rounded += 1

                    model_costs.setdefault(model, {"precise": 0.0, "rounded": 0.0, "count": 0})
                    model_costs[model]["precise"] += cost_cents
                    model_costs[model]["rounded"] += rounded_cents
                    model_costs[model]["count"] += 1

                except (json.JSONDecodeError, KeyError):
                    pass

    print("=" * 70)
    print("BUDI COST VALIDATION REPORT")
    print("=" * 70)
    if since:
        print(f"Period: since {since}")
    print(f"Messages analyzed: {msg_count:,}")
    print()

    print("--- Precision Analysis ---")
    print(f"Precise total:      ${total_precise_cents / 100:.4f}  ({total_precise_cents:.4f} cents)")
    print(f"Rounded total (old): ${total_rounded_cents / 100:.4f}  ({total_rounded_cents:.4f} cents)")
    delta = total_rounded_cents - total_precise_cents
    pct = abs(delta) / total_precise_cents * 100 if total_precise_cents > 0 else 0
    direction = "OVER" if delta > 0 else "UNDER"
    print(f"Rounding error:     ${abs(delta) / 100:.4f}  ({direction}-reported, {pct:.4f}%)")
    print(f"Messages rounded to $0: {zero_rounded:,}")
    print()

    if has_1h_cache:
        print(f"⚠ Found {has_1h_cache} messages with 1hr cache tokens (different pricing)")
    else:
        print("✓ No 1hr cache tokens found (all 5min cache — pricing is correct)")
    print()

    print("--- Per-Model Breakdown ---")
    for model, data in sorted(model_costs.items(), key=lambda x: -x[1]["precise"]):
        precise = data["precise"]
        rounded = data["rounded"]
        count = data["count"]
        delta_m = rounded - precise
        print(f"  {model}: ${precise / 100:.2f} precise, ${rounded / 100:.2f} rounded (Δ${abs(delta_m) / 100:.2f}, {count:,} msgs)")

    print()
    print("--- Summary ---")
    if pct < 0.1:
        print("✓ Cost accuracy is GOOD (< 0.1% error)")
    elif pct < 1.0:
        print(f"⚠ Cost accuracy is FAIR ({pct:.2f}% error)")
    else:
        print(f"✗ Cost accuracy is POOR ({pct:.2f}% error)")

    return 0 if pct < 1.0 else 1


if __name__ == "__main__":
    sys.exit(main())
