#!/usr/bin/env python3
"""
Tool-call reduction benchmark for budi.

Measures how many tool calls Claude needs to answer structural codebase
questions WITH vs WITHOUT budi context pre-injected.

Uses the `claude` CLI (Claude Code) — no ANTHROPIC_API_KEY needed.

Usage:
    python3 scripts/toolcall_benchmark.py --repo ~/_projects/react [--output-dir ./toolcall-bench-out]
    python3 scripts/toolcall_benchmark.py --repo ~/_projects/react --cases 1 2 3  # smoke test
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import textwrap
from dataclasses import dataclass, field, asdict
from pathlib import Path
from typing import Optional

# Env passed to claude subprocesses — must unset CLAUDECODE to allow nested invocation
def _claude_env() -> dict:
    env = os.environ.copy()
    env.pop("CLAUDECODE", None)
    return env

# ---------------------------------------------------------------------------
# Benchmark dataset
# ---------------------------------------------------------------------------

# Each case: structural question + truth_hints (key terms a correct answer must cover)
CASES = [
    {
        "id": 1,
        "prompt": "Where is scheduleUpdateOnFiber defined and what are the first two things it does?",
        "truth_hints": ["ReactFiberWorkLoop", "scheduleUpdateOnFiber"],
        "category": "symbol-definition",
    },
    {
        "id": 2,
        "prompt": "What function does useState call internally to enqueue a state update?",
        "truth_hints": ["dispatchSetState", "queue"],
        "category": "call-graph",
    },
    {
        "id": 3,
        "prompt": "Where is REACT_ELEMENT_TYPE defined?",
        "truth_hints": ["ReactSymbols", "shared/ReactSymbols"],
        "category": "symbol-definition",
    },
    {
        "id": 4,
        "prompt": "What scheduler priority does a click event get assigned in React's event system?",
        "truth_hints": ["DiscreteEventPriority", "ImmediatePriority", "SyncLane"],
        "category": "call-graph",
    },
    {
        "id": 5,
        "prompt": "What does performConcurrentWorkOnRoot return when it can't finish all the work?",
        "truth_hints": ["performConcurrentWorkOnRoot", "bind"],
        "category": "call-graph",
    },
    {
        "id": 6,
        "prompt": "Which file is the entry point for react-dom's client rendering?",
        "truth_hints": ["ReactDOMClient", "createRoot"],
        "category": "architecture",
    },
    {
        "id": 7,
        "prompt": "Where does useEffect cleanup run — which function in which file?",
        "truth_hints": ["commitHookEffectListUnmount", "commitPassiveUnmountEffects", "ReactFiberCommitWork"],
        "category": "flow-trace",
    },
    {
        "id": 8,
        "prompt": "How does React check if it's in concurrent mode vs legacy mode during rendering?",
        "truth_hints": ["ConcurrentMode", "LegacyMode", "mode"],
        "category": "architecture",
    },
]

# ---------------------------------------------------------------------------
# budi context retrieval
# ---------------------------------------------------------------------------

def get_budi_context(repo_root: Path, prompt: str) -> Optional[str]:
    """Returns injected context string from budi, or None if skipped."""
    cmd = ["budi", "repo", "preview", "--json", prompt, "--repo-root", str(repo_root)]
    try:
        result = subprocess.run(cmd, capture_output=True, text=True, timeout=30, env=_claude_env())
        if result.returncode != 0:
            return None
        data = json.loads(result.stdout)
        diag = data.get("diagnostics", {})
        if not diag.get("recommended_injection", False):
            return None
        ctx = data.get("context", "").strip()
        return ctx or None
    except Exception:
        return None

# ---------------------------------------------------------------------------
# Claude CLI invocation
# ---------------------------------------------------------------------------

MAX_TOOL_ROUNDS = 8  # claude handles this internally; we just parse the stream

TASK_PREAMBLE = textwrap.dedent("""\
    You are answering a question about the React source code in this repository.
    Use tools to look up information. Be concise — cite exact file paths and function names.
    Stop tool calls as soon as you have enough information to give a specific answer.
    Do NOT edit any files.
""")


def run_claude(
    prompt: str,
    repo_root: Path,
    budi_context: Optional[str],
    model: str,
) -> tuple[str, int, list[dict]]:
    """
    Run `claude -p` with stream-json output.
    Returns (final_answer, tool_call_count, tool_call_log).
    """
    full_prompt = TASK_PREAMBLE + "\nQuestion: " + prompt

    cmd = [
        "claude",
        "--print",
        "--output-format", "stream-json",
        "--verbose",
        "--no-session-persistence",
        "--dangerously-skip-permissions",
        "--tools", "Read,Glob,Grep",
        "--model", model,
    ]

    if budi_context:
        # Inject budi context as extra system context
        cmd += ["--append-system-prompt", f"\n[Pre-retrieved context from local index]\n{budi_context}"]

    cmd.append(full_prompt)

    try:
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=120,
            cwd=str(repo_root),
            env=_claude_env(),
        )
    except subprocess.TimeoutExpired:
        return "[timeout]", 0, []

    # Parse stream-json — each line is one event
    tool_call_count = 0
    tool_call_log = []
    final_answer_parts = []

    for line in result.stdout.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue

        event_type = event.get("type", "")

        # Count tool_use blocks inside assistant messages
        if event_type == "assistant":
            content = event.get("message", {}).get("content", [])
            for block in content:
                if isinstance(block, dict) and block.get("type") == "tool_use":
                    tool_call_count += 1
                    tool_call_log.append({
                        "name": block.get("name"),
                        "input": block.get("input", {}),
                    })

        # Capture final text result
        if event_type == "result":
            final_answer_parts.append(event.get("result", ""))

    final_answer = "\n".join(final_answer_parts).strip()
    if not final_answer and result.returncode != 0:
        final_answer = f"[error: {result.stderr.strip()[:200]}]"

    return final_answer, tool_call_count, tool_call_log


# ---------------------------------------------------------------------------
# Answer quality judge (also via claude CLI)
# ---------------------------------------------------------------------------

JUDGE_PROMPT_TEMPLATE = textwrap.dedent("""\
    Evaluate whether this answer correctly addresses the question about React source code.

    Question: {prompt}

    Answer: {answer}

    Truth hints — at least one of these terms should appear in a correct answer:
    {hints}

    Rules:
    - "correct" if: answer gives a specific, actionable response AND contains at least one truth hint term (exact or clearly equivalent)
    - "partial" if: answer is on the right track but missing the truth hints or too vague to be useful
    - "wrong" if: answer is incorrect, hallucinates, or completely misses the question

    Do NOT fact-check the answer against your own knowledge. Only evaluate based on specificity and hint presence.

    Respond with exactly this JSON (no other text):
    {{"verdict": "correct"|"partial"|"wrong", "reason": "one sentence"}}
""")


def judge_answer(
    prompt: str,
    answer: str,
    truth_hints: list[str],
    model: str,
    repo_root: Path,
) -> tuple[str, str]:
    if answer.startswith("[error") or answer.startswith("[timeout"):
        return "wrong", answer

    judge_prompt = JUDGE_PROMPT_TEMPLATE.format(
        prompt=prompt,
        answer=answer[:1200],
        hints=", ".join(truth_hints),
    )

    cmd = [
        "claude",
        "--print",
        "--output-format", "text",
        "--no-session-persistence",
        "--dangerously-skip-permissions",
        "--model", model,
        judge_prompt,
    ]

    try:
        result = subprocess.run(
            cmd, capture_output=True, text=True, timeout=60,
            cwd=str(repo_root), env=_claude_env(),
        )
        text = result.stdout.strip()
        # Extract JSON from response (claude may add prose around it)
        start = text.find("{")
        end = text.rfind("}") + 1
        if start >= 0 and end > start:
            parsed = json.loads(text[start:end])
            return str(parsed["verdict"]), str(parsed["reason"])
        return "error", text[:100]
    except Exception as e:
        return "error", str(e)[:100]


# ---------------------------------------------------------------------------
# Data classes
# ---------------------------------------------------------------------------

@dataclass
class ConditionResult:
    tool_call_count: int
    answer: str
    judge_verdict: str      # "correct" | "partial" | "wrong" | "error"
    judge_reason: str
    tool_call_log: list[dict] = field(default_factory=list)
    budi_context_injected: bool = False
    budi_context_chars: int = 0


@dataclass
class CaseResult:
    id: int
    prompt: str
    category: str
    truth_hints: list[str]
    without_budi: Optional[ConditionResult] = None
    with_budi: Optional[ConditionResult] = None
    tool_calls_saved: int = 0
    error: Optional[str] = None


# ---------------------------------------------------------------------------
# Core benchmark loop
# ---------------------------------------------------------------------------

def run_benchmark(repo_root: Path, model: str) -> list[CaseResult]:
    results = []

    for case in CASES:
        cid = case["id"]
        prompt = case["prompt"]
        print(f"\n[{cid}/{len(CASES)}] {prompt[:65]}")

        cr = CaseResult(
            id=cid,
            prompt=prompt,
            category=case["category"],
            truth_hints=case["truth_hints"],
        )

        try:
            # Condition A: WITHOUT budi
            print(f"  without budi ...", end="", flush=True)
            answer_no, count_no, log_no = run_claude(prompt, repo_root, None, model)
            verdict_no, reason_no = judge_answer(prompt, answer_no, case["truth_hints"], model, repo_root)
            cr.without_budi = ConditionResult(
                tool_call_count=count_no,
                answer=answer_no,
                judge_verdict=verdict_no,
                judge_reason=reason_no,
                tool_call_log=log_no,
            )
            print(f" {count_no} tool calls, verdict={verdict_no}")

            # Condition B: WITH budi
            print(f"  with budi    ...", end="", flush=True)
            budi_ctx = get_budi_context(repo_root, prompt)
            answer_yes, count_yes, log_yes = run_claude(prompt, repo_root, budi_ctx, model)
            verdict_yes, reason_yes = judge_answer(prompt, answer_yes, case["truth_hints"], model, repo_root)
            cr.with_budi = ConditionResult(
                tool_call_count=count_yes,
                answer=answer_yes,
                judge_verdict=verdict_yes,
                judge_reason=reason_yes,
                tool_call_log=log_yes,
                budi_context_injected=budi_ctx is not None,
                budi_context_chars=len(budi_ctx) if budi_ctx else 0,
            )
            print(f" {count_yes} tool calls, verdict={verdict_yes}")

            cr.tool_calls_saved = count_no - count_yes

        except Exception as e:
            cr.error = str(e)
            print(f"  ERROR: {e}")

        results.append(cr)

    return results


# ---------------------------------------------------------------------------
# Reports
# ---------------------------------------------------------------------------

def write_json(results: list[CaseResult], out_dir: Path) -> Path:
    out_dir.mkdir(parents=True, exist_ok=True)
    path = out_dir / "toolcall-bench-results.json"
    path.write_text(json.dumps([asdict(r) for r in results], indent=2))
    return path


def write_markdown(results: list[CaseResult], out_dir: Path) -> Path:
    out_dir.mkdir(parents=True, exist_ok=True)
    path = out_dir / "toolcall-bench-report.md"

    valid = [r for r in results if r.without_budi and r.with_budi]
    avg_no  = sum(r.without_budi.tool_call_count for r in valid) / max(len(valid), 1)
    avg_yes = sum(r.with_budi.tool_call_count for r in valid) / max(len(valid), 1)
    correct_no  = sum(1 for r in valid if r.without_budi.judge_verdict == "correct")
    correct_yes = sum(1 for r in valid if r.with_budi.judge_verdict == "correct")
    total_saved = sum(r.tool_calls_saved for r in valid)

    lines = [
        "# Budi Tool-Call Reduction Benchmark",
        "",
        "## Summary",
        "",
        "| Metric | Without budi | With budi | Delta |",
        "|--------|-------------|-----------|-------|",
        f"| Avg tool calls | {avg_no:.1f} | {avg_yes:.1f} | {avg_no - avg_yes:+.1f} |",
        f"| Correct answers | {correct_no}/{len(valid)} | {correct_yes}/{len(valid)} | {correct_yes - correct_no:+d} |",
        f"| Total tool calls saved | — | — | {total_saved} |",
        "",
        "## Per-Case Results",
        "",
        "| # | Category | Without (calls/verdict) | With (calls/verdict) | Saved | budi injected? |",
        "|---|----------|------------------------|---------------------|-------|---------------|",
    ]

    for r in results:
        if r.error:
            lines.append(f"| {r.id} | {r.category} | ERROR | ERROR | — | — |")
            continue
        no_cell  = f"{r.without_budi.tool_call_count} / {r.without_budi.judge_verdict}"
        yes_cell = f"{r.with_budi.tool_call_count} / {r.with_budi.judge_verdict}"
        injected = f"yes ({r.with_budi.budi_context_chars}c)" if r.with_budi.budi_context_injected else "no (skipped)"
        saved    = f"{r.tool_calls_saved:+d}" if r.tool_calls_saved != 0 else "0"
        lines.append(f"| {r.id} | {r.category} | {no_cell} | {yes_cell} | {saved} | {injected} |")

    lines += ["", "## Answers", ""]
    for r in results:
        if r.error:
            lines += [f"### [{r.id}] ERROR", f"`{r.error}`", ""]
            continue
        lines += [
            f"### [{r.id}] {r.prompt}",
            "",
            f"**Without budi** ({r.without_budi.tool_call_count} tool calls, verdict={r.without_budi.judge_verdict})",
            f"> {r.without_budi.judge_reason}",
            "```",
            r.without_budi.answer[:800],
            "```",
            "",
            f"**With budi** ({r.with_budi.tool_call_count} tool calls, verdict={r.with_budi.judge_verdict})",
            f"> {r.with_budi.judge_reason}",
            "```",
            r.with_budi.answer[:800],
            "```",
            "",
        ]

    path.write_text("\n".join(lines) + "\n")
    return path


def print_summary(results: list[CaseResult]) -> None:
    valid = [r for r in results if r.without_budi and r.with_budi]
    if not valid:
        print("\nNo valid results.")
        return

    avg_no  = sum(r.without_budi.tool_call_count for r in valid) / len(valid)
    avg_yes = sum(r.with_budi.tool_call_count for r in valid) / len(valid)
    correct_no  = sum(1 for r in valid if r.without_budi.judge_verdict == "correct")
    correct_yes = sum(1 for r in valid if r.with_budi.judge_verdict == "correct")

    print(f"\n{'='*65}")
    print(f"  Tool calls — without: {avg_no:.1f} avg  |  with: {avg_yes:.1f} avg  |  Δ {avg_no - avg_yes:+.1f}/question")
    print(f"  Quality    — without: {correct_no}/{len(valid)} correct  |  with: {correct_yes}/{len(valid)} correct")
    print(f"{'='*65}\n")

    for r in results:
        if r.error:
            print(f"  [{r.id:2d}] ERROR: {r.error}")
            continue
        arrow = "↓" if r.tool_calls_saved > 0 else ("↑" if r.tool_calls_saved < 0 else "=")
        ctx_info = f"budi: {r.with_budi.budi_context_chars}c" if r.with_budi.budi_context_injected else "budi: skipped"
        print(
            f"  [{r.id:2d}] {r.category:<20}"
            f"  no={r.without_budi.tool_call_count} ({r.without_budi.judge_verdict:<7})"
            f"  yes={r.with_budi.tool_call_count} ({r.with_budi.judge_verdict:<7})"
            f"  {arrow}{abs(r.tool_calls_saved)}"
            f"  [{ctx_info}]"
        )


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

def main() -> int:
    parser = argparse.ArgumentParser(description="Tool-call reduction benchmark (uses claude CLI auth).")
    parser.add_argument("--repo", type=Path, required=True, help="Path to the React repo")
    parser.add_argument("--output-dir", type=Path, default=Path("./toolcall-bench-out"))
    parser.add_argument(
        "--model", default="claude-haiku-4-5-20251001",
        help="Claude model alias or full ID (default: claude-haiku-4-5-20251001)"
    )
    parser.add_argument("--cases", type=int, nargs="*",
                        help="Run only specific case IDs (e.g. --cases 1 2 3)")
    args = parser.parse_args()

    repo_root = args.repo.expanduser().resolve()
    if not repo_root.exists():
        print(f"error: repo not found: {repo_root}", file=sys.stderr)
        return 1

    global CASES
    if args.cases:
        CASES = [c for c in CASES if c["id"] in args.cases]
        if not CASES:
            print(f"error: no matching cases for ids {args.cases}", file=sys.stderr)
            return 1

    print(f"Repo:   {repo_root}")
    print(f"Model:  {args.model}")
    print(f"Cases:  {len(CASES)} (each runs twice — without budi, then with budi)\n")

    results = run_benchmark(repo_root, model=args.model)

    json_path = write_json(results, args.output_dir)
    md_path   = write_markdown(results, args.output_dir)
    print_summary(results)
    print(f"\nOutputs:")
    print(f"  {json_path}")
    print(f"  {md_path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
