#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import re
import statistics
import subprocess
import sys
import time
from collections import Counter, defaultdict
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


@dataclass
class CmdResult:
    args: list[str]
    returncode: int
    stdout: str
    stderr: str
    duration_ms: float


@dataclass
class RepoSpec:
    repo_id: str
    repo_root: Path


@dataclass
class VariantSpec:
    variant_id: str
    overrides: dict[str, Any]
    enabled: bool
    requires_slm_critic: bool
    notes: str
    ab_runner_extra_args: list[str]


def run_cmd(args: list[str], cwd: Path, timeout_sec: int) -> CmdResult:
    started = time.perf_counter()
    proc = subprocess.run(
        args,
        cwd=str(cwd),
        capture_output=True,
        text=True,
        timeout=timeout_sec,
    )
    elapsed_ms = (time.perf_counter() - started) * 1000.0
    return CmdResult(
        args=args,
        returncode=proc.returncode,
        stdout=proc.stdout,
        stderr=proc.stderr,
        duration_ms=elapsed_ms,
    )


def safe_num(value: Any) -> float | None:
    try:
        return float(value)
    except Exception:  # noqa: BLE001
        return None


def mean(values: list[float]) -> float:
    if not values:
        return 0.0
    return statistics.fmean(values)


def pct_improvement(baseline: float, candidate: float) -> float:
    if baseline <= 0.0:
        return 0.0
    return ((baseline - candidate) / baseline) * 100.0


def normalize_id(raw: str, fallback: str) -> str:
    cleaned = re.sub(r"[^a-z0-9_-]+", "-", raw.lower()).strip("-")
    return cleaned or fallback


def load_matrix_config(path: Path, enable_slm_critic: bool) -> tuple[str, list[RepoSpec], list[VariantSpec], dict[str, float]]:
    if not path.exists():
        raise SystemExit(f"Matrix config not found: {path}")
    try:
        payload = json.loads(path.read_text())
    except json.JSONDecodeError as exc:
        raise SystemExit(f"Invalid JSON in matrix config {path}: {exc}") from exc
    if not isinstance(payload, dict):
        raise SystemExit("Matrix config must be a JSON object")

    matrix_name = str(payload.get("name") or path.stem)

    repos_raw = payload.get("repos")
    if not isinstance(repos_raw, list) or not repos_raw:
        raise SystemExit("Matrix config must include a non-empty `repos` array")
    repos: list[RepoSpec] = []
    for idx, item in enumerate(repos_raw, start=1):
        if not isinstance(item, dict):
            raise SystemExit(f"repos[{idx}] must be an object")
        repo_root_raw = str(item.get("repo_root", "")).strip()
        if not repo_root_raw:
            raise SystemExit(f"repos[{idx}] missing `repo_root`")
        repo_root = Path(repo_root_raw).expanduser().resolve()
        if not repo_root.exists():
            raise SystemExit(f"repo path does not exist: {repo_root}")
        repo_id_raw = str(item.get("id") or repo_root.name)
        repo_id = normalize_id(repo_id_raw, f"repo-{idx}")
        repos.append(RepoSpec(repo_id=repo_id, repo_root=repo_root))

    variants_raw = payload.get("variants")
    if not isinstance(variants_raw, list) or not variants_raw:
        raise SystemExit("Matrix config must include a non-empty `variants` array")
    variants: list[VariantSpec] = []
    for idx, item in enumerate(variants_raw, start=1):
        if not isinstance(item, dict):
            raise SystemExit(f"variants[{idx}] must be an object")
        variant_id_raw = str(item.get("id", "")).strip()
        if not variant_id_raw:
            raise SystemExit(f"variants[{idx}] missing `id`")
        variant_id = normalize_id(variant_id_raw, f"variant-{idx}")
        overrides = item.get("overrides", {})
        if not isinstance(overrides, dict):
            raise SystemExit(f"variants[{idx}].overrides must be an object")
        enabled = bool(item.get("enabled", True))
        requires_slm_critic = bool(item.get("requires_slm_critic", False))
        if requires_slm_critic and not enable_slm_critic:
            enabled = False
        notes = str(item.get("notes", ""))
        extra_args = item.get("ab_runner_extra_args", [])
        if not isinstance(extra_args, list):
            raise SystemExit(f"variants[{idx}].ab_runner_extra_args must be an array")
        variants.append(
            VariantSpec(
                variant_id=variant_id,
                overrides=overrides,
                enabled=enabled,
                requires_slm_critic=requires_slm_critic,
                notes=notes,
                ab_runner_extra_args=[str(v) for v in extra_args],
            )
        )

    gates_raw = payload.get("success_gates", {})
    if gates_raw and not isinstance(gates_raw, dict):
        raise SystemExit("success_gates must be an object")
    gates = {
        "bad_fast_api_rate_all_max": float(gates_raw.get("bad_fast_api_rate_all_max", 20.0)),
        "bad_fast_api_rate_when_faster_max": float(
            gates_raw.get("bad_fast_api_rate_when_faster_max", 30.0)
        ),
        "grounding_delta_min": float(gates_raw.get("grounding_delta_min", 0.0)),
    }

    return matrix_name, repos, variants, gates


def write_variant_override_files(out_dir: Path, variants: list[VariantSpec]) -> dict[str, Path]:
    override_dir = out_dir / "_variant_overrides"
    override_dir.mkdir(parents=True, exist_ok=True)
    mapping: dict[str, Path] = {}
    for variant in variants:
        if not variant.overrides:
            continue
        path = override_dir / f"{variant.variant_id}.json"
        path.write_text(json.dumps(variant.overrides, indent=2, sort_keys=True))
        mapping[variant.variant_id] = path
    return mapping


def extract_cases_from_results(path: Path, variant_id: str, repo_id: str, repeat_idx: int) -> list[dict[str, Any]]:
    try:
        payload = json.loads(path.read_text())
    except json.JSONDecodeError as exc:
        raise RuntimeError(f"Invalid results JSON at {path}: {exc}") from exc
    rows = payload.get("rows", [])
    if not isinstance(rows, list):
        return []
    cases: list[dict[str, Any]] = []
    for row in rows:
        if not isinstance(row, dict):
            continue
        judge = row.get("judge", {}) if isinstance(row.get("judge", {}), dict) else {}
        winner = str(judge.get("winner", ""))
        hook = row.get("with_budi_hook", {}) if isinstance(row.get("with_budi_hook", {}), dict) else {}
        hook_output = hook.get("output", {}) if isinstance(hook.get("output", {}), dict) else {}
        no_budi = row.get("no_budi", {}) if isinstance(row.get("no_budi", {}), dict) else {}
        with_budi = row.get("with_budi", {}) if isinstance(row.get("with_budi", {}), dict) else {}
        cases.append(
            {
                "variant_id": variant_id,
                "repo_id": repo_id,
                "repeat_idx": repeat_idx,
                "prompt": str(row.get("prompt", "")),
                "winner": winner if winner in {"no_budi", "with_budi", "tie"} else "",
                "judge_ok": bool(judge.get("ok")),
                "score_no_budi": safe_num(judge.get("score_no_budi")),
                "score_with_budi": safe_num(judge.get("score_with_budi")),
                "grounding_no_budi": safe_num(judge.get("grounding_no_budi")),
                "grounding_with_budi": safe_num(judge.get("grounding_with_budi")),
                "api_no_budi_ms": safe_num(no_budi.get("duration_api_ms") or no_budi.get("duration_ms")),
                "api_with_budi_ms": safe_num(with_budi.get("duration_api_ms") or with_budi.get("duration_ms")),
                "wall_no_budi_ms": safe_num(no_budi.get("wall_duration_ms")),
                "wall_with_budi_ms": safe_num(with_budi.get("wall_duration_ms")),
                "cost_no_budi_usd": safe_num(no_budi.get("total_cost_usd")),
                "cost_with_budi_usd": safe_num(with_budi.get("total_cost_usd")),
                "hook_reason": str(hook_output.get("reason", "")),
                "hook_intent": str(hook_output.get("retrieval_intent", "")),
                "hook_recommended_injection": bool(hook_output.get("recommended_injection", False)),
            }
        )
    return cases


def summarize_cases(cases: list[dict[str, Any]]) -> dict[str, Any]:
    judged = [c for c in cases if c["judge_ok"] and c["winner"] in {"no_budi", "with_budi", "tie"}]

    no_api = [c["api_no_budi_ms"] for c in cases if c["api_no_budi_ms"] is not None and c["api_with_budi_ms"] is not None]
    with_api = [c["api_with_budi_ms"] for c in cases if c["api_no_budi_ms"] is not None and c["api_with_budi_ms"] is not None]
    no_wall = [
        c["wall_no_budi_ms"]
        for c in cases
        if c["wall_no_budi_ms"] is not None and c["wall_with_budi_ms"] is not None
    ]
    with_wall = [
        c["wall_with_budi_ms"]
        for c in cases
        if c["wall_no_budi_ms"] is not None and c["wall_with_budi_ms"] is not None
    ]
    no_cost = [c["cost_no_budi_usd"] for c in cases if c["cost_no_budi_usd"] is not None]
    with_cost = [c["cost_with_budi_usd"] for c in cases if c["cost_with_budi_usd"] is not None]

    faster_api_cases = [
        c for c in judged if c["api_no_budi_ms"] is not None and c["api_with_budi_ms"] is not None and c["api_with_budi_ms"] < c["api_no_budi_ms"]
    ]
    faster_wall_cases = [
        c for c in judged if c["wall_no_budi_ms"] is not None and c["wall_with_budi_ms"] is not None and c["wall_with_budi_ms"] < c["wall_no_budi_ms"]
    ]

    bad_fast_api_cases = [c for c in faster_api_cases if c["winner"] == "no_budi"]
    bad_fast_wall_cases = [c for c in faster_wall_cases if c["winner"] == "no_budi"]

    grounding_deltas = [
        (c["grounding_with_budi"] - c["grounding_no_budi"])
        for c in judged
        if c["grounding_with_budi"] is not None and c["grounding_no_budi"] is not None
    ]
    score_deltas = [
        (c["score_with_budi"] - c["score_no_budi"])
        for c in judged
        if c["score_with_budi"] is not None and c["score_no_budi"] is not None
    ]

    reason_counts = Counter(c["hook_reason"] for c in cases if c["hook_reason"])
    inject_ok_count = sum(1 for c in cases if c["hook_reason"] == "ok")
    skip_count = sum(1 for c in cases if c["hook_reason"].startswith("skip:"))

    return {
        "cases_total": len(cases),
        "judged_cases": len(judged),
        "with_budi_wins": sum(1 for c in judged if c["winner"] == "with_budi"),
        "no_budi_wins": sum(1 for c in judged if c["winner"] == "no_budi"),
        "ties": sum(1 for c in judged if c["winner"] == "tie"),
        "api_avg_no_budi_ms": mean([v for v in no_api if v is not None]),
        "api_avg_with_budi_ms": mean([v for v in with_api if v is not None]),
        "wall_avg_no_budi_ms": mean([v for v in no_wall if v is not None]),
        "wall_avg_with_budi_ms": mean([v for v in with_wall if v is not None]),
        "api_delta_pct": pct_improvement(
            mean([v for v in no_api if v is not None]),
            mean([v for v in with_api if v is not None]),
        ),
        "wall_delta_pct": pct_improvement(
            mean([v for v in no_wall if v is not None]),
            mean([v for v in with_wall if v is not None]),
        ),
        "cost_total_no_budi_usd": sum(v for v in no_cost if v is not None),
        "cost_total_with_budi_usd": sum(v for v in with_cost if v is not None),
        "cost_delta_pct": pct_improvement(
            sum(v for v in no_cost if v is not None),
            sum(v for v in with_cost if v is not None),
        ),
        "avg_grounding_delta": mean(grounding_deltas),
        "avg_score_delta": mean(score_deltas),
        "faster_api_cases": len(faster_api_cases),
        "faster_wall_cases": len(faster_wall_cases),
        "bad_fast_api_count": len(bad_fast_api_cases),
        "bad_fast_wall_count": len(bad_fast_wall_cases),
        "bad_fast_api_rate_all_pct": ((len(bad_fast_api_cases) / len(judged)) * 100.0) if judged else 0.0,
        "bad_fast_wall_rate_all_pct": ((len(bad_fast_wall_cases) / len(judged)) * 100.0) if judged else 0.0,
        "bad_fast_api_rate_when_faster_pct": (
            (len(bad_fast_api_cases) / len(faster_api_cases)) * 100.0 if faster_api_cases else 0.0
        ),
        "bad_fast_wall_rate_when_faster_pct": (
            (len(bad_fast_wall_cases) / len(faster_wall_cases)) * 100.0 if faster_wall_cases else 0.0
        ),
        "hook_inject_ok_count": inject_ok_count,
        "hook_skip_count": skip_count,
        "hook_reason_counts": dict(sorted(reason_counts.items())),
    }


def evaluate_variant_gates(metrics: dict[str, Any], gates: dict[str, float]) -> dict[str, Any]:
    checks = {
        "has_judged_cases": metrics.get("judged_cases", 0) > 0,
        "bad_fast_api_rate_all": metrics.get("bad_fast_api_rate_all_pct", 0.0)
        <= gates["bad_fast_api_rate_all_max"],
        "bad_fast_api_rate_when_faster": metrics.get("bad_fast_api_rate_when_faster_pct", 0.0)
        <= gates["bad_fast_api_rate_when_faster_max"],
        "grounding_delta": metrics.get("avg_grounding_delta", 0.0) >= gates["grounding_delta_min"],
    }
    return {
        "pass": all(checks.values()),
        "checks": checks,
    }


def build_markdown_summary(
    matrix_name: str,
    prompts_file: Path,
    repeats: int,
    gates: dict[str, float],
    variants: list[VariantSpec],
    variant_summaries: dict[str, dict[str, Any]],
) -> str:
    lines: list[str] = []
    lines.append(f"# Pivot Matrix Report ({matrix_name})")
    lines.append("")
    lines.append(f"- Generated at: {datetime.now(timezone.utc).isoformat()}")
    lines.append(f"- Prompts file: `{prompts_file}`")
    lines.append(f"- Repeats: {repeats}")
    lines.append(
        f"- Gates: bad_fast_all<={gates['bad_fast_api_rate_all_max']:.2f}%, "
        f"bad_fast_when_faster<={gates['bad_fast_api_rate_when_faster_max']:.2f}%, "
        f"grounding_delta>={gates['grounding_delta_min']:.2f}"
    )
    lines.append("")
    lines.append(
        "| Variant | Cases | Judged | with_budi wins | no_budi wins | bad_fast_all | bad_fast_when_faster | grounding_delta | API delta | Wall delta | Cost delta | Gate |"
    )
    lines.append("|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|")
    for variant in variants:
        summary = variant_summaries.get(variant.variant_id, {})
        metrics = summary.get("metrics", {})
        gate = summary.get("gate", {})
        lines.append(
            f"| `{variant.variant_id}` | "
            f"{int(metrics.get('cases_total', 0))} | "
            f"{int(metrics.get('judged_cases', 0))} | "
            f"{int(metrics.get('with_budi_wins', 0))} | "
            f"{int(metrics.get('no_budi_wins', 0))} | "
            f"{metrics.get('bad_fast_api_rate_all_pct', 0.0):.2f}% | "
            f"{metrics.get('bad_fast_api_rate_when_faster_pct', 0.0):.2f}% | "
            f"{metrics.get('avg_grounding_delta', 0.0):.3f} | "
            f"{metrics.get('api_delta_pct', 0.0):.2f}% | "
            f"{metrics.get('wall_delta_pct', 0.0):.2f}% | "
            f"{metrics.get('cost_delta_pct', 0.0):.2f}% | "
            f"{'pass' if gate.get('pass') else 'fail'} |"
        )
    lines.append("")
    lines.append("## Variant Notes")
    lines.append("")
    for variant in variants:
        note = variant.notes or "n/a"
        lines.append(f"- `{variant.variant_id}`: {note}")
    lines.append("")
    return "\n".join(lines)


def main() -> None:
    parser = argparse.ArgumentParser(description="Run and aggregate pivot experiment matrix")
    parser.add_argument("--matrix-file", required=True, help="JSON matrix config file")
    parser.add_argument("--prompts-file", required=True, help="Prompt file passed to A/B runner")
    parser.add_argument(
        "--ab-runner-script",
        default="scripts/ab_benchmark_runner.py",
        help="Path to A/B runner script",
    )
    parser.add_argument("--out-dir", default="", help="Output directory for matrix run artifacts")
    parser.add_argument("--repeats", type=int, default=3, help="Number of repeats per repo/variant")
    parser.add_argument(
        "--repo-id",
        action="append",
        default=[],
        help="Only run selected repo IDs from matrix (repeatable)",
    )
    parser.add_argument(
        "--variant-id",
        action="append",
        default=[],
        help="Only run selected variant IDs from matrix (repeatable)",
    )
    parser.add_argument(
        "--timeout-sec",
        type=int,
        default=900,
        help="Timeout per A/B run command",
    )
    parser.add_argument(
        "--enable-slm-critic",
        action="store_true",
        help="Enable variants marked requires_slm_critic=true",
    )
    parser.add_argument(
        "--continue-on-error",
        action="store_true",
        help="Continue matrix execution when one run fails",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Print commands without executing benchmark runs",
    )
    parser.add_argument(
        "--ab-max-prompts",
        type=int,
        default=0,
        help="Pass --max-prompts to A/B runner (0 = all prompts)",
    )
    parser.add_argument(
        "--ab-judge-limit",
        type=int,
        default=0,
        help="Pass --judge-limit to A/B runner (0 = judge all)",
    )
    parser.add_argument(
        "--ab-skip-judge",
        action="store_true",
        help="Pass --skip-judge to A/B runner",
    )
    parser.add_argument(
        "--ab-claude-timeout-sec",
        type=int,
        default=0,
        help="Pass --claude-timeout-sec to A/B runner (0 = runner default)",
    )
    parser.add_argument(
        "--ab-judge-timeout-sec",
        type=int,
        default=0,
        help="Pass --judge-timeout-sec to A/B runner (0 = runner default)",
    )
    parser.add_argument(
        "--ab-disable-with-budi-retry",
        action="store_true",
        help="Pass --disable-with-budi-retry to A/B runner",
    )
    args = parser.parse_args()

    matrix_file = Path(args.matrix_file).expanduser().resolve()
    prompts_file = Path(args.prompts_file).expanduser().resolve()
    if not prompts_file.exists():
        raise SystemExit(f"Prompts file not found: {prompts_file}")
    ab_runner_script = Path(args.ab_runner_script).expanduser().resolve()
    if not ab_runner_script.exists():
        raise SystemExit(f"A/B runner script not found: {ab_runner_script}")
    if args.repeats < 1:
        raise SystemExit("--repeats must be >= 1")
    if args.ab_max_prompts < 0:
        raise SystemExit("--ab-max-prompts must be >= 0")
    if args.ab_judge_limit < 0:
        raise SystemExit("--ab-judge-limit must be >= 0")
    if args.ab_claude_timeout_sec < 0:
        raise SystemExit("--ab-claude-timeout-sec must be >= 0")
    if args.ab_judge_timeout_sec < 0:
        raise SystemExit("--ab-judge-timeout-sec must be >= 0")

    matrix_name, repos, variants_all, gates = load_matrix_config(matrix_file, args.enable_slm_critic)
    if args.repo_id:
        wanted_repo_ids = {normalize_id(v, "") for v in args.repo_id}
        repos = [repo for repo in repos if repo.repo_id in wanted_repo_ids]
        if not repos:
            raise SystemExit("--repo-id filter removed all repos from matrix")
    variants = [variant for variant in variants_all if variant.enabled]
    if args.variant_id:
        wanted_variant_ids = {normalize_id(v, "") for v in args.variant_id}
        variants = [variant for variant in variants if variant.variant_id in wanted_variant_ids]
    if not variants:
        raise SystemExit("No enabled variants to run")

    timestamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    out_dir = (
        Path(args.out_dir).expanduser().resolve()
        if args.out_dir
        else (Path.cwd() / "tmp" / f"pivot_matrix_{timestamp}")
    )
    out_dir.mkdir(parents=True, exist_ok=True)

    override_files = write_variant_override_files(out_dir, variants)

    run_records: list[dict[str, Any]] = []
    for repeat_idx in range(1, args.repeats + 1):
        for repo in repos:
            for variant in variants:
                run_dir = out_dir / f"repeat_{repeat_idx:02d}" / repo.repo_id / variant.variant_id
                run_dir.mkdir(parents=True, exist_ok=True)
                run_label = f"{matrix_name}:{variant.variant_id}:r{repeat_idx}:{repo.repo_id}"
                cmd = [
                    sys.executable,
                    str(ab_runner_script),
                    "--repo-root",
                    str(repo.repo_root),
                    "--prompts-file",
                    str(prompts_file),
                    "--no-default-prompts",
                    "--out-dir",
                    str(run_dir),
                    "--run-label",
                    run_label,
                    "--variant-id",
                    variant.variant_id,
                ]
                override_path = override_files.get(variant.variant_id)
                if override_path:
                    cmd.extend(["--variant-overrides-file", str(override_path)])
                if args.ab_max_prompts > 0:
                    cmd.extend(["--max-prompts", str(args.ab_max_prompts)])
                if args.ab_judge_limit > 0:
                    cmd.extend(["--judge-limit", str(args.ab_judge_limit)])
                if args.ab_skip_judge:
                    cmd.append("--skip-judge")
                if args.ab_claude_timeout_sec > 0:
                    cmd.extend(["--claude-timeout-sec", str(args.ab_claude_timeout_sec)])
                if args.ab_judge_timeout_sec > 0:
                    cmd.extend(["--judge-timeout-sec", str(args.ab_judge_timeout_sec)])
                if args.ab_disable_with_budi_retry:
                    cmd.append("--disable-with-budi-retry")
                cmd.extend(variant.ab_runner_extra_args)

                if args.dry_run:
                    print("[pivot][dry-run]", " ".join(cmd), flush=True)
                    run_records.append(
                        {
                            "repo_id": repo.repo_id,
                            "repo_root": str(repo.repo_root),
                            "variant_id": variant.variant_id,
                            "repeat_idx": repeat_idx,
                            "command": cmd,
                            "status": "dry-run",
                            "run_dir": str(run_dir),
                        }
                    )
                    continue

                print(
                    f"[pivot] repeat={repeat_idx}/{args.repeats} "
                    f"repo={repo.repo_id} variant={variant.variant_id}",
                    flush=True,
                )
                result = run_cmd(cmd, cwd=Path.cwd(), timeout_sec=args.timeout_sec)
                status = "ok" if result.returncode == 0 else "failed"
                record = {
                    "repo_id": repo.repo_id,
                    "repo_root": str(repo.repo_root),
                    "variant_id": variant.variant_id,
                    "repeat_idx": repeat_idx,
                    "command": cmd,
                    "status": status,
                    "returncode": result.returncode,
                    "duration_ms": result.duration_ms,
                    "run_dir": str(run_dir),
                    "stderr_tail": result.stderr[-4000:],
                }
                run_records.append(record)
                if result.returncode != 0 and not args.continue_on_error:
                    summary_path = out_dir / "pivot-matrix-run-records.json"
                    summary_path.write_text(json.dumps({"run_records": run_records}, indent=2))
                    raise SystemExit(
                        f"Matrix run failed for repo={repo.repo_id} variant={variant.variant_id} "
                        f"(see {summary_path})"
                    )

    all_cases: list[dict[str, Any]] = []
    for record in run_records:
        if record.get("status") != "ok":
            continue
        result_path = Path(record["run_dir"]) / "ab-results.json"
        if not result_path.exists():
            record["status"] = "missing-results"
            continue
        cases = extract_cases_from_results(
            result_path,
            variant_id=record["variant_id"],
            repo_id=record["repo_id"],
            repeat_idx=int(record["repeat_idx"]),
        )
        all_cases.extend(cases)

    variant_summaries: dict[str, dict[str, Any]] = {}
    for variant in variants:
        variant_cases = [case for case in all_cases if case["variant_id"] == variant.variant_id]
        metrics = summarize_cases(variant_cases)
        gate = evaluate_variant_gates(metrics, gates)
        per_repo: dict[str, Any] = {}
        for repo in repos:
            repo_cases = [case for case in variant_cases if case["repo_id"] == repo.repo_id]
            per_repo[repo.repo_id] = summarize_cases(repo_cases)
        variant_summaries[variant.variant_id] = {
            "variant": {
                "id": variant.variant_id,
                "overrides": variant.overrides,
                "notes": variant.notes,
                "requires_slm_critic": variant.requires_slm_critic,
            },
            "metrics": metrics,
            "gate": gate,
            "per_repo": per_repo,
        }

    summary = {
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "matrix_file": str(matrix_file),
        "matrix_name": matrix_name,
        "prompts_file": str(prompts_file),
        "repeats": args.repeats,
        "success_gates": gates,
        "variants": variant_summaries,
        "run_records": run_records,
    }

    json_path = out_dir / "pivot-matrix-summary.json"
    json_path.write_text(json.dumps(summary, indent=2))

    md = build_markdown_summary(
        matrix_name=matrix_name,
        prompts_file=prompts_file,
        repeats=args.repeats,
        gates=gates,
        variants=variants,
        variant_summaries=variant_summaries,
    )
    md_path = out_dir / "pivot-matrix-summary.md"
    md_path.write_text(md)

    run_records_path = out_dir / "pivot-matrix-run-records.json"
    run_records_path.write_text(json.dumps({"run_records": run_records}, indent=2))

    print(f"[pivot] summary json: {json_path}")
    print(f"[pivot] summary md:   {md_path}")
    print(f"[pivot] run records:  {run_records_path}")


if __name__ == "__main__":
    main()

