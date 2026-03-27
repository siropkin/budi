#!/usr/bin/env python3
"""Validate Budi cost calculations against official Anthropic billing CSV.

Usage:
    python3 scripts/validate-cost.py <billing-csv-path> [--api-key-filter <substring>]

The billing CSV is exported from the Anthropic Console:
    Console → Settings → Billing → Usage → Export CSV

Example:
    python3 scripts/validate-cost.py ~/Downloads/claude_api_cost.csv --api-key-filter ivan.seredkin
"""

import argparse
import csv
import sqlite3
import os
import sys
from collections import defaultdict
from pathlib import Path


def find_budi_db():
    """Find the Budi analytics database."""
    candidates = [
        Path.home() / ".local/share/budi/analytics.db",
        Path.home() / "Library/Application Support/budi/analytics.db",
    ]
    for p in candidates:
        if p.exists():
            return p
    return None


def load_official_data(csv_path, api_key_filter=None):
    """Load official billing data from Anthropic CSV export."""
    daily = defaultdict(float)
    by_type = defaultdict(float)
    total = 0.0

    with open(csv_path) as f:
        reader = csv.DictReader(f)
        for row in reader:
            if api_key_filter and api_key_filter not in row.get("api_key", ""):
                continue
            cost = float(row["cost_usd"])
            date = row["usage_date_utc"]
            token_type = row.get("token_type", "--")
            ctx_window = row.get("context_window", "--")

            daily[date] += cost
            by_type[(token_type, ctx_window)] += cost
            total += cost

    return daily, by_type, total


def load_budi_data(db_path, since=None, until=None):
    """Load Budi cost data from the analytics database."""
    conn = sqlite3.connect(str(db_path))
    conditions = ["role='assistant'", "provider='claude_code'"]
    params = []
    if since:
        conditions.append("timestamp >= ?")
        params.append(since)
    if until:
        conditions.append("timestamp < ?")
        params.append(until)

    where = " AND ".join(conditions)

    # Daily totals
    daily = {}
    for row in conn.execute(
        f"SELECT DATE(timestamp), ROUND(SUM(cost_cents)/100.0, 2), COUNT(*) "
        f"FROM messages WHERE {where} GROUP BY DATE(timestamp) ORDER BY 1",
        params,
    ):
        daily[row[0]] = row[1]

    # Total
    result = conn.execute(
        f"SELECT ROUND(SUM(cost_cents)/100.0, 2), COUNT(*) FROM messages WHERE {where}",
        params,
    ).fetchone()
    total = result[0] or 0.0
    msg_count = result[1] or 0

    # By cost_confidence
    by_confidence = {}
    for row in conn.execute(
        f"SELECT cost_confidence, COUNT(*), ROUND(SUM(cost_cents)/100.0, 2) "
        f"FROM messages WHERE {where} GROUP BY cost_confidence",
        params,
    ):
        by_confidence[row[0]] = {"count": row[1], "cost_usd": row[2]}

    conn.close()
    return daily, total, msg_count, by_confidence


def main():
    parser = argparse.ArgumentParser(description="Validate Budi vs official Anthropic billing")
    parser.add_argument("csv_path", help="Path to Anthropic billing CSV export")
    parser.add_argument("--api-key-filter", help="Filter CSV by API key substring (e.g., your username)")
    parser.add_argument("--since", help="Start date (YYYY-MM-DD)")
    parser.add_argument("--until", help="End date exclusive (YYYY-MM-DD)")
    args = parser.parse_args()

    if not os.path.exists(args.csv_path):
        print(f"Error: CSV not found: {args.csv_path}")
        sys.exit(1)

    db_path = find_budi_db()
    if not db_path:
        print("Error: Budi analytics database not found")
        sys.exit(1)

    print(f"Official CSV: {args.csv_path}")
    print(f"Budi DB:      {db_path}")
    if args.api_key_filter:
        print(f"API key filter: {args.api_key_filter}")
    print()

    # Load data
    official_daily, official_by_type, official_total = load_official_data(
        args.csv_path, args.api_key_filter
    )
    budi_daily, budi_total, budi_msg_count, budi_by_confidence = load_budi_data(
        db_path, args.since, args.until
    )

    # Filter official data by date range
    if args.since or args.until:
        filtered_daily = {}
        for date, cost in official_daily.items():
            if args.since and date < args.since:
                continue
            if args.until and date >= args.until:
                continue
            filtered_daily[date] = cost
        official_daily = filtered_daily
        official_total = sum(official_daily.values())

    # Daily comparison
    all_dates = sorted(set(list(official_daily.keys()) + list(budi_daily.keys())))

    print(f"{'Date':12s} {'Official':>10s} {'Budi':>10s} {'Delta':>10s} {'%':>8s}")
    print("-" * 55)

    for date in all_dates:
        o = official_daily.get(date, 0.0)
        b = budi_daily.get(date, 0.0)
        d = b - o
        pct = (d / o * 100) if o else 0
        flag = " ***" if abs(pct) > 5 and o > 1.0 else ""
        print(f"{date:12s} ${o:>9.2f} ${b:>9.2f} ${d:>+9.2f} {pct:>+7.1f}%{flag}")

    print("-" * 55)
    o_total = sum(official_daily.values())
    b_total = sum(budi_daily.get(d, 0.0) for d in official_daily.keys())
    d_total = b_total - o_total
    pct_total = (d_total / o_total * 100) if o_total else 0
    print(f"{'Total':12s} ${o_total:>9.2f} ${b_total:>9.2f} ${d_total:>+9.2f} {pct_total:>+7.1f}%")

    print()
    print(f"Budi messages: {budi_msg_count}")
    print(f"Cost confidence breakdown:")
    for conf, data in sorted(budi_by_confidence.items(), key=lambda x: -x[1]["cost_usd"]):
        print(f"  {conf:25s} {data['count']:>6d} msgs  ${data['cost_usd']:>9.2f}")

    print()
    print("Official cost by token type:")
    for (tt, ctx), cost in sorted(official_by_type.items(), key=lambda x: -x[1]):
        if cost >= 0.01:
            print(f"  {tt:25s} {ctx:15s} ${cost:>9.2f}")

    # Assessment
    print()
    if abs(pct_total) < 2:
        print(f"PASS: Budi within 2% of official ({pct_total:+.1f}%)")
    elif abs(pct_total) < 5:
        print(f"WARN: Budi within 5% of official ({pct_total:+.1f}%)")
    else:
        print(f"FAIL: Budi differs by {pct_total:+.1f}% from official")

    sys.exit(0 if abs(pct_total) < 5 else 1)


if __name__ == "__main__":
    main()
