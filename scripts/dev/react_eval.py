#!/usr/bin/env python3
"""
React retrieval eval for budi.

Usage:
    python3 scripts/react_eval.py --repo ~/_projects/react [--judge] [--output-dir ./react-eval-out]

--judge requires OPENAI_API_KEY and calls gpt-4o-mini to score snippet relevance.
"""

import argparse
import json
import os
import subprocess
import sys
import textwrap
from dataclasses import dataclass, field, asdict
from pathlib import Path
from typing import Optional

# ---------------------------------------------------------------------------
# Eval dataset
# ---------------------------------------------------------------------------

PROMPTS = [
    {"id": 1,  "prompt": "where is useState defined and what does it return?",                     "intent": "symbol-lookup",  "expected_inject": True},
    {"id": 2,  "prompt": "how does the reconciler decide which fiber nodes to update?",            "intent": "flow-trace",     "expected_inject": True},
    {"id": 3,  "prompt": "what are the different scheduler priority levels?",                      "intent": "symbol-lookup",  "expected_inject": True},
    {"id": 4,  "prompt": "how does React read environment variables or feature flags?",            "intent": "runtime-config", "expected_inject": True},
    {"id": 5,  "prompt": "what is the entry point for server-side rendering?",                     "intent": "architecture",   "expected_inject": True},
    {"id": 6,  "prompt": "where does useEffect cleanup run in the commit phase?",                  "intent": "flow-trace",     "expected_inject": True},
    {"id": 7,  "prompt": "what files are in the react-dom package?",                               "intent": "architecture",   "expected_inject": True},
    {"id": 8,  "prompt": "how does React handle error boundaries?",                                "intent": "symbol-lookup",  "expected_inject": True},
    {"id": 9,  "prompt": "write me a poem about React hooks",                                      "intent": "non-code",       "expected_inject": False},
    {"id": 10, "prompt": "what's 2 + 2?",                                                          "intent": "non-code",       "expected_inject": False},
    {"id": 11, "prompt": "how do I make pasta carbonara?",                                         "intent": "non-code",       "expected_inject": False},
    {"id": 12, "prompt": "how does the event delegation system work in react-dom?",                "intent": "flow-trace",     "expected_inject": True},
]

# ---------------------------------------------------------------------------
# Data classes
# ---------------------------------------------------------------------------

@dataclass
class SnippetResult:
    path: str
    start_line: int
    end_line: int
    score: float
    context_note: Optional[str] = None

@dataclass
class EvalResult:
    id: int
    prompt: str
    intent: str
    expected_inject: bool
    actual_inject: bool
    budi_intent: str
    budi_confidence: float
    budi_skip_reason: Optional[str]
    snippets: list = field(default_factory=list)
    judge_score: Optional[int] = None
    judge_verdict: Optional[str] = None
    passed: bool = False
    error: Optional[str] = None

# ---------------------------------------------------------------------------
# budi invocation
# ---------------------------------------------------------------------------

def run_budi_preview(repo_root: Path, prompt: str) -> dict:
    cmd = ["budi", "repo", "preview", "--json", prompt, "--repo-root", str(repo_root)]
    result = subprocess.run(cmd, capture_output=True, text=True, timeout=120)
    if result.returncode != 0:
        raise RuntimeError(f"budi exited {result.returncode}: {result.stderr.strip()}")
    return json.loads(result.stdout)

# ---------------------------------------------------------------------------
# Claude judge
# ---------------------------------------------------------------------------

JUDGE_SYSTEM = "You are a code retrieval evaluator. Respond only with valid JSON."

JUDGE_TEMPLATE = """\
Question: {prompt}

Retrieved snippets:
{snippets_text}

Score 0-10: how directly relevant are these snippets for answering the question?
0-3: wrong or irrelevant  4-6: partial  7-10: directly answers with specific code

Respond with exactly: {{"score": N, "verdict": "one sentence"}}"""

def judge_snippets(prompt: str, snippets: list[SnippetResult], api_key: str) -> tuple[int, str]:
    from openai import OpenAI  # lazy import — only needed with --judge

    snippets_text = "\n\n".join(
        f"[{i+1}] {s.path}:{s.start_line}-{s.end_line}\n"
        + (f"  SLM note: {s.context_note}" if s.context_note else "  (no SLM note)")
        for i, s in enumerate(snippets[:5])
    )
    if not snippets_text:
        snippets_text = "(no snippets retrieved)"

    user_msg = JUDGE_TEMPLATE.format(prompt=prompt, snippets_text=snippets_text)

    client = OpenAI(api_key=api_key)
    response = client.chat.completions.create(
        model="gpt-4o-mini",
        max_tokens=128,
        messages=[
            {"role": "system", "content": JUDGE_SYSTEM},
            {"role": "user", "content": user_msg},
        ],
    )
    raw = response.choices[0].message.content.strip()
    parsed = json.loads(raw)
    return int(parsed["score"]), str(parsed["verdict"])

# ---------------------------------------------------------------------------
# Core eval loop
# ---------------------------------------------------------------------------

def run_eval(repo_root: Path, use_judge: bool, api_key: Optional[str]) -> list[EvalResult]:
    results = []
    for entry in PROMPTS:
        pid = entry["id"]
        prompt = entry["prompt"]
        expected_inject = entry["expected_inject"]
        print(f"  [{pid:2d}/12] {prompt[:60]}...", end="", flush=True)

        result = EvalResult(
            id=pid,
            prompt=prompt,
            intent=entry["intent"],
            expected_inject=expected_inject,
            actual_inject=False,
            budi_intent="",
            budi_confidence=0.0,
            budi_skip_reason=None,
        )

        try:
            data = run_budi_preview(repo_root, prompt)
            diag = data.get("diagnostics", {})
            result.actual_inject = bool(diag.get("recommended_injection", False))
            result.budi_intent = diag.get("intent", "")
            result.budi_confidence = float(diag.get("confidence", 0.0))
            result.budi_skip_reason = diag.get("skip_reason")
            result.snippets = [
                SnippetResult(
                    path=s.get("path", ""),
                    start_line=s.get("start_line", 0),
                    end_line=s.get("end_line", 0),
                    score=float(s.get("score", 0.0)),
                    context_note=s.get("context_note"),
                )
                for s in data.get("snippets", [])[:5]
            ]

            if use_judge and api_key:
                try:
                    score, verdict = judge_snippets(prompt, result.snippets, api_key)
                    result.judge_score = score
                    result.judge_verdict = verdict
                except Exception as e:
                    result.judge_verdict = f"judge error: {e}"

        except Exception as e:
            result.error = str(e)

        result.passed = (result.actual_inject == expected_inject)
        status = "PASS" if result.passed else "FAIL"
        print(f" {status}")
        results.append(result)

    return results

# ---------------------------------------------------------------------------
# Report generation
# ---------------------------------------------------------------------------

def write_json(results: list[EvalResult], out_dir: Path) -> Path:
    out_dir.mkdir(parents=True, exist_ok=True)
    path = out_dir / "react-eval-results.json"
    serializable = []
    for r in results:
        d = asdict(r)
        serializable.append(d)
    path.write_text(json.dumps(serializable, indent=2))
    return path


def write_markdown(results: list[EvalResult], out_dir: Path) -> Path:
    out_dir.mkdir(parents=True, exist_ok=True)
    path = out_dir / "react-eval-report.md"

    passed = sum(1 for r in results if r.passed)
    total = len(results)

    lines = [
        "# Budi Retrieval Eval — React",
        "",
        f"**Pass rate: {passed}/{total}**",
        "",
        "| # | Prompt | Expected | Actual | Intent | Confidence | Skip reason | Judge | Pass |",
        "|---|--------|----------|--------|--------|-----------|------------|-------|------|",
    ]

    for r in results:
        judge_cell = str(r.judge_score) if r.judge_score is not None else "—"
        inject_symbol = lambda b: "inject" if b else "skip"
        pass_symbol = "✓" if r.passed else "✗"
        prompt_short = r.prompt[:50] + ("…" if len(r.prompt) > 50 else "")
        lines.append(
            f"| {r.id} | {prompt_short} "
            f"| {inject_symbol(r.expected_inject)} "
            f"| {inject_symbol(r.actual_inject)} "
            f"| {r.budi_intent} "
            f"| {r.budi_confidence:.2f} "
            f"| {r.budi_skip_reason or '—'} "
            f"| {judge_cell} "
            f"| {pass_symbol} |"
        )

    if any(r.judge_verdict for r in results):
        lines += ["", "## Judge Verdicts", ""]
        for r in results:
            if r.judge_verdict:
                lines.append(f"**[{r.id}]** {r.prompt}  ")
                lines.append(f"> {r.judge_verdict}")
                lines.append("")

    if any(r.error for r in results):
        lines += ["", "## Errors", ""]
        for r in results:
            if r.error:
                lines.append(f"- **[{r.id}]** {r.prompt}: `{r.error}`")

    path.write_text("\n".join(lines) + "\n")
    return path


def print_summary(results: list[EvalResult]) -> None:
    passed = sum(1 for r in results if r.passed)
    total = len(results)
    print(f"\nResults: {passed}/{total} passed")
    print()
    for r in results:
        status = "PASS" if r.passed else "FAIL"
        inject = "inject" if r.actual_inject else "skip  "
        expected = "inject" if r.expected_inject else "skip  "
        judge = f"  judge={r.judge_score}" if r.judge_score is not None else ""
        print(f"  [{r.id:2d}] {status}  expected={expected} actual={inject}  intent={r.budi_intent:<18} conf={r.budi_confidence:.2f}{judge}")
        if r.error:
            print(f"        ERROR: {r.error}")

# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

def main() -> int:
    parser = argparse.ArgumentParser(description="Budi retrieval eval on the React repo.")
    parser.add_argument("--repo", type=Path, required=True, help="Path to the cloned React repo")
    parser.add_argument("--judge", action="store_true", help="Enable Claude API judging (requires ANTHROPIC_API_KEY)")
    parser.add_argument("--output-dir", type=Path, default=Path("./react-eval-out"), help="Directory to write output files")
    args = parser.parse_args()

    repo_root = args.repo.expanduser().resolve()
    if not repo_root.exists():
        print(f"error: repo not found: {repo_root}", file=sys.stderr)
        return 1

    api_key: Optional[str] = None
    if args.judge:
        api_key = os.environ.get("OPENAI_API_KEY")
        if not api_key:
            print("error: --judge requires OPENAI_API_KEY env var", file=sys.stderr)
            return 1
        try:
            from openai import OpenAI  # noqa: F401
        except ImportError:
            print("error: openai package not installed. Run: pip install openai", file=sys.stderr)
            return 1

    print(f"Repo: {repo_root}")
    print(f"Judge: {'enabled' if args.judge else 'disabled'}")
    print(f"Running {len(PROMPTS)} prompts...\n")

    results = run_eval(repo_root, use_judge=args.judge, api_key=api_key)

    json_path = write_json(results, args.output_dir)
    md_path = write_markdown(results, args.output_dir)
    print_summary(results)
    print(f"\nOutputs:")
    print(f"  {json_path}")
    print(f"  {md_path}")

    passed = sum(1 for r in results if r.passed)
    return 0 if passed >= 10 else 1


if __name__ == "__main__":
    sys.exit(main())
