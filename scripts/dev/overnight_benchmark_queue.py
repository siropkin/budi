#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import time
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Iterable


RESULTS_JSON_RE = re.compile(r"^\[ab\] results json:\s+(?P<path>.+?)\s*$", re.MULTILINE)


@dataclass
class BenchmarkJob:
    repo_root: str
    prompts_file: str
    run_label: str
    parallel: int = 1
    judge_timeout_sec: int = 300
    extra_args: list[str] = field(default_factory=list)


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat()


def append_summary(summary_file: Path, message: str) -> None:
    summary_file.parent.mkdir(parents=True, exist_ok=True)
    with summary_file.open("a", encoding="utf-8") as handle:
        handle.write(message)
        if not message.endswith("\n"):
            handle.write("\n")


def process_exists(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def extract_results_json_from_text(text: str) -> str | None:
    matches = RESULTS_JSON_RE.findall(text)
    if not matches:
        return None
    return matches[-1].strip()


def extract_results_json_from_terminal(terminal_file: Path) -> str | None:
    if not terminal_file.exists():
        return None
    try:
        return extract_results_json_from_text(terminal_file.read_text(encoding="utf-8"))
    except OSError:
        return None


def summarize_results_json(results_json: Path) -> str:
    payload = json.loads(results_json.read_text(encoding="utf-8"))
    run_label = payload.get("run_label", "")
    prompt_set = payload.get("prompt_set", {})
    judge_summary = payload.get("judge_summary", {})
    rows = payload.get("rows", [])
    lines = [
        f"### {run_label or results_json.parent.name}",
        "",
        f"- Results: `{results_json}`",
        f"- Prompt count: `{prompt_set.get('count', len(rows))}`",
        f"- With-budi wins: `{judge_summary.get('with_budi_wins', 0)}`",
        f"- No-budi wins: `{judge_summary.get('no_budi_wins', 0)}`",
        f"- Ties: `{judge_summary.get('ties', 0)}`",
        f"- Avg quality: `{judge_summary.get('avg_score_no_budi', 0)} -> {judge_summary.get('avg_score_with_budi', 0)}`",
        f"- Avg grounding: `{judge_summary.get('avg_grounding_no_budi', 0)} -> {judge_summary.get('avg_grounding_with_budi', 0)}`",
        "",
    ]
    return "\n".join(lines)


def wait_for_existing_run(
    pid: int,
    poll_sec: int,
    summary_file: Path,
    terminal_file: Path | None,
) -> None:
    append_summary(
        summary_file,
        f"## Waiting Existing Run\n\n- Started waiting at: `{utc_now()}`\n- PID: `{pid}`\n"
        + (f"- Terminal file: `{terminal_file}`\n" if terminal_file else ""),
    )
    while process_exists(pid):
        time.sleep(poll_sec)
    append_summary(
        summary_file,
        f"- Existing run finished at: `{utc_now()}`\n",
    )
    if terminal_file is not None:
        results_json = wait_for_terminal_results_json(terminal_file, poll_sec=poll_sec, max_wait_sec=300)
        if results_json is not None:
            append_summary(
                summary_file,
                summarize_results_json(Path(results_json)),
            )


def wait_for_terminal_results_json(
    terminal_file: Path,
    poll_sec: int,
    max_wait_sec: int,
) -> str | None:
    deadline = time.time() + max_wait_sec
    while time.time() < deadline:
        results_json = extract_results_json_from_terminal(terminal_file)
        if results_json is not None:
            return results_json
        time.sleep(poll_sec)
    return None


def parse_job(raw: str) -> BenchmarkJob:
    parts = raw.split("::")
    if len(parts) < 3:
        raise argparse.ArgumentTypeError(
            "--job must be 'repo_root::prompts_file::run_label[::parallel][::judge_timeout_sec][::extra args json]'"
        )
    repo_root, prompts_file, run_label = parts[:3]
    parallel = int(parts[3]) if len(parts) >= 4 and parts[3] else 1
    judge_timeout_sec = int(parts[4]) if len(parts) >= 5 and parts[4] else 300
    extra_args: list[str] = []
    if len(parts) >= 6 and parts[5]:
        parsed = json.loads(parts[5])
        if not isinstance(parsed, list) or not all(isinstance(item, str) for item in parsed):
            raise argparse.ArgumentTypeError("job extra args json must be an array of strings")
        extra_args = parsed
    return BenchmarkJob(
        repo_root=repo_root,
        prompts_file=prompts_file,
        run_label=run_label,
        parallel=parallel,
        judge_timeout_sec=judge_timeout_sec,
        extra_args=extra_args,
    )


def iter_command_output(proc: subprocess.Popen[str]) -> Iterable[str]:
    assert proc.stdout is not None
    for line in proc.stdout:
        yield line


def run_job(job: BenchmarkJob, workspace_root: Path, summary_file: Path, dry_run: bool) -> int:
    runner = workspace_root / "scripts" / "dev" / "ab_benchmark_runner.py"
    command = [
        sys.executable,
        str(runner),
        "--repo-root",
        job.repo_root,
        "--prompts-file",
        job.prompts_file,
        "--run-label",
        job.run_label,
        "--parallel",
        str(job.parallel),
        "--judge-timeout-sec",
        str(job.judge_timeout_sec),
        *job.extra_args,
    ]
    append_summary(
        summary_file,
        "## Starting Job\n\n"
        f"- Started at: `{utc_now()}`\n"
        f"- Repo: `{job.repo_root}`\n"
        f"- Prompts: `{job.prompts_file}`\n"
        f"- Run label: `{job.run_label}`\n"
        f"- Parallel: `{job.parallel}`\n"
        f"- Judge timeout: `{job.judge_timeout_sec}`\n"
        f"- Command: `{' '.join(command)}`\n",
    )
    if dry_run:
        print("[overnight] dry-run:", " ".join(command), flush=True)
        return 0

    proc = subprocess.Popen(
        command,
        cwd=str(workspace_root),
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        bufsize=1,
    )
    results_json: str | None = None
    for line in iter_command_output(proc):
        print(line, end="", flush=True)
        maybe_results = extract_results_json_from_text(line)
        if maybe_results is not None:
            results_json = maybe_results
    return_code = proc.wait()
    append_summary(
        summary_file,
        f"- Finished at: `{utc_now()}`\n- Exit code: `{return_code}`\n",
    )
    if results_json is not None:
        append_summary(summary_file, summarize_results_json(Path(results_json)))
    return return_code


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Queue benchmark runs overnight and keep moving after each run finishes.",
    )
    parser.add_argument(
        "--summary-file",
        required=True,
        help="Markdown file to append job progress and result summaries to.",
    )
    parser.add_argument(
        "--wait-pid",
        type=int,
        default=0,
        help="Optional existing benchmark PID to wait for before starting queued jobs.",
    )
    parser.add_argument(
        "--wait-terminal-file",
        default="",
        help="Optional terminal transcript for the existing run so its results json can be captured.",
    )
    parser.add_argument(
        "--poll-sec",
        type=int,
        default=30,
        help="Polling interval while waiting for an existing PID.",
    )
    parser.add_argument(
        "--job",
        action="append",
        default=[],
        help=(
            "Queued benchmark job as "
            "'repo_root::prompts_file::run_label[::parallel][::judge_timeout_sec][::extra args json]'"
        ),
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Print the queued commands without running them.",
    )
    return parser


def main() -> int:
    parser = build_parser()
    args = parser.parse_args()
    jobs = [parse_job(raw) for raw in args.job]
    if not jobs and args.wait_pid <= 0:
        parser.error("Provide at least one --job or a --wait-pid to monitor.")

    workspace_root = Path(__file__).resolve().parents[2]
    summary_file = Path(args.summary_file).expanduser().resolve()
    append_summary(
        summary_file,
        f"# Overnight Benchmark Queue\n\n- Started at: `{utc_now()}`\n- Workspace: `{workspace_root}`\n",
    )

    terminal_file = (
        Path(args.wait_terminal_file).expanduser().resolve()
        if args.wait_terminal_file
        else None
    )
    if args.wait_pid > 0:
        wait_for_existing_run(args.wait_pid, max(1, args.poll_sec), summary_file, terminal_file)

    worst_exit_code = 0
    for job in jobs:
        exit_code = run_job(job, workspace_root, summary_file, args.dry_run)
        if exit_code != 0:
            worst_exit_code = exit_code

    append_summary(
        summary_file,
        f"## Queue Finished\n\n- Finished at: `{utc_now()}`\n- Exit code: `{worst_exit_code}`\n",
    )
    return worst_exit_code


if __name__ == "__main__":
    raise SystemExit(main())
