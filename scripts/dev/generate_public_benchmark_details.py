#!/usr/bin/env python3
"""Generate docs/benchmark-details.md from the latest A/B benchmark data.

Auto-discovers benchmark results from ~/.local/share/budi/repos/*/benchmarks/.
Picks the latest full-tier run per repo. Deduplicates repos indexed from
different paths (e.g. react from two locations).
"""
from __future__ import annotations

import json
import argparse
import os
import subprocess
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


SCRIPT_DIR = Path(__file__).resolve().parent
PROJECT_ROOT = SCRIPT_DIR.parents[1]
BUDI_DATA_DIR = Path(
    os.environ.get("BUDI_DATA_DIR", str(Path.home() / ".local" / "share" / "budi" / "repos"))
).expanduser()

# Canonical repo metadata (key = lowercase basename of repo_root)
REPO_META: dict[str, dict[str, str]] = {
    "react": {"name": "React", "url": "https://github.com/facebook/react", "lang": "JavaScript"},
    "flask": {"name": "Flask", "url": "https://github.com/pallets/flask", "lang": "Python"},
    "django": {"name": "Django", "url": "https://github.com/djangoproject/django", "lang": "Python"},
    "fastapi": {"name": "FastAPI", "url": "https://github.com/fastapi/fastapi", "lang": "Python"},
    "fastify": {"name": "Fastify", "url": "https://github.com/fastify/fastify", "lang": "JavaScript"},
    "express": {"name": "Express", "url": "https://github.com/expressjs/express", "lang": "JavaScript"},
    "ripgrep": {"name": "ripgrep", "url": "https://github.com/BurntSushi/ripgrep", "lang": "Rust"},
    "terraform": {"name": "Terraform", "url": "https://github.com/hashicorp/terraform", "lang": "Go"},
}


@dataclass
class BenchmarkRun:
    repo_key: str
    name: str
    url: str
    lang: str
    repo_root: str
    results_path: Path
    data: dict[str, Any]


def discover_runs() -> list[BenchmarkRun]:
    """Find the latest full benchmark run for each unique repo."""
    candidates: dict[str, BenchmarkRun] = {}

    for repo_dir in sorted(BUDI_DATA_DIR.iterdir()):
        bench = repo_dir / "benchmarks"
        if not bench.is_dir():
            continue

        for run_dir in sorted(bench.iterdir(), reverse=True):
            results = run_dir / "ab-results.json"
            if not results.exists():
                continue
            try:
                data = json.loads(results.read_text())
            except (json.JSONDecodeError, OSError):
                continue

            rows = data.get("rows", [])
            if len(rows) < 5:
                continue

            repo_root = data.get("repo_root", "")
            repo_basename = Path(repo_root).name.lower()

            meta = REPO_META.get(repo_basename)
            if not meta:
                continue

            # Prefer full runs over focused; skip if we already have this repo
            if repo_basename in candidates:
                existing = candidates[repo_basename]
                existing_tier = existing.data.get("run_options", {}).get("validation_tier", "")
                new_tier = data.get("run_options", {}).get("validation_tier", "")
                # Prefer full over focused, and more rows
                if existing_tier == "full" and new_tier != "full":
                    continue
                if len(existing.data.get("rows", [])) >= len(rows) and existing_tier == "full":
                    continue

            candidates[repo_basename] = BenchmarkRun(
                repo_key=repo_basename,
                name=meta["name"],
                url=meta["url"],
                lang=meta["lang"],
                repo_root=repo_root,
                results_path=results,
                data=data,
            )
            break  # Take the latest qualifying run for this budi repo entry

    # Sort by display order
    order = ["react", "flask", "django", "fastapi", "fastify", "express", "ripgrep", "terraform"]
    return sorted(candidates.values(), key=lambda r: order.index(r.repo_key) if r.repo_key in order else 99)


def short_commit(repo_path: str) -> str:
    try:
        out = subprocess.check_output(
            ["git", "-C", repo_path, "rev-parse", "--short", "HEAD"],
            text=True,
            stderr=subprocess.DEVNULL,
        )
        return out.strip()
    except Exception:
        return "unknown"


def pct_delta(with_value: float, no_value: float) -> float:
    if no_value == 0:
        return 0.0
    return ((with_value - no_value) / no_value) * 100.0


def clip(text: Any, max_chars: int = 1800) -> str:
    raw = str(text or "").strip()
    if len(raw) <= max_chars:
        return raw
    return raw[:max_chars] + f"\n...[truncated {len(raw) - max_chars} chars]"


def fence_safe(text: str) -> str:
    return text.replace("```", "``\\`")


def main() -> None:
    parser = argparse.ArgumentParser(description="Generate public benchmark details markdown.")
    parser.add_argument(
        "--output",
        default=str(PROJECT_ROOT / "docs" / "benchmark-details.md"),
        help="Output markdown path",
    )
    args = parser.parse_args()

    runs = discover_runs()
    if not runs:
        print("No benchmark data found. Run benchmarks first.")
        return

    # Aggregate stats
    all_judges: list[dict[str, Any]] = []
    agg_no_cost = agg_with_cost = 0.0
    total_rows = 0

    for run in runs:
        summary = run.data.get("summary", {})
        agg_no_cost += float(summary.get("no_budi", {}).get("cost_usd_total", 0.0))
        agg_with_cost += float(summary.get("with_budi", {}).get("cost_usd_total", 0.0))
        for row in run.data.get("rows", []):
            total_rows += 1
            judge = row.get("judge", {})
            if judge.get("winner"):
                all_judges.append(judge)

    cost_reduction = -pct_delta(agg_with_cost, agg_no_cost)
    with_wins = sum(1 for j in all_judges if j.get("winner") == "with_budi")
    no_wins = sum(1 for j in all_judges if j.get("winner") == "no_budi")
    ties = sum(1 for j in all_judges if j.get("winner") == "tie")
    non_regressions = with_wins + ties
    judged = with_wins + no_wins + ties

    lines: list[str] = []
    lines.append("# Public Benchmark Details")
    lines.append("")
    lines.append("Reproducible A/B benchmark of `budi` across 8 open-source repositories.")
    lines.append("Each case includes prompts, hook injection traces, both model responses, and LLM judge rationales.")
    lines.append("")
    lines.append(f"- Generated: {datetime.now(timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')}")
    lines.append(f"- budi version: 3.1.0")
    lines.append("- Runner: `scripts/dev/ab_benchmark_runner.py`")
    lines.append(f"- Repos: {len(runs)}")
    lines.append(f"- Total prompts judged: {judged}")
    lines.append("")

    # Repositories table
    lines.append("## Repositories")
    lines.append("")
    lines.append("| Repo | Language | URL | Commit | Prompts |")
    lines.append("| --- | --- | --- | --- | ---: |")
    for run in runs:
        commit = short_commit(run.repo_root)
        n = len(run.data.get("rows", []))
        lines.append(f"| {run.name} | {run.lang} | [{run.url}]({run.url}) | `{commit}` | {n} |")
    lines.append("")

    # Topline
    lines.append("## Topline")
    lines.append("")
    lines.append("| Metric | Result |")
    lines.append("| --- | --- |")
    lines.append(f"| Non-regressions | **{non_regressions}/{judged} ({100*non_regressions//judged}%)** |")
    lines.append(f"| Wins (budi better) | {with_wins} |")
    lines.append(f"| Ties (same quality, lower cost) | {ties} |")
    lines.append(f"| Regressions (quality drops) | {no_wins} |")
    lines.append(f"| Total cost savings | **{cost_reduction:.0f}%** |")
    lines.append("")
    lines.append("```mermaid")
    lines.append("pie showData")
    lines.append('    title "Judge outcomes across all repos"')
    lines.append(f'    "budi wins" : {with_wins}')
    lines.append(f'    "ties" : {ties}')
    lines.append(f'    "regressions" : {no_wins}')
    lines.append("```")
    lines.append("")

    # Per-repo summary
    lines.append("## Per-Repo Summary")
    lines.append("")
    lines.append("| Repo | Prompts | Non-reg | Wins | Ties | Losses | Cost delta | Avg Q delta | Avg G delta |")
    lines.append("| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: |")
    for run in runs:
        js = run.data.get("judge_summary", {})
        summary = run.data.get("summary", {})
        no_cost = float(summary.get("no_budi", {}).get("cost_usd_total", 0.0))
        with_cost = float(summary.get("with_budi", {}).get("cost_usd_total", 0.0))
        cost_d = pct_delta(with_cost, no_cost)
        w = js.get("with_budi_wins", 0)
        n = js.get("no_budi_wins", 0)
        t = js.get("ties", 0)
        total = w + n + t
        nr = w + t
        q_no = js.get("avg_score_no_budi", 0)
        q_with = js.get("avg_score_with_budi", 0)
        g_no = js.get("avg_grounding_no_budi", 0)
        g_with = js.get("avg_grounding_with_budi", 0)
        q_delta = q_with - q_no if q_with and q_no else 0
        g_delta = g_with - g_no if g_with and g_no else 0
        lines.append(
            f"| {run.name} | {total} | **{nr}/{total}** | {w} | {t} | {n} | {cost_d:+.0f}% | {q_delta:+.2f} | {g_delta:+.2f} |"
        )
    lines.append("")

    # Full case evidence per repo
    lines.append("## Full Case Evidence")
    lines.append("")
    lines.append("Each case includes: prompt, hook injection trace, both responses, and judge rationale.")
    lines.append("")

    for run in runs:
        lines.append(f"### {run.name}")
        lines.append("")
        for idx, row in enumerate(run.data.get("rows", []), start=1):
            no = row.get("no_budi", {})
            with_budi = row.get("with_budi", {})
            judge = row.get("judge", {})
            hook = row.get("with_budi_hook", {})
            hook_out = hook.get("output", {}) if isinstance(hook, dict) else {}
            prompt = row.get("prompt", "")
            winner = judge.get("winner", "n/a")
            q_no = judge.get("quality_no_budi", judge.get("score_no_budi", "?"))
            q_with = judge.get("quality_with_budi", judge.get("score_with_budi", "?"))
            g_no = judge.get("grounding_no_budi", "?")
            g_with = judge.get("grounding_with_budi", "?")

            title = f"{run.name} P{idx} | winner={winner} | Q {q_no}→{q_with} G {g_no}→{g_with}"
            lines.append(f"<details><summary>{title}</summary>")
            lines.append("")
            lines.append(f"**Prompt:** {prompt}")
            lines.append("")
            lines.append(f"- Interactions: no_budi={no.get('num_turns', '?')} / with_budi={with_budi.get('num_turns', '?')}")
            lines.append(
                f"- Cost USD: no_budi=${float(no.get('total_cost_usd', 0.0)):.4f} / with_budi=${float(with_budi.get('total_cost_usd', 0.0)):.4f}"
            )
            ctx = hook_out.get("context_chars", "n/a")
            reason = hook_out.get("reason", "n/a")
            lines.append(f"- Hook: reason={reason} context_chars={ctx}")
            lines.append("")

            # Injected context
            excerpt = hook_out.get("context_excerpt", "")
            if excerpt:
                lines.append("#### Injected context")
                lines.append("")
                lines.append("```text")
                lines.append(fence_safe(clip(excerpt, 1200)))
                lines.append("```")
                lines.append("")

            # Responses
            lines.append("#### Response (`no_budi`)")
            lines.append("")
            lines.append("```text")
            lines.append(fence_safe(clip(no.get("result", ""), 1500)))
            lines.append("```")
            lines.append("")
            lines.append("#### Response (`with_budi`)")
            lines.append("")
            lines.append("```text")
            lines.append(fence_safe(clip(with_budi.get("result", ""), 1500)))
            lines.append("```")
            lines.append("")

            # Judge
            justification = judge.get("justification", "")
            if justification:
                lines.append("#### Judge rationale")
                lines.append("")
                lines.append("```text")
                lines.append(fence_safe(clip(justification, 1500)))
                lines.append("```")
                lines.append("")

            lines.append("</details>")
            lines.append("")

    output_path = Path(args.output).expanduser().resolve()
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text("\n".join(lines) + "\n")
    print(f"Wrote {output_path} ({len(runs)} repos, {total_rows} prompts)")


if __name__ == "__main__":
    main()
