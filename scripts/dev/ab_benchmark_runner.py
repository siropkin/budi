#!/usr/bin/env python3
from __future__ import annotations

import argparse
import concurrent.futures
import hashlib
import json
import os
import re
import statistics
import subprocess
import textwrap
import threading
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

_print_lock = threading.Lock()


def _print(*args, **kwargs) -> None:
    with _print_lock:
        print(*args, **kwargs)


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


def _claude_env() -> dict:
    """Return env with CLAUDECODE unset to allow nested claude invocations."""
    env = os.environ.copy()
    env.pop("CLAUDECODE", None)
    return env


def run_cmd(
    args: list[str],
    cwd: Path,
    input_text: str | None = None,
    timeout_sec: int = 300,
    env: dict | None = None,
) -> CmdResult:
    started = time.perf_counter()
    proc = subprocess.run(
        args,
        cwd=str(cwd),
        input=input_text,
        text=True,
        capture_output=True,
        timeout=timeout_sec,
        env=env,
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


def apply_prompt_limit(prompts: list[str], max_prompts: int) -> list[str]:
    if max_prompts <= 0:
        return list(prompts)
    return list(prompts[:max_prompts])


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


def _toml_literal(value: Any) -> str:
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, float):
        return repr(value)
    if isinstance(value, str):
        return json.dumps(value)
    if isinstance(value, list):
        rendered = ", ".join(_toml_literal(item) for item in value)
        return f"[{rendered}]"
    raise TypeError(f"Unsupported override type: {type(value).__name__}")


def _apply_toml_pairs(raw: str, pairs: dict[str, Any]) -> str:
    updated = raw
    for key, value in pairs.items():
        pattern = re.compile(rf"^{re.escape(key)}\s*=.*$", re.MULTILINE)
        rendered = _toml_literal(value)
        if pattern.search(updated):
            updated = pattern.sub(f"{key} = {rendered}", updated)
        else:
            if not updated.endswith("\n"):
                updated += "\n"
            updated += f"{key} = {rendered}\n"
    return updated


def ensure_debug_io_enabled(repo_root: Path, variant_overrides: dict[str, Any] | None = None) -> str:
    cfg_path = resolve_budi_config_path(repo_root)
    if not cfg_path.exists():
        raise RuntimeError(f"Missing config file: {cfg_path}")

    original_raw = cfg_path.read_text()
    raw = _apply_toml_pairs(
        original_raw,
        {
            "debug_io": True,
            "debug_io_full_text": False,
            "debug_io_max_chars": 1500,
        },
    )
    if variant_overrides:
        raw = _apply_toml_pairs(raw, variant_overrides)
    if raw != original_raw:
        cfg_path.write_text(raw)
    # Keep benchmark runs non-disruptive for other terminals. Hooks and daemon
    # read repo config per request, so a forced global daemon restart is not
    # required here.
    return original_raw


def restore_debug_io_config(repo_root: Path, original_raw: str) -> None:
    cfg_path = resolve_budi_config_path(repo_root)
    if not cfg_path.exists():
        return
    current_raw = cfg_path.read_text()
    if current_raw != original_raw:
        cfg_path.write_text(original_raw)


def _load_variant_overrides(
    variant_overrides_file: str,
    variant_overrides_json: str,
) -> dict[str, Any]:
    merged: dict[str, Any] = {}

    if variant_overrides_file:
        path = Path(variant_overrides_file).expanduser().resolve()
        if not path.exists():
            raise SystemExit(f"variant overrides file not found: {path}")
        try:
            parsed = json.loads(path.read_text())
        except json.JSONDecodeError as exc:  # noqa: PERF203
            raise SystemExit(f"invalid variant overrides JSON in {path}: {exc}") from exc
        if not isinstance(parsed, dict):
            raise SystemExit(f"variant overrides file must contain a JSON object: {path}")
        merged.update(parsed)

    if variant_overrides_json:
        try:
            parsed = json.loads(variant_overrides_json)
        except json.JSONDecodeError as exc:
            raise SystemExit(f"invalid --variant-overrides-json payload: {exc}") from exc
        if not isinstance(parsed, dict):
            raise SystemExit("--variant-overrides-json must be a JSON object")
        merged.update(parsed)

    for key in merged:
        if not re.match(r"^[A-Za-z_][A-Za-z0-9_]*$", key):
            raise SystemExit(f"invalid override key: {key!r}")
    return merged


def run_claude_prompt(
    repo_root: Path,
    prompt: str,
    disable_hooks: bool,
    timeout_sec: int,
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
    cmd = run_cmd(args, cwd=repo_root, input_text=prompt, timeout_sec=timeout_sec, env=_claude_env())
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


def collect_hook_trace_for_session(repo_root: Path, session_id: str) -> dict[str, Any]:
    if not session_id:
        return {}
    path = resolve_hook_log_path(repo_root)
    if not path.exists():
        return {}
    input_event = None
    output_event = None  # fallback: any output event
    ok_output_event = None  # preferred: output event where budi actually injected
    for line in reversed(path.read_text().splitlines()):
        try:
            obj = json.loads(line)
        except json.JSONDecodeError:
            continue
        if obj.get("event") != "UserPromptSubmit":
            continue
        if obj.get("session_id") != session_id:
            continue
        phase = obj.get("phase")
        if phase == "output":
            if output_event is None:
                output_event = obj
            # Prefer the event where budi injected (reason=="ok"), since each
            # session may fire the hook multiple times (user prompt + tool calls).
            if obj.get("reason") == "ok" and ok_output_event is None:
                ok_output_event = obj
        elif phase == "input" and input_event is None:
            input_event = obj
        if input_event and output_event and ok_output_event:
            break
    return {"input": input_event, "output": ok_output_event or output_event}


def hook_trace_is_healthy(trace: dict[str, Any]) -> bool:
    output = trace.get("output") if isinstance(trace, dict) else None
    if not isinstance(output, dict):
        return False
    reason = str(output.get("reason", ""))
    if output.get("success") is False:
        return False
    if reason in {"query_timeout", "query_transport_error", "daemon_unavailable", "query_error"}:
        return False
    return True


def truncate_text(text: str, max_chars: int = 4000) -> str:
    if len(text) <= max_chars:
        return text
    return text[:max_chars] + f"\n...[truncated {len(text) - max_chars} chars]"


def judge_pair(
    repo_root: Path,
    prompt: str,
    no_budi_result: str,
    with_budi_result: str,
    timeout_sec: int,
) -> dict[str, Any]:
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
    cmd = run_cmd(args, cwd=repo_root, input_text=judge_prompt, timeout_sec=timeout_sec, env=_claude_env())
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


def judge_pair_majority(
    repo_root: Path,
    prompt: str,
    no_budi_result: str,
    with_budi_result: str,
    timeout_sec: int,
    passes: int = 3,
) -> dict[str, Any]:
    """Run judge_pair `passes` times in parallel, take majority winner, average numeric scores."""
    with concurrent.futures.ThreadPoolExecutor(max_workers=passes) as executor:
        futures = [
            executor.submit(judge_pair, repo_root, prompt, no_budi_result, with_budi_result, timeout_sec)
            for _ in range(passes)
        ]
        results = [f.result() for f in concurrent.futures.as_completed(futures) if f.result().get("ok")]
    if not results:
        return {"ok": False, "error": "all_judge_passes_failed"}

    winner_counts: dict[str, int] = {}
    for r in results:
        w = r.get("winner", "tie")
        winner_counts[w] = winner_counts.get(w, 0) + 1
    majority_winner = max(winner_counts, key=lambda k: (winner_counts[k], k == "with_budi"))

    score_keys = [
        "score_no_budi", "score_with_budi",
        "grounding_no_budi", "grounding_with_budi",
        "actionability_no_budi", "actionability_with_budi",
    ]
    averaged = {k: sum(r.get(k, 0) for r in results) / len(results) for k in score_keys}
    justifications = " | ".join(r.get("justification", "") for r in results if r.get("justification"))

    return {
        "ok": True,
        "winner": majority_winner,
        "judge_passes": len(results),
        "winner_counts": winner_counts,
        "justification": justifications,
        **averaged,
    }


def run_one_prompt(
    repo_root: Path,
    prompt: str,
    idx: int,
    total: int,
    claude_timeout_sec: int,
    disable_with_budi_retry: bool,
) -> dict[str, Any]:
    """Run no_budi and with_budi concurrently for one prompt, return a row dict."""
    _print(f"[ab] prompt {idx}/{total}", flush=True)
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as executor:
        no_budi_future = executor.submit(
            run_claude_prompt, repo_root, prompt, True, claude_timeout_sec
        )
        with_budi_future = executor.submit(
            run_claude_prompt, repo_root, prompt, False, claude_timeout_sec
        )
        no_budi = no_budi_future.result()
        with_budi = with_budi_future.result()

    with_budi_hook_retry = False
    with_budi_hook: dict[str, Any] = {}
    with_budi_session_id = str(with_budi.get("session_id", "")) if isinstance(with_budi, dict) else ""
    if with_budi_session_id:
        with_budi_hook = collect_hook_trace_for_session(repo_root, with_budi_session_id)

    if (
        not disable_with_budi_retry
        and with_budi.get("ok")
        and not hook_trace_is_healthy(with_budi_hook)
    ):
        with_budi_hook_retry = True
        _print(f"[ab] prompt {idx}/{total} retrying with_budi (unhealthy hook trace)", flush=True)
        with_budi_retry = run_claude_prompt(
            repo_root, prompt, False, max(claude_timeout_sec, 480)
        )
        retry_session_id = str(with_budi_retry.get("session_id", ""))
        retry_hook = (
            collect_hook_trace_for_session(repo_root, retry_session_id)
            if retry_session_id
            else {}
        )
        if with_budi_retry.get("ok"):
            with_budi = with_budi_retry
            with_budi_hook = retry_hook

    return {
        "prompt": prompt,
        "no_budi": no_budi,
        "with_budi": with_budi,
        "with_budi_hook": with_budi_hook,
        "with_budi_hook_retry": with_budi_hook_retry,
        "judge": {"ok": False},
    }


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
    variant_id: str,
    variant_overrides: dict[str, Any],
) -> str:
    lines: list[str] = []
    lines.append(f"# A/B Benchmark Report ({repo_root.name})")
    lines.append("")
    lines.append(f"- Generated at: {datetime.now(timezone.utc).isoformat()}")
    if run_label:
        lines.append(f"- Run label: `{run_label}`")
    lines.append(f"- Variant: `{variant_id}`")
    if variant_overrides:
        lines.append(f"- Variant overrides: `{json.dumps(variant_overrides, sort_keys=True)}`")
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
    lines.append(f"| Rows judged | {int(judge_summary.get('judged_rows', 0))} |")
    lines.append(f"| Judge attempts | {int(judge_summary.get('judge_attempted_rows', 0))} |")
    lines.append(f"| Judge skipped | {bool(judge_summary.get('judge_skipped', False))} |")
    lines.append(f"| Judge limit | {int(judge_summary.get('judge_limit', 0))} |")
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
        "| # | Prompt | cost nb/wb | Q nb→wb | G nb→wb | intent | top | ctx | winner |"
    )
    lines.append("|---:|---|---:|---:|---:|---|---:|---:|---|")
    for i, row in enumerate(rows, start=1):
        prompt_short = row["prompt"][:100].replace("\n", " ")
        a = row["no_budi"]
        b = row["with_budi"]
        judge = row.get("judge", {})
        hook_out = (row.get("with_budi_hook") or {}).get("output") or {}
        intent_raw = str(hook_out.get("retrieval_intent", ""))
        # Abbreviate intent for table compactness
        intent_abbrev = {
            "flow-trace": "flow",
            "symbol-definition": "sym-def",
            "symbol-usage": "sym-use",
            "architecture": "arch",
            "test-lookup": "test",
            "runtime-config": "rt-cfg",
            "path-lookup": "path",
            "non-code": "non-code",
        }.get(intent_raw, intent_raw or "—")
        top_score = hook_out.get("retrieval_top_score")
        ctx_chars = hook_out.get("context_chars")
        top_str = f"{top_score:.2f}" if isinstance(top_score, (int, float)) else "—"
        ctx_str = str(ctx_chars) if isinstance(ctx_chars, int) else "—"
        q_nb = safe_num(judge.get("score_no_budi"))
        q_wb = safe_num(judge.get("score_with_budi"))
        g_nb = safe_num(judge.get("grounding_no_budi"))
        g_wb = safe_num(judge.get("grounding_with_budi"))
        q_str = f"{q_nb:.0f}→{q_wb:.0f}" if judge.get("ok") else "—"
        g_str = f"{g_nb:.0f}→{g_wb:.0f}" if judge.get("ok") else "—"
        cost_nb = safe_num(a.get("total_cost_usd"))
        cost_wb = safe_num(b.get("total_cost_usd"))
        lines.append(
            f"| {i} | {prompt_short} | {cost_nb:.4f}/{cost_wb:.4f} | "
            f"{q_str} | {g_str} | {intent_abbrev} | {top_str} | {ctx_str} | "
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
    parser.add_argument(
        "--variant-id",
        default="v1_current",
        help="Variant identifier stored in results metadata",
    )
    parser.add_argument(
        "--variant-overrides-file",
        default="",
        help="Optional JSON file with budi config key/value overrides for this run",
    )
    parser.add_argument(
        "--variant-overrides-json",
        default="",
        help="Optional inline JSON object with budi config overrides for this run",
    )
    parser.add_argument(
        "--max-prompts",
        type=int,
        default=0,
        help="Run only the first N prompts after prompt resolution (0 = all)",
    )
    parser.add_argument(
        "--skip-judge",
        action="store_true",
        help="Skip LLM judge pass (faster, but no quality/grounding metrics)",
    )
    parser.add_argument(
        "--judge-limit",
        type=int,
        default=0,
        help="Judge only first N eligible rows (0 = all)",
    )
    parser.add_argument(
        "--claude-timeout-sec",
        type=int,
        default=420,
        help="Timeout for each no_budi/with_budi Claude run",
    )
    parser.add_argument(
        "--judge-timeout-sec",
        type=int,
        default=300,
        help="Timeout for each judge Claude run",
    )
    parser.add_argument(
        "--judge-passes",
        type=int,
        default=1,
        help="Number of judge passes per prompt (majority vote, default 1). Use 3 to reduce variance.",
    )
    parser.add_argument(
        "--parallel",
        type=int,
        default=1,
        help="Number of prompts to evaluate concurrently (default: 1). Each prompt already runs "
             "no_budi and with_budi in parallel, so --parallel 3 runs 6 Claude calls at once.",
    )
    parser.add_argument(
        "--disable-with-budi-retry",
        action="store_true",
        help="Disable second with_budi attempt when hook trace looks unhealthy",
    )
    args = parser.parse_args()

    repo_root = Path(args.repo_root).expanduser().resolve()
    if not repo_root.exists():
        raise SystemExit(f"Repo does not exist: {repo_root}")
    if args.max_prompts < 0:
        raise SystemExit("--max-prompts must be >= 0")
    if args.judge_limit < 0:
        raise SystemExit("--judge-limit must be >= 0")
    if args.claude_timeout_sec < 30:
        raise SystemExit("--claude-timeout-sec must be >= 30")
    if args.judge_timeout_sec < 30:
        raise SystemExit("--judge-timeout-sec must be >= 30")

    prompts: list[str] = []
    prompt_source = ""
    if not args.reuse_results_json:
        prompts, prompt_source = resolve_prompts(
            prompts_file=args.prompts_file,
            inline_prompts=args.prompt,
            use_default_prompts=not args.no_default_prompts,
        )
        prompts = apply_prompt_limit(prompts, args.max_prompts)

    ts = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    out_dir = Path(args.out_dir).expanduser().resolve() if args.out_dir else (
        budi_repo_paths(repo_root)["benchmarks"] / ts
    )
    out_dir.mkdir(parents=True, exist_ok=True)

    rows: list[dict[str, Any]] = []
    source_results_json = ""
    variant_overrides = _load_variant_overrides(
        args.variant_overrides_file,
        args.variant_overrides_json,
    )
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
            prompts = apply_prompt_limit(prompts, args.max_prompts)
            if args.max_prompts > 0:
                rows = rows[: len(prompts)]
            prompt_source = f"reuse:{source_path}"
            _print(f"[ab] reusing rows from: {source_path}", flush=True)
        else:
            ensure_budi_ready(repo_root)
            original_config_raw = ensure_debug_io_enabled(repo_root, variant_overrides)

            parallel = max(1, args.parallel)
            if parallel == 1:
                for idx, prompt in enumerate(prompts, start=1):
                    rows.append(run_one_prompt(
                        repo_root, prompt, idx, len(prompts),
                        args.claude_timeout_sec, args.disable_with_budi_retry,
                    ))
            else:
                _print(f"[ab] running {len(prompts)} prompts with --parallel {parallel}", flush=True)
                # Submit all prompts; preserve original order in rows
                ordered: dict[int, dict[str, Any]] = {}
                with concurrent.futures.ThreadPoolExecutor(max_workers=parallel) as executor:
                    future_to_idx = {
                        executor.submit(
                            run_one_prompt,
                            repo_root, prompt, idx, len(prompts),
                            args.claude_timeout_sec, args.disable_with_budi_retry,
                        ): idx
                        for idx, prompt in enumerate(prompts, start=1)
                    }
                    for future in concurrent.futures.as_completed(future_to_idx):
                        idx = future_to_idx[future]
                        ordered[idx] = future.result()
                rows = [ordered[i] for i in range(1, len(prompts) + 1)]

        prompt_set = {
            "name": DEFAULT_PROMPT_SET_NAME if prompt_source.startswith("default:") else "custom",
            "source": prompt_source or "unknown",
            "count": len(prompts),
            "fingerprint_sha256": prompt_set_fingerprint(prompts),
        }
        _print(
            f"[ab] prompts={prompt_set['count']} source={prompt_set['source']} "
            f"sha256={prompt_set['fingerprint_sha256'][:12]}",
            flush=True,
        )

        judge_budget = args.judge_limit if args.judge_limit > 0 else None
        judge_passes = getattr(args, "judge_passes", 1)

        # Determine which rows are eligible for judging
        eligible_indices = []
        for idx, row in enumerate(rows):
            no_budi = row.get("no_budi", {})
            with_budi = row.get("with_budi", {})
            if (
                not args.skip_judge
                and isinstance(no_budi, dict)
                and isinstance(with_budi, dict)
                and no_budi.get("ok")
                and with_budi.get("ok")
            ):
                eligible_indices.append(idx)
        if judge_budget is not None:
            eligible_indices = eligible_indices[:judge_budget]
        judge_attempts = len(eligible_indices)

        def _run_judge(idx: int) -> dict[str, Any]:
            row = rows[idx]
            prompt = str(row.get("prompt", ""))
            no_budi = row.get("no_budi", {})
            with_budi = row.get("with_budi", {})
            _print(f"[ab] judging {idx + 1}/{len(rows)}", flush=True)
            if judge_passes > 1:
                return judge_pair_majority(
                    repo_root,
                    prompt,
                    str(no_budi.get("result", "")),
                    str(with_budi.get("result", "")),
                    timeout_sec=args.judge_timeout_sec,
                    passes=judge_passes,
                )
            return judge_pair(
                repo_root,
                prompt,
                str(no_budi.get("result", "")),
                str(with_budi.get("result", "")),
                timeout_sec=args.judge_timeout_sec,
            )

        # Default non-eligible judge values
        for row in rows:
            if args.skip_judge:
                row["judge"] = {"ok": False, "error": "judge_skipped"}
            else:
                row["judge"] = {"ok": False, "error": "judge_not_selected"}

        if eligible_indices:
            judge_parallel = max(1, args.parallel)
            with concurrent.futures.ThreadPoolExecutor(max_workers=judge_parallel) as executor:
                future_to_idx = {executor.submit(_run_judge, idx): idx for idx in eligible_indices}
                for future in concurrent.futures.as_completed(future_to_idx):
                    idx = future_to_idx[future]
                    rows[idx]["judge"] = future.result()

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
            "judged_rows": len(judges),
            "judge_attempted_rows": judge_attempts,
            "judge_skipped": bool(args.skip_judge),
            "judge_limit": args.judge_limit,
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
            "variant": {
                "id": args.variant_id,
                "overrides": variant_overrides,
            },
            "run_options": {
                "max_prompts": args.max_prompts,
                "skip_judge": bool(args.skip_judge),
                "judge_limit": args.judge_limit,
                "claude_timeout_sec": args.claude_timeout_sec,
                "judge_timeout_sec": args.judge_timeout_sec,
                "disable_with_budi_retry": bool(args.disable_with_budi_retry),
            },
            "prompt_set": prompt_set,
            "prompts": prompts,
            "rows": rows,
            "summary": summary,
            "judge_summary": judge_summary,
            "hook_log_example": hook_example,
        }
        json_path = out_dir / "ab-results.json"
        json_path.write_text(json.dumps(results, indent=2))

        md = build_markdown(
            repo_root,
            prompts,
            rows,
            summary,
            judge_summary,
            prompt_set,
            args.run_label,
            args.variant_id,
            variant_overrides,
        )
        md_path = out_dir / "ab-results.md"
        md_path.write_text(md)

        _print(f"[ab] results json: {json_path}")
        _print(f"[ab] results md:   {md_path}")
    finally:
        if original_config_raw is not None:
            restore_debug_io_config(repo_root, original_config_raw)


if __name__ == "__main__":
    main()
