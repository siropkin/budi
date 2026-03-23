#!/usr/bin/env python3
"""Generate cross-encoder training data from budi benchmark results.

Extracts (query, chunk_text, relevance_label) triplets from ab-results.json
files across all benchmark repos. Produces a JSONL file for fine-tuning.

Labeling strategy:
  - Positive (1.0): chunks injected when with_budi won or tied AND quality >= 8
  - Weak positive (0.5): chunks injected when with_budi tied AND quality == 7
  - Negative (0.0): chunks injected when with_budi lost (quality dropped)
  - Hard negative (0.0): random chunks from same repo but different query intent

Usage:
    python3 scripts/dev/reranker_training_data.py [--output training_data.jsonl]
"""

import argparse
import json
import os
import random
from pathlib import Path


BUDI_DATA = Path.home() / ".local" / "share" / "budi" / "repos"


def find_ab_results() -> list[Path]:
    """Find all ab-results.json files."""
    results = []
    for root, _dirs, files in os.walk(BUDI_DATA):
        for f in files:
            if f == "ab-results.json":
                results.append(Path(root) / f)
    return sorted(results)


def read_source_lines(repo_root: str, rel_path: str, start: int, end: int) -> str | None:
    """Read source code lines from a benchmark repo."""
    full_path = Path(repo_root) / rel_path
    if not full_path.exists():
        return None
    try:
        lines = full_path.read_text(encoding="utf-8", errors="replace").splitlines()
        # start/end are 1-based
        chunk_lines = lines[max(0, start - 1) : end]
        text = "\n".join(chunk_lines)
        # Skip very short chunks (< 3 lines) — not useful for training
        if len(chunk_lines) < 3:
            return None
        # Skip very long chunks (> 200 lines) — truncate for 512-token model
        if len(chunk_lines) > 200:
            text = "\n".join(chunk_lines[:200])
        return text
    except Exception:
        return None


def extract_training_pairs(ab_results_path: Path) -> list[dict]:
    """Extract training pairs from one ab-results.json file."""
    with open(ab_results_path) as f:
        data = json.load(f)

    repo_root = data.get("repo_root", "")
    if not repo_root or not Path(repo_root).exists():
        return []

    pairs = []
    for row in data.get("rows", []):
        prompt = row.get("prompt", "")
        if not prompt:
            continue

        # Get hook output with snippet refs
        hook = row.get("with_budi_hook", {})
        output = hook.get("output", {})
        snippet_refs = output.get("snippet_refs", [])
        intent = output.get("retrieval_intent", "unknown")

        if not snippet_refs:
            continue

        # Get judge verdict
        judge = row.get("judge", {})
        if not judge or not judge.get("ok"):
            continue

        winner = judge.get("winner", "")
        q_no = judge.get("score_no_budi", 0)
        q_with = judge.get("score_with_budi", 0)

        # Determine relevance label
        if winner == "with_budi" and q_with >= 8:
            label = 1.0
        elif winner == "tie" and q_with >= 8:
            label = 1.0
        elif winner == "tie" and q_with == 7:
            label = 0.5
        elif winner == "no_budi":
            # Chunks from losing runs are negative — the injection hurt
            label = 0.0
        else:
            # Skip ambiguous cases
            continue

        # Extract chunk texts
        for ref in snippet_refs:
            text = read_source_lines(
                repo_root,
                ref.get("path", ""),
                ref.get("start_line", 0),
                ref.get("end_line", 0),
            )
            if text is None:
                continue

            pairs.append(
                {
                    "query": prompt,
                    "passage": text,
                    "label": label,
                    "source": "benchmark",
                    "intent": intent,
                    "repo": Path(repo_root).name,
                    "path": ref.get("path", ""),
                    "score": ref.get("score", 0.0),
                    "q_no": q_no,
                    "q_with": q_with,
                    "winner": winner,
                }
            )

    return pairs


def generate_hard_negatives(
    all_pairs: list[dict], per_query: int = 2
) -> list[dict]:
    """Generate hard negatives: chunks from different queries in the same repo.

    For each positive pair, pick random chunks from other queries in the same
    repo as negative examples. These are 'hard' because they're real code from
    the same codebase, just not relevant to this specific query.
    """
    # Group positive passages by repo
    repo_passages: dict[str, list[dict]] = {}
    for p in all_pairs:
        if p["label"] > 0:
            repo = p["repo"]
            repo_passages.setdefault(repo, []).append(p)

    negatives = []
    seen_pairs = set()

    for p in all_pairs:
        if p["label"] <= 0:
            continue  # Only generate negatives for positive queries

        repo = p["repo"]
        candidates = repo_passages.get(repo, [])
        if len(candidates) < 3:
            continue

        # Pick random chunks from OTHER queries
        query = p["query"]
        other_chunks = [c for c in candidates if c["query"] != query]
        if not other_chunks:
            continue

        sample = random.sample(other_chunks, min(per_query, len(other_chunks)))
        for neg in sample:
            key = (query, neg["path"], neg.get("score", 0))
            if key in seen_pairs:
                continue
            seen_pairs.add(key)

            negatives.append(
                {
                    "query": query,
                    "passage": neg["passage"],
                    "label": 0.0,
                    "source": "hard_negative",
                    "intent": p["intent"],
                    "repo": repo,
                    "path": neg["path"],
                    "score": 0.0,
                    "q_no": 0,
                    "q_with": 0,
                    "winner": "synthetic",
                }
            )

    return negatives


def deduplicate_pairs(pairs: list[dict]) -> list[dict]:
    """Remove duplicate (query, passage) pairs, keeping highest label."""
    seen: dict[tuple, dict] = {}
    for p in pairs:
        # Use first 200 chars of passage as key to handle slight variations
        key = (p["query"], p["passage"][:200])
        if key not in seen or p["label"] > seen[key]["label"]:
            seen[key] = p
    return list(seen.values())


def main():
    parser = argparse.ArgumentParser(description="Generate reranker training data")
    parser.add_argument(
        "--output",
        default="scripts/dev/reranker_training_data.jsonl",
        help="Output JSONL file path",
    )
    parser.add_argument(
        "--hard-negatives",
        type=int,
        default=2,
        help="Hard negatives per positive query (default: 2)",
    )
    args = parser.parse_args()

    print("Scanning for ab-results.json files...")
    ab_files = find_ab_results()
    print(f"Found {len(ab_files)} benchmark result files")

    all_pairs = []
    for ab_file in ab_files:
        pairs = extract_training_pairs(ab_file)
        if pairs:
            all_pairs.extend(pairs)

    print(f"Extracted {len(all_pairs)} raw pairs")

    # Deduplicate
    all_pairs = deduplicate_pairs(all_pairs)
    print(f"After dedup: {len(all_pairs)} pairs")

    # Count by label
    pos = sum(1 for p in all_pairs if p["label"] == 1.0)
    weak = sum(1 for p in all_pairs if p["label"] == 0.5)
    neg = sum(1 for p in all_pairs if p["label"] == 0.0)
    print(f"  Positive (1.0): {pos}")
    print(f"  Weak positive (0.5): {weak}")
    print(f"  Negative (0.0): {neg}")

    # Generate hard negatives
    hard_negs = generate_hard_negatives(all_pairs, per_query=args.hard_negatives)
    print(f"Generated {len(hard_negs)} hard negatives")
    all_pairs.extend(hard_negs)

    # Final dedup
    all_pairs = deduplicate_pairs(all_pairs)
    print(f"Final dataset: {len(all_pairs)} pairs")

    # Stats by repo
    repo_counts: dict[str, int] = {}
    for p in all_pairs:
        repo_counts[p["repo"]] = repo_counts.get(p["repo"], 0) + 1
    print("\nPer-repo breakdown:")
    for repo, count in sorted(repo_counts.items()):
        print(f"  {repo}: {count}")

    # Stats by intent
    intent_counts: dict[str, int] = {}
    for p in all_pairs:
        intent_counts[p["intent"]] = intent_counts.get(p["intent"], 0) + 1
    print("\nPer-intent breakdown:")
    for intent, count in sorted(intent_counts.items()):
        print(f"  {intent}: {count}")

    # Write output
    output_path = Path(args.output)
    output_path.parent.mkdir(parents=True, exist_ok=True)
    with open(output_path, "w") as f:
        for p in all_pairs:
            f.write(json.dumps(p) + "\n")

    print(f"\nWritten to {output_path}")

    # Also write a summary
    summary = {
        "total_pairs": len(all_pairs),
        "positive": sum(1 for p in all_pairs if p["label"] == 1.0),
        "weak_positive": sum(1 for p in all_pairs if p["label"] == 0.5),
        "negative": sum(1 for p in all_pairs if p["label"] == 0.0),
        "repos": dict(sorted(repo_counts.items())),
        "intents": dict(sorted(intent_counts.items())),
        "ab_files_scanned": len(ab_files),
    }
    summary_path = output_path.with_suffix(".summary.json")
    with open(summary_path, "w") as f:
        json.dump(summary, f, indent=2)
    print(f"Summary: {summary_path}")


if __name__ == "__main__":
    main()
