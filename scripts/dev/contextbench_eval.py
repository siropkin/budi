#!/usr/bin/env python3
"""
ContextBench evaluation for budi.

Evaluates budi's retrieval quality against ContextBench gold-annotated contexts.
Uses file-level recall and precision: does budi find the right files given a
bug report / issue description?

Usage:
    python3 scripts/dev/contextbench_eval.py --repos psf/requests sharkdp/fd
    python3 scripts/dev/contextbench_eval.py --repos psf/requests --max-tasks 3
    python3 scripts/dev/contextbench_eval.py --list-repos

Requires: pip install datasets pyarrow  (in a venv)
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from collections import defaultdict
from pathlib import Path

# ---------------------------------------------------------------------------
# Dataset loading
# ---------------------------------------------------------------------------

def load_contextbench():
    """Load ContextBench dataset from HuggingFace."""
    try:
        from datasets import load_dataset
    except ImportError:
        print("Error: 'datasets' package required. Install with: pip install datasets pyarrow")
        sys.exit(1)

    ds = load_dataset("Contextbench/ContextBench", "default", split="train")
    return ds


def group_by_repo(ds):
    """Group tasks by repo name."""
    groups = defaultdict(list)
    for row in ds:
        groups[row["repo"]].append(row)
    return groups


# ---------------------------------------------------------------------------
# Repo setup
# ---------------------------------------------------------------------------

def clone_repo(repo_url, base_commit, dest_dir):
    """Clone a repo at a specific commit."""
    print(f"  Cloning {repo_url} at {base_commit[:10]}...")
    # Full clone needed to reach arbitrary base commits.
    # Use --filter=blob:none for a partial (treeless) clone to save bandwidth.
    subprocess.run(
        ["git", "clone", "--quiet", "--filter=blob:none", repo_url, dest_dir],
        check=True,
        capture_output=True,
        text=True,
        timeout=600,
    )
    subprocess.run(
        ["git", "checkout", "--quiet", base_commit],
        cwd=dest_dir,
        check=True,
        capture_output=True,
        text=True,
        timeout=60,
    )


def index_repo(repo_dir, budi_bin="budi"):
    """Run budi init --index on a repo."""
    print(f"  Indexing with budi...")
    t0 = time.time()
    result = subprocess.run(
        [budi_bin, "init", "--index"],
        cwd=repo_dir,
        capture_output=True,
        text=True,
        timeout=600,
    )
    elapsed = time.time() - t0
    if result.returncode != 0:
        print(f"  WARNING: budi init --index failed: {result.stderr[:200]}")
        return False, elapsed
    print(f"  Indexed in {elapsed:.1f}s")
    return True, elapsed


def query_daemon(repo_dir, prompt, daemon_url="http://127.0.0.1:7878"):
    """Query budi daemon and return response."""
    import urllib.request

    payload = json.dumps({
        "repo_root": str(repo_dir),
        "prompt": prompt,
        "dump_candidates": True,
    }).encode()

    req = urllib.request.Request(
        f"{daemon_url}/query",
        data=payload,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            return json.loads(resp.read())
    except Exception as e:
        print(f"  WARNING: daemon query failed: {e}")
        return None


# ---------------------------------------------------------------------------
# Evaluation metrics
# ---------------------------------------------------------------------------

def normalize_gold_path(path):
    """Strip /workspace/<repo>/ prefix from ContextBench gold paths."""
    import re
    # Pattern: /workspace/<owner>__<repo>__<version>/path/to/file
    m = re.match(r"^/workspace/[^/]+/(.+)$", path)
    if m:
        return m.group(1)
    return path


def extract_gold_files(gold_context):
    """Extract unique file paths from gold context, normalized."""
    if isinstance(gold_context, str):
        gold_context = json.loads(gold_context)
    return set(normalize_gold_path(entry["file"]) for entry in gold_context)


def extract_retrieved_files(response):
    """Extract file paths from budi response (snippets + candidates)."""
    if not response:
        return set(), set()

    # Primary: files in injected snippets
    snippet_files = set()
    for s in response.get("snippets", []):
        snippet_files.add(s["path"])

    # Extended: top candidates (before filtering)
    candidate_files = set()
    for c in response.get("diagnostics", {}).get("candidates", []):
        candidate_files.add(c["path"])

    return snippet_files, candidate_files


def compute_metrics(gold_files, retrieved_files):
    """Compute file-level recall and precision."""
    if not gold_files:
        return {"recall": 0.0, "precision": 0.0, "f1": 0.0, "gold_count": 0, "retrieved_count": 0}

    hits = gold_files & retrieved_files
    recall = len(hits) / len(gold_files) if gold_files else 0.0
    precision = len(hits) / len(retrieved_files) if retrieved_files else 0.0
    f1 = 2 * recall * precision / (recall + precision) if (recall + precision) > 0 else 0.0

    return {
        "recall": recall,
        "precision": precision,
        "f1": f1,
        "gold_count": len(gold_files),
        "retrieved_count": len(retrieved_files),
        "hits": sorted(hits),
        "missed": sorted(gold_files - retrieved_files),
    }


# ---------------------------------------------------------------------------
# Main evaluation loop
# ---------------------------------------------------------------------------

def evaluate_repo(repo_name, tasks, budi_bin, daemon_url, max_tasks=None):
    """Evaluate budi on a single repo's ContextBench tasks."""
    if max_tasks:
        tasks = tasks[:max_tasks]

    print(f"\n{'='*60}")
    print(f"Evaluating: {repo_name} ({len(tasks)} tasks)")
    print(f"{'='*60}")

    # Use the most recent base_commit (arbitrary choice — file-level recall
    # is fairly stable across nearby commits)
    base_commit = tasks[0]["base_commit"]
    repo_url = tasks[0]["repo_url"]

    tmpdir = tempfile.mkdtemp(prefix=f"contextbench-{repo_name.replace('/', '-')}-")
    try:
        clone_repo(repo_url, base_commit, tmpdir)
        ok, index_time = index_repo(tmpdir, budi_bin)
        if not ok:
            return None

        # Wait briefly for daemon to be ready
        time.sleep(1)

        results = []
        for i, task in enumerate(tasks):
            instance_id = task["instance_id"]
            prompt = task["problem_statement"]
            gold_files = extract_gold_files(task["gold_context"])

            # Truncate very long problem statements (some are >2000 chars)
            if len(prompt) > 1500:
                prompt = prompt[:1500]

            print(f"\n  Task {i+1}/{len(tasks)}: {instance_id}")
            print(f"    Gold files: {sorted(gold_files)}")
            print(f"    Prompt: {prompt[:120]}...")

            response = query_daemon(tmpdir, prompt, daemon_url)
            if response is None:
                results.append({
                    "instance_id": instance_id,
                    "error": "daemon query failed",
                })
                continue

            snippet_files, candidate_files = extract_retrieved_files(response)
            intent = response.get("detected_intent", "unknown")
            skip_reason = response.get("diagnostics", {}).get("skip_reason")
            top_score = response.get("diagnostics", {}).get("top_score", 0)
            snippets_count = len(response.get("snippets", []))

            # File-level metrics on injected snippets
            snippet_metrics = compute_metrics(gold_files, snippet_files)
            # File-level metrics on all candidates (top 100)
            candidate_metrics = compute_metrics(gold_files, candidate_files)

            result = {
                "instance_id": instance_id,
                "intent": intent,
                "top_score": top_score,
                "skip_reason": skip_reason,
                "snippets_count": snippets_count,
                "snippet_metrics": snippet_metrics,
                "candidate_metrics": candidate_metrics,
                "gold_files": sorted(gold_files),
                "snippet_files": sorted(snippet_files),
            }
            results.append(result)

            # Print summary
            status = "SKIP" if skip_reason else "INJ"
            sr = snippet_metrics["recall"]
            cr = candidate_metrics["recall"]
            print(f"    [{status}] intent={intent} top={top_score:.2f} "
                  f"snippet_recall={sr:.0%} candidate_recall={cr:.0%}")
            if snippet_metrics.get("hits"):
                print(f"    Hits: {snippet_metrics['hits']}")
            if snippet_metrics.get("missed"):
                print(f"    Missed: {snippet_metrics['missed']}")

        return {
            "repo": repo_name,
            "tasks_evaluated": len(tasks),
            "index_time": index_time,
            "results": results,
        }

    finally:
        # Clean up clone
        shutil.rmtree(tmpdir, ignore_errors=True)


def print_summary(all_results):
    """Print aggregate summary across all repos."""
    print(f"\n{'='*60}")
    print("AGGREGATE SUMMARY")
    print(f"{'='*60}")

    total_tasks = 0
    total_snippet_recall = 0.0
    total_candidate_recall = 0.0
    total_injected = 0
    total_skipped = 0
    total_gold_files = 0
    total_snippet_hits = 0
    total_candidate_hits = 0

    for repo_result in all_results:
        if repo_result is None:
            continue
        repo = repo_result["repo"]
        results = repo_result["results"]
        valid = [r for r in results if "error" not in r]

        if not valid:
            print(f"\n  {repo}: no valid results")
            continue

        injected = [r for r in valid if not r["skip_reason"]]
        skipped = [r for r in valid if r["skip_reason"]]

        # Aggregate file-level recall
        snippet_recalls = [r["snippet_metrics"]["recall"] for r in valid]
        candidate_recalls = [r["candidate_metrics"]["recall"] for r in valid]
        avg_snippet_recall = sum(snippet_recalls) / len(snippet_recalls) if snippet_recalls else 0
        avg_candidate_recall = sum(candidate_recalls) / len(candidate_recalls) if candidate_recalls else 0

        gold_file_count = sum(r["snippet_metrics"]["gold_count"] for r in valid)
        snippet_hit_count = sum(len(r["snippet_metrics"].get("hits", [])) for r in valid)
        candidate_hit_count = sum(len(r["candidate_metrics"].get("hits", [])) for r in valid)

        print(f"\n  {repo} ({len(valid)} tasks, {len(injected)} injected, {len(skipped)} skipped):")
        print(f"    Avg snippet file recall:   {avg_snippet_recall:.1%}")
        print(f"    Avg candidate file recall:  {avg_candidate_recall:.1%}")
        print(f"    Gold files: {gold_file_count}, snippet hits: {snippet_hit_count}, candidate hits: {candidate_hit_count}")
        print(f"    Index time: {repo_result['index_time']:.1f}s")

        total_tasks += len(valid)
        total_snippet_recall += sum(snippet_recalls)
        total_candidate_recall += sum(candidate_recalls)
        total_injected += len(injected)
        total_skipped += len(skipped)
        total_gold_files += gold_file_count
        total_snippet_hits += snippet_hit_count
        total_candidate_hits += candidate_hit_count

    if total_tasks > 0:
        print(f"\n  TOTAL ({total_tasks} tasks, {total_injected} injected, {total_skipped} skipped):")
        print(f"    Avg snippet file recall:   {total_snippet_recall/total_tasks:.1%}")
        print(f"    Avg candidate file recall:  {total_candidate_recall/total_tasks:.1%}")
        print(f"    Gold files: {total_gold_files}, snippet hits: {total_snippet_hits}, candidate hits: {total_candidate_hits}")


def main():
    parser = argparse.ArgumentParser(description="ContextBench evaluation for budi")
    parser.add_argument("--repos", nargs="+", help="Repos to evaluate (e.g. psf/requests sharkdp/fd)")
    parser.add_argument("--list-repos", action="store_true", help="List available repos and exit")
    parser.add_argument("--max-tasks", type=int, default=None, help="Max tasks per repo")
    parser.add_argument("--budi-bin", default=os.path.expanduser("~/.local/bin/budi"), help="Path to budi binary")
    parser.add_argument("--daemon-url", default="http://127.0.0.1:7878", help="Daemon URL")
    parser.add_argument("--output", default=None, help="Output JSON file for results")
    args = parser.parse_args()

    print("Loading ContextBench dataset...")
    ds = load_contextbench()
    groups = group_by_repo(ds)

    if args.list_repos:
        print(f"\nAvailable repos ({len(groups)}):")
        for repo, tasks in sorted(groups.items(), key=lambda x: -len(x[1])):
            print(f"  {repo} ({tasks[0]['language']}, {len(tasks)} tasks)")
        return

    if not args.repos:
        parser.error("--repos required (use --list-repos to see available)")

    # Validate repos
    for repo in args.repos:
        if repo not in groups:
            print(f"Error: repo '{repo}' not found in ContextBench. Use --list-repos.")
            sys.exit(1)

    # Run evaluation
    all_results = []
    for repo in args.repos:
        tasks = groups[repo]
        result = evaluate_repo(
            repo, tasks, args.budi_bin, args.daemon_url, args.max_tasks
        )
        all_results.append(result)

    print_summary(all_results)

    if args.output:
        with open(args.output, "w") as f:
            json.dump(all_results, f, indent=2, default=str)
        print(f"\nResults saved to {args.output}")


if __name__ == "__main__":
    main()
