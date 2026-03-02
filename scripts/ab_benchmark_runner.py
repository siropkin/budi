#!/usr/bin/env python3
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import statistics
import subprocess
import textwrap
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


DEFAULT_PROMPT_SET_NAME = "generic-v1"
DEFAULT_PROMPTS = [
    "Map the high-level architecture of this repository. List key modules/directories and responsibilities with exact file paths. Do not edit files.",
    "Identify where runtime configuration and environment variables are loaded and validated. Include key functions and likely failure modes. Do not edit files.",
    "Trace one critical request or data flow end-to-end from an entrypoint to a final output. Include exact files and functions. Do not edit files.",
    "Propose 5 high-value tests for a critical subsystem in this repo, with concrete target files and test names. Do not edit files.",
    "Spot one likely security or reliability risk in this repo and propose a safe fix plan with exact files touched. Do not edit files.",
]


@dataclass
class CmdResult:
    args: list[str]
    returncode: int
    stdout: str
    stderr: str
    duration_ms: float


def run_cmd(
    args: list[str],
    cwd: Path,
    input_text: str | None = None,
    timeout_sec: int = 300,
) -> CmdResult:
    started = time.perf_counter()
    proc = subprocess.run(
        args,
        cwd=str(cwd),
        input=input_text,
        text=True,
        capture_output=True,
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


def parse_json_output(raw: str) -> dict[str, Any]:
    raw = raw.strip()
    if not raw:
        raise ValueError("Empty output")
    try:
        return json.loads(raw)
    except json.JSONDecodeError:
        pass
    for line in reversed(raw.splitlines()):
        line = line.strip()
        if not line:
            continue
        try:
            return json.loads(line)
        except json.JSONDecodeError:
            continue
    raise ValueError(f"Unable to parse JSON output:\n{raw[:1000]}")


def has_judge_fields(payload: Any) -> bool:
    if not isinstance(payload, dict):
        return False
    required = {
        "winner",
        "score_no_budi",
        "score_with_budi",
        "grounding_no_budi",
        "grounding_with_budi",
        "actionability_no_budi",
        "actionability_with_budi",
        "justification",
    }
    return required.issubset(set(payload.keys()))


def extract_judge_payload(parsed: dict[str, Any]) -> dict[str, Any] | None:
    candidates: list[dict[str, Any]] = []
    if has_judge_fields(parsed):
        candidates.append(parsed)

    result = parsed.get("result")
    if isinstance(result, dict) and has_judge_fields(result):
        candidates.append(result)
    elif isinstance(result, str):
        try:
            maybe = json.loads(result)
            if has_judge_fields(maybe):
                candidates.append(maybe)
        except json.JSONDecodeError:
            pass

    content = parsed.get("content")
    if isinstance(content, dict) and has_judge_fields(content):
        candidates.append(content)
    elif isinstance(content, str):
        try:
            maybe = json.loads(content)
            if has_judge_fields(maybe):
                candidates.append(maybe)
        except json.JSONDecodeError:
            pass

    structured_output = parsed.get("structured_output")
    if isinstance(structured_output, dict) and has_judge_fields(structured_output):
        candidates.append(structured_output)
    elif isinstance(structured_output, str):
        try:
            maybe = json.loads(structured_output)
            if has_judge_fields(maybe):
                candidates.append(maybe)
        except json.JSONDecodeError:
            pass

    return candidates[0] if candidates else None


def parse_prompts_file(path: Path) -> list[str]:
    raw = path.read_text()
    stripped = raw.strip()
    if not stripped:
        return []

    # Support JSON arrays for multi-line prompts; fallback to line-based format.
    if stripped.startswith("["):
        try:
            payload = json.loads(stripped)
            if isinstance(payload, list):
                return [str(x).strip() for x in payload if str(x).strip()]
        except json.JSONDecodeError:
            pass

    prompts: list[str] = []
    for line in raw.splitlines():
        item = line.strip()
        if not item or item.startswith("#"):
            continue
        prompts.append(item)
    return prompts


def normalize_prompt(prompt: str) -> str:
    return re.sub(r"\s+", " ", prompt).strip()


def dedupe_prompts(prompts: list[str]) -> list[str]:
    out: list[str] = []
    seen: set[str] = set()
    for prompt in prompts:
        normalized = normalize_prompt(prompt)
        if not normalized or normalized in seen:
            continue
        seen.add(normalized)
        out.append(normalized)
    return out


def resolve_prompts(
    prompts_file: str,
    inline_prompts: list[str],
    use_default_prompts: bool,
) -> tuple[list[str], str]:
    collected: list[str] = []
    sources: list[str] = []

    if prompts_file:
        pf = Path(prompts_file).expanduser().resolve()
        if not pf.exists():
            raise SystemExit(f"Prompts file not found: {pf}")
        file_prompts = parse_prompts_file(pf)
        if not file_prompts:
            raise SystemExit(f"Prompts file is empty: {pf}")
        collected.extend(file_prompts)
        sources.append(f"file:{pf}")

    if inline_prompts:
        collected.extend(inline_prompts)
        sources.append("cli")

    if not collected and use_default_prompts:
        collected = list(DEFAULT_PROMPTS)
        sources.append(f"default:{DEFAULT_PROMPT_SET_NAME}")

    prompts = dedupe_prompts(collected)
    if not prompts:
        raise SystemExit(
            "No prompts to run. Use --prompt, --prompts-file, or omit --no-default-prompts."
        )

    return prompts, "+".join(sources)


def prompt_set_fingerprint(prompts: list[str]) -> str:
    canonical = "\n".join(prompts)
    return hashlib.sha256(canonical.encode("utf-8")).hexdigest()


def budi_home_dir() -> Path:
    override = os.environ.get("BUDI_HOME")
    if override:
        return Path(override).expanduser().resolve()
    return (Path.home() / ".local" / "share" / "budi").resolve()


def repo_storage_id(repo_root: Path) -> str:
    canonical = repo_root.resolve()
    normalized = str(canonical).replace("\\", "/")
    digest = hashlib.sha256(normalized.encode("utf-8")).hexdigest()
    slug = re.sub(r"[^a-z0-9_-]+", "-", repo_root.name.lower()).strip("-") or "repo"
    slug = slug[:32]
    return f"{slug}-{digest[:12]}"


def budi_repo_paths(repo_root: Path) -> dict[str, Path]:
    data_dir = budi_home_dir() / "repos" / repo_storage_id(repo_root)
    return {
        "data_dir": data_dir,
        "config": data_dir / "config.toml",
        "hook_log": data_dir / "logs" / "hook-io.jsonl",
        "benchmarks": data_dir / "benchmarks",
    }


def resolve_budi_config_path(repo_root: Path) -> Path:
    return budi_repo_paths(repo_root)["config"]


def resolve_hook_log_path(repo_root: Path) -> Path:
    return budi_repo_paths(repo_root)["hook_log"]


def ensure_budi_ready(repo_root: Path) -> None:
    r = run_cmd(["budi", "--version"], cwd=repo_root, timeout_sec=30)
    if r.returncode != 0:
        raise RuntimeError("budi is not installed or not on PATH")

    run_cmd(["budi", "init"], cwd=repo_root, timeout_sec=120)
    index = run_cmd(["budi", "index"], cwd=repo_root, timeout_sec=1800)
    if index.returncode != 0:
        # fallback to hard if needed
        hard = run_cmd(["budi", "index", "--hard"], cwd=repo_root, timeout_sec=1800)
        if hard.returncode != 0:
            raise RuntimeError(
                f"Failed to index repo:\nindex stderr:\n{index.stderr}\n\nhard stderr:\n{hard.stderr}"
            )


def ensure_debug_io_enabled(repo_root: Path) -> str:
    cfg_path = resolve_budi_config_path(repo_root)
    if not cfg_path.exists():
        raise RuntimeError(f"Missing config file: {cfg_path}")

    original_raw = cfg_path.read_text()
    raw = original_raw
    pairs = {
        "debug_io": "true",
        "debug_io_full_text": "false",
        "debug_io_max_chars": "1500",
    }
    for key, value in pairs.items():
        pattern = re.compile(rf"^{re.escape(key)}\s*=.*$", re.MULTILINE)
        if pattern.search(raw):
            raw = pattern.sub(f"{key} = {value}", raw)
        else:
            if not raw.endswith("\n"):
                raw += "\n"
            raw += f"{key} = {value}\n"
    if raw != original_raw:
        cfg_path.write_text(raw)

    # restart daemon so updated config is applied during hook runs
    run_cmd(["pkill", "-f", "budi-daemon serve"], cwd=repo_root, timeout_sec=30)
    run_cmd(["budi", "init"], cwd=repo_root, timeout_sec=120)
    return original_raw


def restore_debug_io_config(repo_root: Path, original_raw: str) -> None:
    cfg_path = resolve_budi_config_path(repo_root)
    if not cfg_path.exists():
        return
    current_raw = cfg_path.read_text()
    if current_raw != original_raw:
        cfg_path.write_text(original_raw)
    run_cmd(["pkill", "-f", "budi-daemon serve"], cwd=repo_root, timeout_sec=30)
    run_cmd(["budi", "init"], cwd=repo_root, timeout_sec=120)


def run_claude_prompt(
    repo_root: Path,
    prompt: str,
    disable_hooks: bool,
    timeout_sec: int = 420,
) -> dict[str, Any]:
    settings = json.dumps({"disableAllHooks": disable_hooks})
    args = [
        "claude",
        "-p",
        "--output-format",
        "json",
        "--permission-mode",
        "bypassPermissions",
        "--dangerously-skip-permissions",
        "--tools",
        "Read,Grep,Glob,LS",
        "--settings",
        settings,
    ]
    cmd = run_cmd(args, cwd=repo_root, input_text=prompt, timeout_sec=timeout_sec)
    if cmd.returncode != 0:
        return {
            "ok": False,
            "error": f"claude exited with {cmd.returncode}",
            "stderr": cmd.stderr[-2000:],
            "stdout": cmd.stdout[-2000:],
            "wall_duration_ms": cmd.duration_ms,
        }
    try:
        parsed = parse_json_output(cmd.stdout)
    except Exception as exc:  # noqa: BLE001
        return {
            "ok": False,
            "error": f"json_parse_error: {exc}",
            "stderr": cmd.stderr[-2000:],
            "stdout": cmd.stdout[-2000:],
            "wall_duration_ms": cmd.duration_ms,
        }
    parsed["ok"] = True
    parsed["wall_duration_ms"] = cmd.duration_ms
    return parsed


def truncate_text(text: str, max_chars: int = 4000) -> str:
    if len(text) <= max_chars:
        return text
    return text[:max_chars] + f"\n...[truncated {len(text) - max_chars} chars]"


def judge_pair(repo_root: Path, prompt: str, no_budi_result: str, with_budi_result: str) -> dict[str, Any]:
    schema = {
        "type": "object",
        "properties": {
            "winner": {"type": "string", "enum": ["no_budi", "with_budi", "tie"]},
            "score_no_budi": {"type": "number"},
            "score_with_budi": {"type": "number"},
            "grounding_no_budi": {"type": "number"},
            "grounding_with_budi": {"type": "number"},
            "actionability_no_budi": {"type": "number"},
            "actionability_with_budi": {"type": "number"},
            "justification": {"type": "string"},
        },
        "required": [
            "winner",
            "score_no_budi",
            "score_with_budi",
            "grounding_no_budi",
            "grounding_with_budi",
            "actionability_no_budi",
            "actionability_with_budi",
            "justification",
        ],
        "additionalProperties": False,
    }

    judge_prompt = textwrap.dedent(
        f"""
        You are evaluating two coding-assistant responses to the SAME prompt in the SAME repository.
        Evaluate quality on:
        1) overall quality/correctness (0-10)
        2) grounding in repository specifics (0-10)
        3) actionability/practical usefulness (0-10)

        Return strict JSON only according to the provided schema.

        Prompt:
        {prompt}

        Response A (no_budi):
        {truncate_text(no_budi_result)}

        Response B (with_budi):
        {truncate_text(with_budi_result)}
        """
    ).strip()

    settings = json.dumps({"disableAllHooks": True})
    args = [
        "claude",
        "-p",
        "--output-format",
        "json",
        "--permission-mode",
        "bypassPermissions",
        "--dangerously-skip-permissions",
        "--tools",
        "",
        "--settings",
        settings,
        "--json-schema",
        json.dumps(schema),
    ]
    cmd = run_cmd(args, cwd=repo_root, input_text=judge_prompt, timeout_sec=300)
    if cmd.returncode != 0:
        return {
            "ok": False,
            "error": f"judge_failed_exit_{cmd.returncode}",
            "stderr": cmd.stderr[-1200:],
        }
    try:
        parsed = parse_json_output(cmd.stdout)
    except Exception as exc:  # noqa: BLE001
        return {
            "ok": False,
            "error": f"judge_parse_error: {exc}",
            "stderr": cmd.stderr[-1200:],
            "stdout": cmd.stdout[-1500:],
        }

    judged = extract_judge_payload(parsed)
    if not judged:
        return {
            "ok": False,
            "error": "judge_result_json_parse_error",
            "raw_result": cmd.stdout[-1500:],
        }
    judged["ok"] = True
    return judged


def safe_num(value: Any, default: float = 0.0) -> float:
    try:
        return float(value)
    except Exception:  # noqa: BLE001
        return default


def aggregate(rows: list[dict[str, Any]], mode: str) -> dict[str, float]:
    selected = [r[mode] for r in rows if r[mode].get("ok")]
    if not selected:
        return {}
    durations = [safe_num(x.get("duration_ms")) for x in selected]
    costs = [safe_num(x.get("total_cost_usd")) for x in selected]
    in_tokens = [safe_num((x.get("usage") or {}).get("input_tokens")) for x in selected]
    out_tokens = [safe_num((x.get("usage") or {}).get("output_tokens")) for x in selected]
    cache_create = [
        safe_num((x.get("usage") or {}).get("cache_creation_input_tokens")) for x in selected
    ]
    cache_read = [safe_num((x.get("usage") or {}).get("cache_read_input_tokens")) for x in selected]
    wall = [safe_num(x.get("wall_duration_ms")) for x in selected]
    return {
        "runs": float(len(selected)),
        "duration_ms_avg": statistics.fmean(durations),
        "duration_ms_median": statistics.median(durations),
        "wall_duration_ms_avg": statistics.fmean(wall),
        "cost_usd_total": sum(costs),
        "cost_usd_avg": statistics.fmean(costs),
        "input_tokens_total": sum(in_tokens),
        "output_tokens_total": sum(out_tokens),
        "cache_create_total": sum(cache_create),
        "cache_read_total": sum(cache_read),
    }


def build_markdown(
    repo_root: Path,
    prompts: list[str],
    rows: list[dict[str, Any]],
    summary: dict[str, Any],
    judge_summary: dict[str, Any],
    prompt_set: dict[str, Any],
    run_label: str,
) -> str:
    lines: list[str] = []
    lines.append(f"# A/B Benchmark Report ({repo_root.name})")
    lines.append("")
    lines.append(f"- Generated at: {datetime.now(timezone.utc).isoformat()}")
    if run_label:
        lines.append(f"- Run label: `{run_label}`")
    lines.append(f"- Prompt source: `{prompt_set.get('source', 'unknown')}`")
    lines.append(f"- Prompt count: {int(prompt_set.get('count', len(prompts)))}")
    lines.append(f"- Prompt set SHA256: `{prompt_set.get('fingerprint_sha256', '')}`")
    lines.append("- Mode A: `disableAllHooks=true` (no_budi)")
    lines.append("- Mode B: `disableAllHooks=false` (with_budi)")
    lines.append("- Claude run mode: `claude -p --output-format json`")
    lines.append("")
    lines.append("## Quantitative Summary")
    lines.append("")
    lines.append("| Metric | no_budi | with_budi | Delta (with-no) |")
    lines.append("|---|---:|---:|---:|")
    for metric in [
        ("duration_ms_avg", "Avg API duration ms"),
        ("wall_duration_ms_avg", "Avg wall duration ms"),
        ("cost_usd_total", "Total cost USD"),
        ("input_tokens_total", "Total input tokens"),
        ("output_tokens_total", "Total output tokens"),
        ("cache_create_total", "Total cache creation tokens"),
        ("cache_read_total", "Total cache read tokens"),
    ]:
        key, label = metric
        a = summary.get("no_budi", {}).get(key, 0.0)
        b = summary.get("with_budi", {}).get(key, 0.0)
        delta = b - a
        lines.append(f"| {label} | {a:,.2f} | {b:,.2f} | {delta:,.2f} |")
    lines.append("")
    lines.append("## Response Quality (LLM Judge)")
    lines.append("")
    lines.append("| Metric | Value |")
    lines.append("|---|---:|")
    lines.append(f"| Prompts evaluated | {len(prompts)} |")
    lines.append(f"| Winner: with_budi | {judge_summary.get('with_budi_wins', 0)} |")
    lines.append(f"| Winner: no_budi | {judge_summary.get('no_budi_wins', 0)} |")
    lines.append(f"| Winner: tie | {judge_summary.get('ties', 0)} |")
    lines.append(f"| Avg quality score (no_budi) | {judge_summary.get('avg_score_no_budi', 0):.2f} |")
    lines.append(f"| Avg quality score (with_budi) | {judge_summary.get('avg_score_with_budi', 0):.2f} |")
    lines.append(
        f"| Avg grounding score (no_budi) | {judge_summary.get('avg_grounding_no_budi', 0):.2f} |"
    )
    lines.append(
        f"| Avg grounding score (with_budi) | {judge_summary.get('avg_grounding_with_budi', 0):.2f} |"
    )
    lines.append("")
    lines.append("## Per-Prompt Outcomes")
    lines.append("")
    lines.append(
        "| # | Prompt (short) | no_budi cost | with_budi cost | no_budi ms | with_budi ms | Judge winner |"
    )
    lines.append("|---:|---|---:|---:|---:|---:|---|")
    for i, row in enumerate(rows, start=1):
        prompt_short = row["prompt"][:72].replace("\n", " ")
        a = row["no_budi"]
        b = row["with_budi"]
        judge = row.get("judge", {})
        lines.append(
            f"| {i} | {prompt_short} | {safe_num(a.get('total_cost_usd')):.5f} | "
            f"{safe_num(b.get('total_cost_usd')):.5f} | "
            f"{safe_num(a.get('duration_ms')):.0f} | {safe_num(b.get('duration_ms')):.0f} | "
            f"{judge.get('winner', 'n/a')} |"
        )
    lines.append("")
    return "\n".join(lines)


def collect_hook_log_example(repo_root: Path) -> dict[str, Any]:
    path = resolve_hook_log_path(repo_root)
    if not path.exists():
        return {}
    lines = path.read_text().splitlines()
    user_input = None
    user_output = None
    for line in reversed(lines):
        try:
            obj = json.loads(line)
        except json.JSONDecodeError:
            continue
        if obj.get("event") == "UserPromptSubmit" and obj.get("phase") == "output" and user_output is None:
            user_output = obj
        if obj.get("event") == "UserPromptSubmit" and obj.get("phase") == "input" and user_input is None:
            user_input = obj
        if user_input and user_output:
            break
    return {"input": user_input, "output": user_output}


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Run A/B benchmark of Claude responses with and without budi hooks"
    )
    parser.add_argument("--repo-root", required=True, help="Target repository path")
    parser.add_argument(
        "--prompts-file",
        default="",
        help="Prompt file: one prompt per line, or a JSON array of prompts",
    )
    parser.add_argument(
        "--prompt",
        action="append",
        default=[],
        help="Inline prompt text (repeat flag to add multiple prompts)",
    )
    parser.add_argument(
        "--no-default-prompts",
        action="store_true",
        help="Require --prompt/--prompts-file (disable built-in generic prompt set)",
    )
    parser.add_argument(
        "--out-dir",
        default="",
        help="Output directory (default: <budi-home>/repos/<repo-id>/benchmarks/<timestamp>)",
    )
    parser.add_argument(
        "--reuse-results-json",
        default="",
        help="Optional existing ab-results.json to reuse no_budi/with_budi runs and recompute judge scores",
    )
    parser.add_argument(
        "--run-label",
        default="",
        help="Optional label stored in output for cross-run/repo comparisons",
    )
    args = parser.parse_args()

    repo_root = Path(args.repo_root).expanduser().resolve()
    if not repo_root.exists():
        raise SystemExit(f"Repo does not exist: {repo_root}")

    prompts: list[str] = []
    prompt_source = ""
    if not args.reuse_results_json:
        prompts, prompt_source = resolve_prompts(
            prompts_file=args.prompts_file,
            inline_prompts=args.prompt,
            use_default_prompts=not args.no_default_prompts,
        )

    ts = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    out_dir = Path(args.out_dir).expanduser().resolve() if args.out_dir else (
        budi_repo_paths(repo_root)["benchmarks"] / ts
    )
    out_dir.mkdir(parents=True, exist_ok=True)

    rows: list[dict[str, Any]] = []
    source_results_json = ""
    original_config_raw: str | None = None
    try:
        if args.reuse_results_json:
            source_path = Path(args.reuse_results_json).expanduser().resolve()
            if not source_path.exists():
                raise SystemExit(f"reuse results file not found: {source_path}")
            source_results_json = str(source_path)
            loaded = json.loads(source_path.read_text())
            loaded_rows = loaded.get("rows")
            if not isinstance(loaded_rows, list) or not loaded_rows:
                raise SystemExit("reuse results file has no rows")
            rows = loaded_rows
            prompts = [str(r.get("prompt", "")) for r in rows if str(r.get("prompt", "")).strip()]
            prompt_source = f"reuse:{source_path}"
            print(f"[ab] reusing rows from: {source_path}", flush=True)
        else:
            ensure_budi_ready(repo_root)
            original_config_raw = ensure_debug_io_enabled(repo_root)

            for idx, prompt in enumerate(prompts, start=1):
                print(f"[ab] prompt {idx}/{len(prompts)}", flush=True)
                no_budi = run_claude_prompt(repo_root, prompt, disable_hooks=True)
                with_budi = run_claude_prompt(repo_root, prompt, disable_hooks=False)
                rows.append(
                    {
                        "prompt": prompt,
                        "no_budi": no_budi,
                        "with_budi": with_budi,
                        "judge": {"ok": False},
                    }
                )

        prompt_set = {
            "name": DEFAULT_PROMPT_SET_NAME if prompt_source.startswith("default:") else "custom",
            "source": prompt_source or "unknown",
            "count": len(prompts),
            "fingerprint_sha256": prompt_set_fingerprint(prompts),
        }
        print(
            f"[ab] prompts={prompt_set['count']} source={prompt_set['source']} "
            f"sha256={prompt_set['fingerprint_sha256'][:12]}",
            flush=True,
        )

        # Always (re)run judge pass so quality metrics reflect latest parser logic.
        for idx, row in enumerate(rows, start=1):
            prompt = str(row.get("prompt", ""))
            no_budi = row.get("no_budi", {})
            with_budi = row.get("with_budi", {})
            judge = {"ok": False}
            if isinstance(no_budi, dict) and isinstance(with_budi, dict) and no_budi.get("ok") and with_budi.get("ok"):
                print(f"[ab] judging {idx}/{len(rows)}", flush=True)
                judge = judge_pair(
                    repo_root,
                    prompt,
                    str(no_budi.get("result", "")),
                    str(with_budi.get("result", "")),
                )
            row["judge"] = judge

        summary = {
            "no_budi": aggregate(rows, "no_budi"),
            "with_budi": aggregate(rows, "with_budi"),
        }

        judges = [r["judge"] for r in rows if r.get("judge", {}).get("ok")]
        with_budi_wins = sum(1 for j in judges if j.get("winner") == "with_budi")
        no_budi_wins = sum(1 for j in judges if j.get("winner") == "no_budi")
        ties = sum(1 for j in judges if j.get("winner") == "tie")
        judge_summary = {
            "with_budi_wins": with_budi_wins,
            "no_budi_wins": no_budi_wins,
            "ties": ties,
            "avg_score_no_budi": statistics.fmean([safe_num(j.get("score_no_budi")) for j in judges])
            if judges
            else 0.0,
            "avg_score_with_budi": statistics.fmean([safe_num(j.get("score_with_budi")) for j in judges])
            if judges
            else 0.0,
            "avg_grounding_no_budi": statistics.fmean([safe_num(j.get("grounding_no_budi")) for j in judges])
            if judges
            else 0.0,
            "avg_grounding_with_budi": statistics.fmean(
                [safe_num(j.get("grounding_with_budi")) for j in judges]
            )
            if judges
            else 0.0,
        }

        hook_example = collect_hook_log_example(repo_root)
        results = {
            "generated_at": datetime.now(timezone.utc).isoformat(),
            "repo_root": str(repo_root),
            "run_label": args.run_label,
            "source_results_json": source_results_json,
            "prompt_set": prompt_set,
            "prompts": prompts,
            "rows": rows,
            "summary": summary,
            "judge_summary": judge_summary,
            "hook_log_example": hook_example,
        }
        json_path = out_dir / "ab-results.json"
        json_path.write_text(json.dumps(results, indent=2))

        md = build_markdown(repo_root, prompts, rows, summary, judge_summary, prompt_set, args.run_label)
        md_path = out_dir / "ab-results.md"
        md_path.write_text(md)

        print(f"[ab] results json: {json_path}")
        print(f"[ab] results md:   {md_path}")
    finally:
        if original_config_raw is not None:
            restore_debug_io_config(repo_root, original_config_raw)


if __name__ == "__main__":
    main()
