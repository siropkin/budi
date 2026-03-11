#!/usr/bin/env python3
"""Capture per-candidate channel scores across all benchmark prompts.

Produces a JSONL file with raw fusion scores for weight optimization.
Requires a running budi-daemon on localhost:7878.

Usage:
    python3 scripts/dev/capture_channel_scores.py [--output scores.jsonl]
"""

import argparse
import json
import sys
import urllib.request
from pathlib import Path

BENCHMARK_REPOS = {
    "flask": "/Users/ivan.seredkin/_projects/public-budi-bench/flask",
    "react": "/Users/ivan.seredkin/_projects/react",
    "django": "/Users/ivan.seredkin/_projects/public-budi-bench/django",
    "ripgrep": "/Users/ivan.seredkin/_projects/ripgrep",
    "terraform": "/Users/ivan.seredkin/_projects/public-budi-bench/terraform",
    "fastify": "/Users/ivan.seredkin/_projects/public-budi-bench/fastify",
    "fastapi": "/Users/ivan.seredkin/_projects/public-budi-bench/fastapi",
    "express": "/Users/ivan.seredkin/_projects/public-budi-bench/express",
}

PROMPT_FILES = {
    "flask": "scripts/dev/benchmarks/flask-structural-v1.prompts.json",
    "react": "scripts/dev/benchmarks/react-structural-v1.prompts.json",
    "django": "scripts/dev/benchmarks/django-v1.prompts.json",
    "ripgrep": "scripts/dev/benchmarks/ripgrep-v1.prompts.json",
    "terraform": "scripts/dev/benchmarks/terraform-v1.prompts.json",
    "fastify": "scripts/dev/benchmarks/fastify-v1.prompts.json",
    "fastapi": "scripts/dev/benchmarks/fastapi-v1.prompts.json",
    "express": "scripts/dev/benchmarks/express-v1.prompts.json",
}

DAEMON_URL = "http://localhost:7878/query"


def query_daemon(repo_root: str, prompt: str) -> dict:
    """Send a query with dump_candidates=true and return the full response."""
    payload = json.dumps({
        "repo_root": repo_root,
        "prompt": prompt,
        "dump_candidates": True,
    }).encode()
    req = urllib.request.Request(
        DAEMON_URL, data=payload,
        headers={"Content-Type": "application/json"}
    )
    resp = urllib.request.urlopen(req, timeout=30)
    return json.loads(resp.read())


def main():
    parser = argparse.ArgumentParser(description="Capture channel scores for weight optimization")
    parser.add_argument("--output", "-o", default="scripts/dev/channel_scores.jsonl",
                        help="Output JSONL file")
    parser.add_argument("--repos", nargs="*", default=None,
                        help="Repos to process (default: all)")
    args = parser.parse_args()

    budi_root = Path(__file__).parent.parent.parent
    repos = args.repos or list(BENCHMARK_REPOS.keys())

    total_prompts = 0
    total_candidates = 0

    with open(args.output, "w") as out:
        for repo_name in repos:
            repo_root = BENCHMARK_REPOS.get(repo_name)
            prompt_file = PROMPT_FILES.get(repo_name)
            if not repo_root or not prompt_file:
                print(f"  SKIP {repo_name}: not configured", file=sys.stderr)
                continue

            prompt_path = budi_root / prompt_file
            if not prompt_path.exists():
                print(f"  SKIP {repo_name}: {prompt_path} not found", file=sys.stderr)
                continue

            prompts = json.loads(prompt_path.read_text())
            print(f"\n{repo_name}: {len(prompts)} prompts, repo={repo_root}", file=sys.stderr)

            for i, prompt in enumerate(prompts):
                try:
                    response = query_daemon(repo_root, prompt)
                except Exception as e:
                    print(f"  P{i+1} ERROR: {e}", file=sys.stderr)
                    continue

                diag = response.get("diagnostics", {})
                candidates = diag.get("candidates", [])
                selected = response.get("snippets", [])
                intent = diag.get("intent", "unknown")
                top_score = diag.get("top_score", 0.0)

                # Write one record per prompt with all candidates
                record = {
                    "repo": repo_name,
                    "prompt_index": i + 1,
                    "prompt": prompt,
                    "intent": intent,
                    "top_score": top_score,
                    "selected_count": len(selected),
                    "candidate_count": len(candidates),
                    "selected_paths": [s["path"] for s in selected],
                    "candidates": candidates,
                }
                out.write(json.dumps(record) + "\n")

                total_prompts += 1
                total_candidates += len(candidates)

                status = f"top={top_score:.3f}" if top_score > 0 else "skip"
                print(f"  P{i+1}: {intent} {status} candidates={len(candidates)} selected={len(selected)}",
                      file=sys.stderr)

    print(f"\nDone: {total_prompts} prompts, {total_candidates} candidates → {args.output}",
          file=sys.stderr)


if __name__ == "__main__":
    main()
