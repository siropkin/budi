#!/usr/bin/env python3
from __future__ import annotations

import json
import argparse
import os
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


@dataclass
class RepoRun:
    key: str
    name: str
    url: str
    local_path: Path
    results_path: Path


REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_REPO_BASE = Path(
    os.environ.get("PUBLIC_BENCH_REPO_BASE", str(Path.home() / "_projects" / "public-budi-bench"))
).expanduser()


def default_runs() -> list[RepoRun]:
    return [
        RepoRun(
            key="react",
            name="React",
            url="https://github.com/facebook/react",
            local_path=DEFAULT_REPO_BASE / "react",
            results_path=REPO_ROOT / "tmp" / "public_bench_react_v2b" / "ab-results.json",
        ),
        RepoRun(
            key="flask",
            name="Flask",
            url="https://github.com/pallets/flask",
            local_path=DEFAULT_REPO_BASE / "flask",
            results_path=REPO_ROOT / "tmp" / "public_bench_flask_v2b" / "ab-results.json",
        ),
        RepoRun(
            key="express",
            name="Express",
            url="https://github.com/expressjs/express",
            local_path=DEFAULT_REPO_BASE / "express",
            results_path=REPO_ROOT / "tmp" / "public_bench_express_v2b" / "ab-results.json",
        ),
    ]


def short_commit(repo_path: Path) -> str:
    head = repo_path.joinpath(".git")
    if not head.exists():
        return "unknown"
    import subprocess

    try:
        out = subprocess.check_output(
            ["git", "-C", str(repo_path), "rev-parse", "--short", "HEAD"],
            text=True,
        )
        return out.strip()
    except Exception:  # noqa: BLE001
        return "unknown"


def pct_delta(with_value: float, no_value: float) -> float:
    if no_value == 0:
        return 0.0
    return ((with_value - no_value) / no_value) * 100.0


def improvement_percent(with_value: float, no_value: float) -> float:
    return -pct_delta(with_value, no_value)


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
        default=str(REPO_ROOT / "docs" / "benchmark-details.md"),
        help="Output markdown path",
    )
    parser.add_argument(
        "--summary-output",
        default=str(REPO_ROOT / "tmp" / "public_bench_summary_v2b.json"),
        help="Output summary JSON path",
    )
    args = parser.parse_args()

    runs = default_runs()
    payloads: list[tuple[RepoRun, dict[str, Any]]] = []
    for run in runs:
        payloads.append((run, json.loads(run.results_path.read_text())))

    all_rows: list[tuple[str, dict[str, Any]]] = []
    judges: list[dict[str, Any]] = []
    agg_no_api = agg_with_api = 0.0
    agg_no_wall = agg_with_wall = 0.0
    agg_no_cost = agg_with_cost = 0.0
    prompt_fingerprint = ""
    prompts: list[str] = []

    for run, payload in payloads:
        summary = payload["summary"]
        no = summary["no_budi"]
        with_budi = summary["with_budi"]
        agg_no_api += float(no.get("duration_ms_avg", 0.0)) * float(no.get("runs", 0.0))
        agg_with_api += float(with_budi.get("duration_ms_avg", 0.0)) * float(with_budi.get("runs", 0.0))
        agg_no_wall += float(no.get("wall_duration_ms_avg", 0.0)) * float(no.get("runs", 0.0))
        agg_with_wall += float(with_budi.get("wall_duration_ms_avg", 0.0)) * float(with_budi.get("runs", 0.0))
        agg_no_cost += float(no.get("cost_usd_total", 0.0))
        agg_with_cost += float(with_budi.get("cost_usd_total", 0.0))
        for row in payload.get("rows", []):
            all_rows.append((run.key, row))
            judge = row.get("judge", {})
            if judge.get("ok"):
                judges.append(judge)
        if not prompt_fingerprint:
            prompt_fingerprint = payload.get("prompt_set", {}).get("fingerprint_sha256", "")
            prompts = payload.get("prompts", [])

    api_improvement = improvement_percent(agg_with_api, agg_no_api)
    wall_improvement = improvement_percent(agg_with_wall, agg_no_wall)
    cost_reduction = improvement_percent(agg_with_cost, agg_no_cost)
    with_wins = sum(1 for j in judges if j.get("winner") == "with_budi")
    no_wins = sum(1 for j in judges if j.get("winner") == "no_budi")
    ties = sum(1 for j in judges if j.get("winner") == "tie")
    quality_delta = (
        sum(float(j.get("score_with_budi", 0.0)) - float(j.get("score_no_budi", 0.0)) for j in judges)
        / max(len(judges), 1)
    )
    grounding_delta = (
        sum(
            float(j.get("grounding_with_budi", 0.0)) - float(j.get("grounding_no_budi", 0.0))
            for j in judges
        )
        / max(len(judges), 1)
    )

    hook_outputs = [
        row.get("with_budi_hook", {}).get("output", {})
        for _, row in all_rows
        if isinstance(row.get("with_budi_hook", {}).get("output", {}), dict)
    ]
    hook_ok = [
        output
        for output in hook_outputs
        if output.get("success") and output.get("reason") == "ok"
    ]
    avg_context_chars = sum(float(output.get("context_chars", 0.0)) for output in hook_outputs) / max(
        len(hook_outputs), 1
    )

    lines: list[str] = []
    lines.append("# Public Benchmark Details")
    lines.append("")
    lines.append("This report is a reproducible, fully public A/B benchmark of `budi` with exact repos, prompts,")
    lines.append("session-level hook evidence, responses, and judge rationales.")
    lines.append("")
    lines.append(f"- Generated (UTC): {datetime.now(timezone.utc).isoformat()}")
    lines.append("- Source runner: `scripts/ab_benchmark_runner.py`")
    lines.append("- Report generator: `scripts/generate_public_benchmark_details.py`")
    lines.append(f"- Prompt-set fingerprint: `{prompt_fingerprint}`")
    lines.append(f"- Cases: {len(all_rows)} (3 repos x 3 prompts)")
    lines.append("")
    lines.append("## Repositories")
    lines.append("")
    lines.append("| Repo | URL | Commit |")
    lines.append("| --- | --- | --- |")
    for run, _ in payloads:
        lines.append(f"| {run.name} | [{run.url}]({run.url}) | `{short_commit(run.local_path)}` |")
    lines.append("")
    lines.append("## Topline")
    lines.append("")
    lines.append("| Metric | Result |")
    lines.append("| --- | --- |")
    lines.append(f"| API speed (aggregate) | **{api_improvement:.2f}% faster** with `budi` |")
    lines.append(f"| End-to-end speed (aggregate) | **{wall_improvement:.2f}% faster** with `budi` |")
    lines.append(f"| Total cost (aggregate) | **{cost_reduction:.2f}% lower** with `budi` |")
    lines.append(f"| Judge wins | `with_budi` {with_wins} / `no_budi` {no_wins} / tie {ties} |")
    lines.append(f"| Avg quality delta | `{quality_delta:+.2f}` (`with_budi - no_budi`) |")
    lines.append(f"| Avg grounding delta | `{grounding_delta:+.2f}` (`with_budi - no_budi`) |")
    lines.append(f"| Hook injection success | `{len(hook_ok)}/{len(all_rows)}` rows had `reason=ok` |")
    lines.append(f"| Avg injected context chars | `{avg_context_chars:.0f}` |")
    lines.append("")
    lines.append("```mermaid")
    lines.append("xychart-beta")
    lines.append('    title "Aggregate improvement with budi (higher is better)"')
    lines.append('    x-axis ["API speed", "Wall speed", "Cost reduction"]')
    ymax = max(api_improvement, wall_improvement, cost_reduction, 1.0)
    lines.append(f'    y-axis "Percent" 0 --> {int(ymax) + 5}')
    lines.append(
        f"    bar [{api_improvement:.2f}, {wall_improvement:.2f}, {cost_reduction:.2f}]"
    )
    lines.append("```")
    lines.append("")
    lines.append("```mermaid")
    lines.append("pie showData")
    lines.append('    title Judge winner split')
    lines.append(f'    "with_budi" : {with_wins}')
    lines.append(f'    "no_budi" : {no_wins}')
    lines.append(f'    "tie" : {ties}')
    lines.append("```")
    lines.append("")
    lines.append("## Prompts")
    lines.append("")
    for idx, prompt in enumerate(prompts, start=1):
        lines.append(f"{idx}. {prompt}")
    lines.append("")
    lines.append("## Repo Summary")
    lines.append("")
    lines.append("| Repo | API delta (with-no) | Wall delta (with-no) | Cost delta (with-no) | Judge wins (with/no/tie) |")
    lines.append("| --- | ---: | ---: | ---: | --- |")
    for run, payload in payloads:
        no = payload["summary"]["no_budi"]
        with_budi = payload["summary"]["with_budi"]
        api_delta = pct_delta(float(with_budi["duration_ms_avg"]), float(no["duration_ms_avg"]))
        wall_delta = pct_delta(float(with_budi["wall_duration_ms_avg"]), float(no["wall_duration_ms_avg"]))
        cost_delta = pct_delta(float(with_budi["cost_usd_total"]), float(no["cost_usd_total"]))
        js = payload["judge_summary"]
        lines.append(
            "| {name} | {api:+.2f}% | {wall:+.2f}% | {cost:+.2f}% | {w}/{n}/{t} |".format(
                name=run.name,
                api=api_delta,
                wall=wall_delta,
                cost=cost_delta,
                w=js.get("with_budi_wins", 0),
                n=js.get("no_budi_wins", 0),
                t=js.get("ties", 0),
            )
        )
    lines.append("")
    lines.append("## Full Case Evidence")
    lines.append("")
    lines.append("Each case includes: prompt, hook injection trace (`with_budi`), both final model responses,")
    lines.append("interaction counts, and judge justification.")
    lines.append("")

    for run, payload in payloads:
        lines.append(f"### {run.name}")
        lines.append("")
        for idx, row in enumerate(payload.get("rows", []), start=1):
            no = row.get("no_budi", {})
            with_budi = row.get("with_budi", {})
            judge = row.get("judge", {})
            hook = row.get("with_budi_hook", {})
            hook_out = hook.get("output", {}) if isinstance(hook, dict) else {}
            prompt = row.get("prompt", "")
            title = f"{run.name} · Prompt {idx} · winner={judge.get('winner', 'n/a')}"
            lines.append(f"<details><summary>{title}</summary>")
            lines.append("")
            lines.append(f"- Prompt: {prompt}")
            lines.append(
                f"- Interactions: no_budi={no.get('num_turns', 'n/a')} / with_budi={with_budi.get('num_turns', 'n/a')}"
            )
            lines.append(
                f"- API duration ms: no_budi={no.get('duration_ms', 'n/a')} / with_budi={with_budi.get('duration_ms', 'n/a')}"
            )
            lines.append(
                f"- Cost USD: no_budi={float(no.get('total_cost_usd', 0.0)):.5f} / with_budi={float(with_budi.get('total_cost_usd', 0.0)):.5f}"
            )
            lines.append(
                f"- Hook output: success={hook_out.get('success', 'n/a')} reason={hook_out.get('reason', 'n/a')} context_chars={hook_out.get('context_chars', 'n/a')} retry={row.get('with_budi_hook_retry', False)}"
            )
            lines.append("")
            lines.append("#### What budi injected")
            lines.append("")
            lines.append("```text")
            lines.append(fence_safe(clip(hook_out.get("context_excerpt", ""), 1500)))
            lines.append("```")
            lines.append("")
            lines.append("#### Final response (`no_budi`)")
            lines.append("")
            lines.append("```text")
            lines.append(fence_safe(clip(no.get("result", ""), 2000)))
            lines.append("```")
            lines.append("")
            lines.append("#### Final response (`with_budi`)")
            lines.append("")
            lines.append("```text")
            lines.append(fence_safe(clip(with_budi.get("result", ""), 2000)))
            lines.append("```")
            lines.append("")
            lines.append("#### Judge rationale")
            lines.append("")
            lines.append("```text")
            lines.append(fence_safe(clip(judge.get("justification", ""), 1800)))
            lines.append("```")
            lines.append("")
            lines.append("</details>")
            lines.append("")

    output_path = Path(args.output).expanduser().resolve()
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text("\n".join(lines) + "\n")
    print(f"Wrote {output_path}")

    summary = {
        "api_improvement_percent": api_improvement,
        "wall_improvement_percent": wall_improvement,
        "cost_reduction_percent": cost_reduction,
        "with_budi_wins": with_wins,
        "no_budi_wins": no_wins,
        "ties": ties,
        "quality_delta": quality_delta,
        "grounding_delta": grounding_delta,
        "hook_ok_rows": len(hook_ok),
        "total_rows": len(all_rows),
    }
    summary_path = Path(args.summary_output).expanduser().resolve()
    summary_path.parent.mkdir(parents=True, exist_ok=True)
    summary_path.write_text(json.dumps(summary, indent=2))
    print(f"Wrote {summary_path}")


if __name__ == "__main__":
    main()
