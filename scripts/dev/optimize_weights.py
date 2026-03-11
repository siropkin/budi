#!/usr/bin/env python3
"""Analyze channel scores and suggest weight improvements.

Reads the channel_scores.jsonl produced by capture_channel_scores.py
and cross-references with benchmark labels from reranker_training_data.jsonl.

Usage:
    python3 scripts/dev/optimize_weights.py [--scores channel_scores.jsonl] [--labels reranker_training_data.jsonl]
"""

import argparse
import json
import sys
from collections import defaultdict
from pathlib import Path


def load_labels(path: str) -> dict:
    """Load benchmark labels: (repo, prompt_text) → {path: label}."""
    labels = defaultdict(dict)
    with open(path) as f:
        for line in f:
            d = json.loads(line)
            key = (d["repo"], d["query"])
            labels[key][d["path"]] = d["label"]
    return dict(labels)


def load_scores(path: str) -> list:
    """Load captured channel scores."""
    records = []
    with open(path) as f:
        for line in f:
            records.append(json.loads(line))
    return records


def analyze_intent_channels(records: list, labels: dict):
    """Analyze per-intent channel contribution patterns."""
    intent_stats = defaultdict(lambda: {
        "count": 0,
        "channel_sums": defaultdict(float),
        "channel_nonzero": defaultdict(int),
        "positive_channel_sums": defaultdict(float),
        "negative_channel_sums": defaultdict(float),
        "positive_count": 0,
        "negative_count": 0,
    })

    channels = ["lexical", "vector", "symbol", "path", "graph"]

    for rec in records:
        intent = rec["intent"]
        repo = rec["repo"]
        prompt = rec["prompt"]
        label_key = (repo, prompt)
        prompt_labels = labels.get(label_key, {})

        stats = intent_stats[intent]
        stats["count"] += 1

        for cand in rec["candidates"]:
            cand_path = cand["path"]
            cs = cand["channel_scores"]
            label = prompt_labels.get(cand_path)

            for ch in channels:
                val = cs.get(ch, 0.0)
                stats["channel_sums"][ch] += val
                if val > 0:
                    stats["channel_nonzero"][ch] += 1

                if label is not None:
                    if label > 0.5:
                        stats["positive_channel_sums"][ch] += val
                        if ch == channels[0]:
                            stats["positive_count"] += 1
                    else:
                        stats["negative_channel_sums"][ch] += val
                        if ch == channels[0]:
                            stats["negative_count"] += 1

    return intent_stats, channels


def print_analysis(intent_stats, channels):
    """Print analysis of channel contributions per intent."""
    print("\n=== Channel Contribution Analysis ===\n")

    # Current weights for reference
    current_weights = {
        "symbol-definition": {"lexical": 1.5, "vector": 1.0, "symbol": 2.0, "path": 0.5, "graph": 1.0},
        "flow-trace": {"lexical": 1.0, "vector": 1.0, "symbol": 1.5, "path": 0.5, "graph": 2.5},
        "symbol-usage": {"lexical": 1.0, "vector": 1.0, "symbol": 2.0, "path": 0.5, "graph": 2.0},
        "architecture": {"lexical": 1.0, "vector": 2.0, "symbol": 1.0, "path": 1.5, "graph": 0.5},
        "test-lookup": {"lexical": 1.5, "vector": 1.5, "symbol": 1.0, "path": 1.0, "graph": 0.5},
        "runtime-config": {"lexical": 1.5, "vector": 1.5, "symbol": 1.0, "path": 1.5, "graph": 0.5},
    }

    for intent, stats in sorted(intent_stats.items()):
        print(f"\n--- {intent} ({stats['count']} prompts, {stats['positive_count']} labeled+, {stats['negative_count']} labeled-) ---")

        cw = current_weights.get(intent, {})

        # Average channel scores
        total_cands = sum(stats["channel_nonzero"].get(ch, 0) for ch in channels) / max(len(channels), 1)
        print(f"  Avg candidates with non-zero scores: {total_cands:.0f}")

        print(f"\n  {'Channel':<10} {'Weight':<8} {'Avg Score':<12} {'Non-zero%':<12} {'Pos Avg':<12} {'Neg Avg':<12} {'Discrimination':<14}")
        print(f"  {'─'*10} {'─'*8} {'─'*12} {'─'*12} {'─'*12} {'─'*12} {'─'*14}")

        for ch in channels:
            weight = cw.get(ch, "?")
            total = stats["channel_sums"][ch]
            nonzero = stats["channel_nonzero"][ch]

            pos_sum = stats["positive_channel_sums"][ch]
            neg_sum = stats["negative_channel_sums"][ch]
            pos_count = max(stats["positive_count"], 1)
            neg_count = max(stats["negative_count"], 1)

            pos_avg = pos_sum / pos_count if pos_count > 0 else 0
            neg_avg = neg_sum / neg_count if neg_count > 0 else 0
            discrim = pos_avg - neg_avg  # Higher = better discrimination

            avg_score = total / max(nonzero, 1)
            nonzero_pct = (nonzero / max(total_cands, 1)) * 100

            discrim_str = f"{discrim:+.4f}" if stats["positive_count"] > 0 else "n/a"

            print(f"  {ch:<10} {weight:<8} {avg_score:<12.4f} {nonzero_pct:<12.1f} {pos_avg:<12.4f} {neg_avg:<12.4f} {discrim_str:<14}")


def suggest_improvements(intent_stats, channels):
    """Suggest weight adjustments based on discrimination analysis."""
    print("\n\n=== Suggested Improvements ===\n")

    suggestions = []
    for intent, stats in sorted(intent_stats.items()):
        if stats["positive_count"] < 3:
            continue

        pos_count = stats["positive_count"]
        neg_count = stats["negative_count"]

        discriminations = {}
        for ch in channels:
            pos_avg = stats["positive_channel_sums"][ch] / max(pos_count, 1)
            neg_avg = stats["negative_channel_sums"][ch] / max(neg_count, 1)
            discriminations[ch] = pos_avg - neg_avg

        # Find channels with strong positive discrimination (weight should be higher)
        # and channels with negative discrimination (weight should be lower)
        for ch in channels:
            d = discriminations[ch]
            if d > 0.03:
                suggestions.append(f"  {intent}/{ch}: discrimination={d:+.4f} → consider INCREASING weight")
            elif d < -0.01:
                suggestions.append(f"  {intent}/{ch}: discrimination={d:+.4f} → consider DECREASING weight")

    if suggestions:
        for s in suggestions:
            print(s)
    else:
        print("  No strong signals for weight changes found.")

    print("\nNote: Discrimination = avg_positive_score - avg_negative_score.")
    print("Positive discrimination means the channel helps distinguish good from bad candidates.")


def main():
    parser = argparse.ArgumentParser(description="Analyze channel scores for weight optimization")
    parser.add_argument("--scores", default="scripts/dev/channel_scores.jsonl")
    parser.add_argument("--labels", default="scripts/dev/reranker_training_data.jsonl")
    args = parser.parse_args()

    if not Path(args.scores).exists():
        print(f"Error: {args.scores} not found. Run capture_channel_scores.py first.", file=sys.stderr)
        sys.exit(1)

    records = load_scores(args.scores)
    labels = load_labels(args.labels) if Path(args.labels).exists() else {}

    print(f"Loaded {len(records)} prompt records, {len(labels)} labeled prompts")

    intent_stats, channels = analyze_intent_channels(records, labels)
    print_analysis(intent_stats, channels)
    suggest_improvements(intent_stats, channels)


if __name__ == "__main__":
    main()
