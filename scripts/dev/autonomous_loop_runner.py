#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


AB_RESULTS_JSON_RE = re.compile(r"^\[ab\] results json:\s+(?P<path>.+?)\s*$", re.MULTILINE)


CLAUDE_STATUS_SCHEMA: dict[str, Any] = {
    "type": "object",
    "properties": {
        "status": {
            "type": "string",
            "enum": ["continue", "wait", "blocked", "finished"],
        },
        "summary": {"type": "string"},
        "next_focus": {"type": "string"},
        "wait_reason": {"type": "string"},
        "wait_pid": {"type": ["integer", "null"]},
        "wait_log_path": {"type": ["string", "null"]},
        "benchmark_results_json": {"type": ["string", "null"]},
        "commit_sha": {"type": ["string", "null"]},
        "version_bumped_to": {"type": ["string", "null"]},
    },
    "required": ["status", "summary", "next_focus"],
    "additionalProperties": True,
}


@dataclass
class ClaudeCycleResult:
    payload: dict[str, Any]
    session_id: str
    raw: dict[str, Any]


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat()


def append_log(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as handle:
        handle.write(text)
        if not text.endswith("\n"):
            handle.write("\n")


def process_exists(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


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
    raise ValueError(f"Unable to parse JSON output:\n{raw[:2000]}")


def default_claude_env(workspace_root: Path) -> dict[str, str]:
    env = os.environ.copy()
    env.pop("CLAUDECODE", None)
    dev_bin = workspace_root / "target" / "debug"
    if dev_bin.exists():
        current_path = env.get("PATH", "")
        env["PATH"] = (
            f"{dev_bin}{os.pathsep}{current_path}" if current_path else str(dev_bin)
        )
    dev_daemon = dev_bin / "budi-daemon"
    if dev_daemon.exists():
        env["BUDI_DAEMON_BIN"] = str(dev_daemon)
    return env


def summarize_ab_results(results_json: Path) -> str:
    payload = json.loads(results_json.read_text(encoding="utf-8"))
    judge = payload.get("judge_summary", {})
    run_label = payload.get("run_label", results_json.parent.name)
    return (
        f"### {run_label}\n\n"
        f"- Results: `{results_json}`\n"
        f"- With-budi wins: `{judge.get('with_budi_wins', 0)}`\n"
        f"- No-budi wins: `{judge.get('no_budi_wins', 0)}`\n"
        f"- Ties: `{judge.get('ties', 0)}`\n"
        f"- Avg quality: `{judge.get('avg_score_no_budi', 0)} -> {judge.get('avg_score_with_budi', 0)}`\n"
        f"- Avg grounding: `{judge.get('avg_grounding_no_budi', 0)} -> {judge.get('avg_grounding_with_budi', 0)}`\n"
    )


def extract_results_json_from_log(log_path: Path) -> str | None:
    if not log_path.exists():
        return None
    try:
        text = log_path.read_text(encoding="utf-8")
    except OSError:
        return None
    matches = AB_RESULTS_JSON_RE.findall(text)
    if not matches:
        return None
    return matches[-1].strip()


def build_agent_prompt(
    workspace_root: Path,
    plan_file: Path,
    overnight_summary: Path,
    cycle_index: int,
    extra_prompt: str,
) -> str:
    return f"""
You are running an autonomous improvement loop for the repository at `{workspace_root}`.

Primary plan file: `{plan_file}`
Rolling overnight summary file: `{overnight_summary}`

Goal:
- Improve budi as a "context buster for Claude Code": retrieve and inject the right local code before Claude searches.
- Think beyond the current implementation. Research, retrieval changes, condenser/context-pack ideas, tooling speedups, benchmark improvements, and version bumps are all allowed.

Autonomy rules:
- Do not wait for user approval.
- Do real coding work when there is a clear next step.
- Run tests and focused validations.
- Commit and push successful validated changes as part of the loop.
- Update the improvement plan file with meaningful findings/results.
- If a version bump is warranted by shipped work, do it.

Cycle instructions:
1. Read the plan file and current repo state.
2. Choose the highest-leverage next step.
3. Research/inspect as needed.
4. Implement if appropriate.
5. Validate.
6. Commit and push if the change is validated.
7. Return structured status JSON.

Long-running benchmark rule:
- If you decide to run a long A/B benchmark or other long validation, start it in the background yourself.
- Write its combined stdout/stderr to a deterministic log file under `~/.local/share/budi/overnight/logs/`.
- Return:
  - `"status": "wait"`
  - `"wait_pid": <pid>`
  - `"wait_log_path": "<absolute log path>"`
  - `"wait_reason": "<what is running>"`
- Do not sit and wait inside Claude for the benchmark to finish; the supervisor will poll every 5 minutes and re-run you afterwards.

If you do not need to wait on a background process:
- Return `"status": "continue"` after your loop pass completes so the supervisor can immediately start the next pass.

If you hit a real blocker:
- Return `"status": "blocked"` with a concise reason in `summary`.

If you believe the overnight loop should stop cleanly:
- Return `"status": "finished"`.

Return fields:
- `status`
- `summary`
- `next_focus`
- optionally `wait_reason`, `wait_pid`, `wait_log_path`, `benchmark_results_json`, `commit_sha`, `version_bumped_to`

Current cycle number: {cycle_index}

{extra_prompt}
""".strip()


def run_claude_cycle(
    workspace_root: Path,
    plan_file: Path,
    overnight_summary: Path,
    cycle_index: int,
    extra_prompt: str,
    timeout_sec: int,
) -> ClaudeCycleResult:
    prompt = build_agent_prompt(
        workspace_root=workspace_root,
        plan_file=plan_file,
        overnight_summary=overnight_summary,
        cycle_index=cycle_index,
        extra_prompt=extra_prompt,
    )
    settings = json.dumps({"disableAllHooks": True})
    args = [
        "claude",
        "-p",
        "--output-format",
        "json",
        "--permission-mode",
        "bypassPermissions",
        "--dangerously-skip-permissions",
        "--settings",
        settings,
        "--json-schema",
        json.dumps(CLAUDE_STATUS_SCHEMA),
        "--add-dir",
        str(plan_file.parent),
        "--add-dir",
        str(Path("/Users/ivan.seredkin/_projects/public-budi-bench")),
    ]
    proc = subprocess.run(
        args,
        cwd=str(workspace_root),
        input=prompt,
        text=True,
        capture_output=True,
        timeout=timeout_sec,
        env=default_claude_env(workspace_root),
    )
    if proc.returncode != 0:
        raise RuntimeError(
            f"claude exited with {proc.returncode}\nstdout:\n{proc.stdout[-4000:]}\nstderr:\n{proc.stderr[-4000:]}"
        )
    raw = parse_json_output(proc.stdout)
    structured = raw.get("structured_output")
    if not isinstance(structured, dict):
        raise RuntimeError(f"Missing structured_output in claude response: {raw}")
    session_id = str(raw.get("session_id", ""))
    return ClaudeCycleResult(payload=structured, session_id=session_id, raw=raw)


def wait_for_pid(
    pid: int,
    poll_seconds: int,
    summary_file: Path,
    wait_reason: str,
    wait_log_path: str,
) -> str | None:
    append_log(
        summary_file,
        "## Waiting On Background Work\n\n"
        f"- Started waiting at: `{utc_now()}`\n"
        f"- PID: `{pid}`\n"
        f"- Reason: `{wait_reason}`\n"
        f"- Log path: `{wait_log_path}`\n",
    )
    while process_exists(pid):
        append_log(
            summary_file,
            f"- Still running at: `{utc_now()}`\n",
        )
        time.sleep(poll_seconds)
    append_log(
        summary_file,
        f"- Process finished at: `{utc_now()}`\n",
    )
    if wait_log_path:
        return extract_results_json_from_log(Path(wait_log_path))
    return None


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Run the budi improvement loop autonomously and keep going after AB tests finish.",
    )
    parser.add_argument(
        "--plan-file",
        default="/Users/ivan.seredkin/.claude/plans/budi-improvement-loop.md",
        help="Improvement loop plan file.",
    )
    parser.add_argument(
        "--summary-file",
        required=True,
        help="Markdown log file for overnight autonomous loop progress.",
    )
    parser.add_argument(
        "--wait-pid",
        type=int,
        default=0,
        help="Optional existing process to wait for before starting Claude cycles.",
    )
    parser.add_argument(
        "--wait-log-path",
        default="",
        help="Optional log file for the existing process so AB results can be summarized when it finishes.",
    )
    parser.add_argument(
        "--poll-seconds",
        type=int,
        default=300,
        help="Polling interval while waiting on background work.",
    )
    parser.add_argument(
        "--cycle-timeout-seconds",
        type=int,
        default=3600,
        help="Timeout for each Claude loop pass.",
    )
    parser.add_argument(
        "--max-cycles",
        type=int,
        default=0,
        help="Maximum Claude cycles to run (0 = no explicit limit).",
    )
    parser.add_argument(
        "--extra-prompt",
        default="",
        help="Optional extra instruction appended to every Claude cycle.",
    )
    return parser


def main() -> int:
    args = build_parser().parse_args()
    workspace_root = Path(__file__).resolve().parents[2]
    plan_file = Path(args.plan_file).expanduser().resolve()
    summary_file = Path(args.summary_file).expanduser().resolve()

    append_log(
        summary_file,
        f"# Autonomous Loop Runner\n\n- Started at: `{utc_now()}`\n- Workspace: `{workspace_root}`\n- Plan: `{plan_file}`\n",
    )

    if args.wait_pid > 0:
        results_json = wait_for_pid(
            pid=args.wait_pid,
            poll_seconds=max(1, args.poll_seconds),
            summary_file=summary_file,
            wait_reason="existing process before autonomous loop",
            wait_log_path=args.wait_log_path,
        )
        if results_json:
            append_log(summary_file, summarize_ab_results(Path(results_json)))

    cycle = 0
    while args.max_cycles <= 0 or cycle < args.max_cycles:
        cycle += 1
        append_log(summary_file, f"## Claude Cycle {cycle}\n\n- Started at: `{utc_now()}`\n")
        try:
            result = run_claude_cycle(
                workspace_root=workspace_root,
                plan_file=plan_file,
                overnight_summary=summary_file,
                cycle_index=cycle,
                extra_prompt=args.extra_prompt,
                timeout_sec=args.cycle_timeout_seconds,
            )
        except Exception as exc:
            append_log(
                summary_file,
                f"- Cycle failed at: `{utc_now()}`\n- Error: `{exc}`\n",
            )
            return 1

        payload = result.payload
        status = str(payload.get("status", "")).strip() or "blocked"
        summary = str(payload.get("summary", "")).strip()
        next_focus = str(payload.get("next_focus", "")).strip()
        append_log(
            summary_file,
            f"- Finished at: `{utc_now()}`\n"
            f"- Status: `{status}`\n"
            f"- Summary: {summary}\n"
            f"- Next focus: {next_focus}\n"
            + (
                f"- Commit: `{payload.get('commit_sha')}`\n"
                if payload.get("commit_sha")
                else ""
            )
            + (
                f"- Version bumped to: `{payload.get('version_bumped_to')}`\n"
                if payload.get("version_bumped_to")
                else ""
            ),
        )

        if status == "wait":
            wait_pid = payload.get("wait_pid")
            if not isinstance(wait_pid, int) or wait_pid <= 0:
                append_log(summary_file, "- Invalid wait_pid returned; stopping.\n")
                return 1
            wait_log_path = str(payload.get("wait_log_path") or "")
            wait_reason = str(payload.get("wait_reason") or "background work")
            results_json = wait_for_pid(
                pid=wait_pid,
                poll_seconds=max(1, args.poll_seconds),
                summary_file=summary_file,
                wait_reason=wait_reason,
                wait_log_path=wait_log_path,
            )
            if not results_json:
                hinted = payload.get("benchmark_results_json")
                if isinstance(hinted, str) and hinted:
                    results_json = hinted
            if results_json:
                append_log(summary_file, summarize_ab_results(Path(results_json)))
            continue

        if status == "continue":
            time.sleep(2)
            continue

        if status in {"blocked", "finished"}:
            append_log(summary_file, f"## Loop Exit\n\n- Ended at: `{utc_now()}`\n")
            return 0

        append_log(summary_file, f"- Unknown status `{status}`; stopping.\n")
        return 1

    append_log(summary_file, f"## Loop Exit\n\n- Reached max cycles at: `{utc_now()}`\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
