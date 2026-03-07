#!/usr/bin/env python3
"""
Budi Session Analyzer — reads hook-io.jsonl for a repo and produces a per-session report.

Usage:
    python3 scripts/session_analyzer.py --repo verkada-web
    python3 scripts/session_analyzer.py --repo verkada-web --date 2026-03-06
    python3 scripts/session_analyzer.py --repo verkada-web --json
    python3 scripts/session_analyzer.py --log-file /path/to/hook-io.jsonl
"""

import argparse
import glob
import json
import os
import sys
from collections import defaultdict
from datetime import datetime, timezone
from pathlib import Path


def find_log_file(repo_name: str) -> Path | None:
    budi_data = Path.home() / ".local" / "share" / "budi" / "repos"
    # Match repos whose dir name starts with the given repo name prefix.
    candidates = sorted(budi_data.glob(f"{repo_name}*/logs/hook-io.jsonl"))
    return candidates[0] if candidates else None


def parse_ts(ts_unix_ms: int | None) -> datetime | None:
    if ts_unix_ms is None:
        return None
    return datetime.fromtimestamp(ts_unix_ms / 1000.0, tz=timezone.utc)


def ms_to_human(ms: float) -> str:
    if ms < 1000:
        return f"{ms:.0f}ms"
    return f"{ms/1000:.1f}s"


def load_events(log_path: Path, date_filter: str | None = None) -> list[dict]:
    events = []
    with open(log_path) as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                ev = json.loads(line)
            except json.JSONDecodeError:
                continue
            if date_filter:
                ts = ev.get("ts_unix_ms")
                if ts:
                    dt = parse_ts(ts)
                    if dt and dt.strftime("%Y-%m-%d") != date_filter:
                        continue
            events.append(ev)
    return events


def analyze_sessions(events: list[dict]) -> dict:
    """Group events by session_id and compute per-session stats."""
    # Separate input/output events.
    sessions: dict[str, dict] = defaultdict(lambda: {
        "prompts": [],
        "output_events": [],
        "session_end": None,
    })

    for ev in events:
        sid = ev.get("session_id") or "__no_session__"
        event_type = ev.get("event", "")
        phase = ev.get("phase", "")

        if event_type == "UserPromptSubmit" and phase == "input":
            sessions[sid]["prompts"].append(ev)
        elif event_type == "UserPromptSubmit" and phase == "output":
            sessions[sid]["output_events"].append(ev)
        elif event_type == "SessionEnd":
            sessions[sid]["session_end"] = ev

    return dict(sessions)


def session_report(sid: str, data: dict) -> dict:
    outputs = data["output_events"]
    prompts = data["prompts"]
    session_end = data["session_end"]

    if not outputs:
        return {}

    all_ts = [ev.get("ts_unix_ms") for ev in outputs if ev.get("ts_unix_ms")]
    first_ts = min(all_ts) if all_ts else None
    last_ts = max(all_ts) if all_ts else None
    duration_secs = (last_ts - first_ts) / 1000 if (first_ts and last_ts) else 0

    injected = [ev for ev in outputs if ev.get("recommended_injection")]
    skipped = [ev for ev in outputs if not ev.get("recommended_injection")]

    # Skip reason breakdown.
    skip_reasons: dict[str, int] = defaultdict(int)
    for ev in skipped:
        reason = ev.get("skip_reason") or "unknown"
        skip_reasons[reason] += 1

    # Intent distribution.
    intent_counts: dict[str, int] = defaultdict(int)
    for ev in outputs:
        intent = ev.get("retrieval_intent") or "unknown"
        intent_counts[intent] += 1

    # Score / latency / snippet stats (injected only).
    scores = [ev.get("retrieval_top_score", 0) for ev in injected]
    latencies = [ev.get("latency_ms", 0) for ev in outputs]
    snippet_counts = [ev.get("snippets_count", 0) for ev in injected]
    ctx_chars = [ev.get("context_chars", 0) for ev in injected]

    avg = lambda xs: sum(xs) / len(xs) if xs else 0.0

    # Top injected files from snippet_refs.
    file_counts: dict[str, int] = defaultdict(int)
    for ev in injected:
        for ref in ev.get("snippet_refs", []):
            path = ref.get("path", "")
            if path:
                file_counts[path] += 1
    top_files = sorted(file_counts.items(), key=lambda x: -x[1])[:10]

    # Prompt excerpts that were skipped (for miss analysis).
    miss_excerpts = []
    for ev in prompts:
        # Find matching output event.
        ts = ev.get("ts_unix_ms", 0)
        prompt_text = ev.get("prompt_excerpt", "")
        # Check the corresponding output event.
        matching_output = next(
            (o for o in skipped if abs((o.get("ts_unix_ms", 0) - ts)) < 5000),
            None,
        )
        if matching_output and prompt_text:
            miss_excerpts.append({
                "prompt": prompt_text[:120],
                "skip_reason": matching_output.get("skip_reason") or "unknown",
                "top_score": matching_output.get("retrieval_top_score", 0),
            })

    return {
        "session_id": sid,
        "first_ts": parse_ts(first_ts).isoformat() if first_ts else None,
        "last_ts": parse_ts(last_ts).isoformat() if last_ts else None,
        "duration_secs": int(duration_secs),
        "total_prompts": len(outputs),
        "injected": len(injected),
        "skipped": len(skipped),
        "injection_rate": len(injected) / len(outputs) if outputs else 0.0,
        "skip_reasons": dict(skip_reasons),
        "intent_distribution": dict(intent_counts),
        "avg_top_score": round(avg(scores), 3),
        "avg_latency_ms": round(avg(latencies), 1),
        "avg_snippets_count": round(avg(snippet_counts), 1),
        "avg_context_chars": round(avg(ctx_chars), 0),
        "top_files": top_files,
        "miss_excerpts": miss_excerpts[:5],
        "session_end_summary": session_end,
    }


def print_markdown_report(reports: list[dict], daily: dict):
    print("# Budi Session Analytics Report\n")
    print(f"**Date:** {daily['date']}  ")
    print(f"**Sessions:** {daily['session_count']}  ")
    print(f"**Total prompts:** {daily['total_prompts']}  ")
    print(f"**Overall injection rate:** {daily['injection_rate']:.1%}  ")
    print()

    # Daily top files.
    if daily["top_files"]:
        print("## Daily Top Files\n")
        print("| File | Injections |")
        print("|------|-----------|")
        for path, count in daily["top_files"][:10]:
            short = path[-60:] if len(path) > 60 else path
            print(f"| `{short}` | {count} |")
        print()

    # Per-session breakdown.
    for r in reports:
        if not r:
            continue
        sid = r["session_id"]
        sid_short = sid[:12] + "…" if sid and len(sid) > 12 else (sid or "—")
        duration = f"{r['duration_secs']//60}m{r['duration_secs']%60}s"
        print(f"## Session `{sid_short}`")
        print(f"- **Duration:** {duration} ({r['first_ts']} → {r['last_ts']})")
        print(f"- **Prompts:** {r['total_prompts']} total, "
              f"{r['injected']} injected, {r['skipped']} skipped "
              f"({r['injection_rate']:.1%})")
        print(f"- **Avg score:** {r['avg_top_score']}  "
              f"**Avg latency:** {ms_to_human(r['avg_latency_ms'])}  "
              f"**Avg snippets:** {r['avg_snippets_count']}  "
              f"**Avg ctx chars:** {r['avg_context_chars']:.0f}")

        if r["intent_distribution"]:
            intent_str = ", ".join(
                f"{k}×{v}" for k, v in sorted(r["intent_distribution"].items(), key=lambda x: -x[1])
            )
            print(f"- **Intents:** {intent_str}")

        if r["skip_reasons"]:
            skip_str = ", ".join(f"{k}×{v}" for k, v in r["skip_reasons"].items())
            print(f"- **Skip reasons:** {skip_str}")

        if r["top_files"]:
            print("- **Top files:**")
            for path, count in r["top_files"][:5]:
                short = path[-70:] if len(path) > 70 else path
                print(f"  - `{short}` ({count}×)")

        if r["miss_excerpts"]:
            print("- **Miss examples:**")
            for m in r["miss_excerpts"][:3]:
                print(f"  - score={m['top_score']:.2f} [{m['skip_reason']}] `{m['prompt']}`")

        print()


def compute_daily_summary(reports: list[dict], date: str) -> dict:
    total_prompts = sum(r.get("total_prompts", 0) for r in reports if r)
    total_injected = sum(r.get("injected", 0) for r in reports if r)

    all_files: dict[str, int] = defaultdict(int)
    for r in reports:
        if not r:
            continue
        for path, count in r.get("top_files", []):
            all_files[path] += count

    top_files = sorted(all_files.items(), key=lambda x: -x[1])[:10]

    return {
        "date": date,
        "session_count": len([r for r in reports if r]),
        "total_prompts": total_prompts,
        "total_injected": total_injected,
        "injection_rate": total_injected / total_prompts if total_prompts else 0.0,
        "top_files": top_files,
    }


def main():
    parser = argparse.ArgumentParser(description="Budi session analytics")
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--repo", help="Repo name prefix (e.g. verkada-web)")
    group.add_argument("--log-file", help="Direct path to hook-io.jsonl")
    parser.add_argument("--date", help="Filter by date YYYY-MM-DD (default: today)")
    parser.add_argument("--json", action="store_true", help="Output raw JSON instead of markdown")
    parser.add_argument("--all-dates", action="store_true", help="Include all dates (no date filter)")
    args = parser.parse_args()

    # Resolve log file.
    if args.log_file:
        log_path = Path(args.log_file)
    else:
        log_path = find_log_file(args.repo)
        if not log_path:
            print(f"ERROR: No hook-io.jsonl found for repo '{args.repo}'", file=sys.stderr)
            sys.exit(1)

    if not log_path.exists():
        print(f"ERROR: Log file not found: {log_path}", file=sys.stderr)
        sys.exit(1)

    # Determine date filter.
    if args.all_dates:
        date_filter = None
        date_label = "all"
    else:
        date_filter = args.date or datetime.now().strftime("%Y-%m-%d")
        date_label = date_filter

    events = load_events(log_path, date_filter)
    if not events:
        print(f"No events found for date={date_label} in {log_path}")
        sys.exit(0)

    sessions = analyze_sessions(events)
    reports = [session_report(sid, data) for sid, data in sessions.items()]
    reports = [r for r in reports if r]  # drop empty

    # Sort by first_ts.
    reports.sort(key=lambda r: r.get("first_ts") or "")

    daily = compute_daily_summary(reports, date_label)

    if args.json:
        print(json.dumps({"daily": daily, "sessions": reports}, indent=2, default=str))
    else:
        print_markdown_report(reports, daily)


if __name__ == "__main__":
    main()
