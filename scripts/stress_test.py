#!/usr/bin/env python3
"""
Budi daemon stress test suite.

Usage:
    python3 scripts/stress_test.py --repo-root /path/to/repo
    python3 scripts/stress_test.py --repo-root /path/to/repo --scenario 2
    python3 scripts/stress_test.py --repo-root /path/to/repo --all

Exit code: 0 if all pass, 1 if any fail.
"""

import argparse
import concurrent.futures
import json
import os
import subprocess
import sys
import tempfile
import time
import tomllib
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional
import urllib.request
import urllib.error

# ---------------------------------------------------------------------------
# Data types
# ---------------------------------------------------------------------------

@dataclass
class ScenarioResult:
    name: str
    passed: bool
    duration_ms: int
    details: str = ""
    errors: list[str] = field(default_factory=list)


# ---------------------------------------------------------------------------
# Daemon helpers
# ---------------------------------------------------------------------------

def detect_daemon_port(repo_root: Path) -> int:
    """Read daemon port from budi config (or default to 7878)."""
    # Try repo-local config first, then global data dir
    candidates = []
    # ~/.local/share/budi/repos/<id>/config.toml
    data_base = Path.home() / ".local" / "share" / "budi" / "repos"
    if data_base.exists():
        for d in data_base.iterdir():
            cfg = d / "config.toml"
            if cfg.exists():
                candidates.append(cfg)
    for cfg_path in candidates:
        try:
            with open(cfg_path, "rb") as f:
                cfg = tomllib.load(f)
            if "daemon_port" in cfg:
                return int(cfg["daemon_port"])
        except Exception:
            pass
    return 7878


def daemon_url(port: int, path: str) -> str:
    return f"http://127.0.0.1:{port}{path}"


def post_json(port: int, path: str, payload: dict, timeout: float = 10.0) -> tuple[int, dict]:
    data = json.dumps(payload).encode()
    req = urllib.request.Request(
        daemon_url(port, path),
        data=data,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            body = json.loads(resp.read())
            return resp.status, body
    except urllib.error.HTTPError as e:
        body = {}
        try:
            body = json.loads(e.read())
        except Exception:
            pass
        return e.code, body
    except Exception as e:
        return 0, {"error": str(e)}


def status_json(port: int, repo_root: Path, timeout: float = 10.0) -> tuple[int, dict]:
    return post_json(port, "/status", {"repo_root": str(repo_root)}, timeout=timeout)


def progress_json(port: int, repo_root: Path, timeout: float = 10.0) -> tuple[int, dict]:
    return post_json(port, "/progress", {"repo_root": str(repo_root)}, timeout=timeout)


def check_daemon(port: int) -> bool:
    """Quick liveness check — use a fake repo_root, any 200 or 4xx means daemon is up."""
    req = urllib.request.Request(
        daemon_url(port, "/status"),
        data=json.dumps({"repo_root": "/tmp/liveness"}).encode(),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=2.0) as resp:
            return resp.status < 500
    except urllib.error.HTTPError as e:
        return e.code < 500
    except Exception:
        return False


# ---------------------------------------------------------------------------
# Scenario 1: Concurrent query storm
# ---------------------------------------------------------------------------

PROMPTS = [
    "How does the query pipeline work?",
    "Where is session dedup implemented?",
    "How does intent classification work?",
    "What does the call graph summary contain?",
    "How is context budget calculated?",
    "How does prefetch_neighbors work?",
    "What is the format_context function?",
    "How are chunk scores computed?",
    "What does min_inject_score do?",
    "How does the session affinity file work?",
]


def scenario1_concurrent_storm(port: int, repo_root: Path) -> ScenarioResult:
    name = "S1: Concurrent query storm"
    t0 = time.monotonic()
    errors = []

    def do_query(prompt: str) -> tuple[bool, float, dict]:
        t = time.monotonic()
        status, body = post_json(port, "/query", {
            "repo_root": str(repo_root),
            "prompt": prompt,
            "session_id": "stress-s1",
        }, timeout=15.0)
        elapsed = (time.monotonic() - t) * 1000
        ok = status == 200 and "error" not in body
        return ok, elapsed, body

    with concurrent.futures.ThreadPoolExecutor(max_workers=10) as exe:
        futures = [exe.submit(do_query, p) for p in PROMPTS]
        results = [f.result() for f in concurrent.futures.as_completed(futures)]

    n_ok = sum(1 for ok, _, _ in results if ok)
    latencies = [ms for _, ms, _ in results]
    p99 = sorted(latencies)[int(len(latencies) * 0.99)] if latencies else 0
    n_candidates = sum(
        1 for ok, _, body in results
        if ok and body.get("total_candidates", 0) > 0
    )

    if n_ok < 10:
        errors.append(f"Only {n_ok}/10 requests succeeded")
    if p99 > 5000:
        errors.append(f"p99 latency {p99:.0f}ms > 5000ms")
    if n_candidates < 8:
        errors.append(f"Only {n_candidates}/10 responses have total_candidates > 0")

    duration_ms = int((time.monotonic() - t0) * 1000)
    passed = len(errors) == 0
    details = f"{n_ok}/10 ok, p99={p99:.0f}ms, {n_candidates}/10 with candidates"
    return ScenarioResult(name, passed, duration_ms, details, errors)


# ---------------------------------------------------------------------------
# Scenario 2: File change → autosync → query round-trip
# ---------------------------------------------------------------------------

SENTINEL = "BUDI_STRESS_SENTINEL_XYZ123"


def scenario2_file_change_roundtrip(port: int, repo_root: Path) -> ScenarioResult:
    name = "S2: File change → autosync → query round-trip"
    t0 = time.monotonic()
    errors = []
    tmp_path = repo_root / f"_stress_sentinel_{int(time.time())}.ts"

    try:
        # 1. Write sentinel file
        tmp_path.write_text(f"// stress test\nexport const {SENTINEL} = 'hello';\n")

        # Get baseline updates_applied
        _, status_before = status_json(port, repo_root)
        baseline = status_before.get("updates_applied", 0)

        # 2. POST /update to trigger immediate indexing
        post_json(port, "/update", {
            "repo_root": str(repo_root),
            "changed_files": [str(tmp_path)],
        }, timeout=10.0)

        # 3. Poll /status until updates_applied increments (max 30s)
        deadline = time.monotonic() + 30
        incremented = False
        while time.monotonic() < deadline:
            _, s = status_json(port, repo_root)
            if s.get("updates_applied", 0) > baseline:
                incremented = True
                break
            time.sleep(0.25)

        if not incremented:
            errors.append("updates_applied did not increment within 15s")

        # 4. Query for sentinel token
        time.sleep(0.2)  # short settle
        _, qbody = post_json(port, "/query", {
            "repo_root": str(repo_root),
            "prompt": f"Where is {SENTINEL} defined?",
        }, timeout=10.0)
        refs = qbody.get("snippet_refs", []) or []
        tmp_name = tmp_path.name  # match on filename (daemon returns relative paths)
        found = any(tmp_name in str(r) for r in refs)
        if not found:
            errors.append(f"Sentinel file not found in snippet_refs: {refs}")

        # 5. Cleanup: delete file and re-index
        tmp_path.unlink()
        post_json(port, "/update", {
            "repo_root": str(repo_root),
            "changed_files": [str(tmp_path)],
        }, timeout=10.0)

        # Poll for update to apply
        _, s2 = status_json(port, repo_root)
        baseline2 = s2.get("updates_applied", baseline)
        deadline2 = time.monotonic() + 15
        while time.monotonic() < deadline2:
            _, s = status_json(port, repo_root)
            if s.get("updates_applied", 0) > baseline2:
                break
            time.sleep(0.25)

        time.sleep(0.2)
        _, qbody2 = post_json(port, "/query", {
            "repo_root": str(repo_root),
            "prompt": f"Where is {SENTINEL} defined?",
        }, timeout=10.0)
        refs2 = qbody2.get("snippet_refs", []) or []
        still_found = any(tmp_name in str(r) for r in refs2)
        if still_found:
            errors.append("Sentinel file still returned after deletion/re-index")

    finally:
        if tmp_path.exists():
            tmp_path.unlink()

    duration_ms = int((time.monotonic() - t0) * 1000)
    passed = len(errors) == 0
    details = f"incremented={incremented}, found_after_add={found}, still_after_del={still_found if 'still_found' in dir() else '?'}"
    return ScenarioResult(name, passed, duration_ms, details, errors)


# ---------------------------------------------------------------------------
# Scenario 3: Query during active indexing
# ---------------------------------------------------------------------------

def scenario3_query_during_indexing(port: int, repo_root: Path) -> ScenarioResult:
    name = "S3: Query during active indexing"
    t0 = time.monotonic()
    errors = []

    # Trigger a full index (hard=false for speed)
    status_code, ibody = post_json(port, "/index", {
        "repo_root": str(repo_root),
        "hard": False,
    }, timeout=10.0)
    if status_code not in (200, 202):
        errors.append(f"/index returned {status_code}: {ibody}")

    # Fire 5 parallel queries immediately
    def do_q(i: int) -> tuple[int, dict]:
        return post_json(port, "/query", {
            "repo_root": str(repo_root),
            "prompt": f"How does chunking work in budi iteration {i}?",
        }, timeout=20.0)

    with concurrent.futures.ThreadPoolExecutor(max_workers=5) as exe:
        futs = [exe.submit(do_q, i) for i in range(5)]
        during_results = [f.result() for f in concurrent.futures.as_completed(futs)]

    n_ok_during = sum(1 for s, b in during_results if s == 200 and "error" not in b)
    if n_ok_during < 5:
        errors.append(f"Only {n_ok_during}/5 queries during indexing succeeded")

    # Wait for indexing to complete (poll /progress, max 120s)
    deadline = time.monotonic() + 120
    done = False
    while time.monotonic() < deadline:
        _, prog = progress_json(port, repo_root)
        state = prog.get("job_state", "")
        if state in ("succeeded", "idle", ""):
            done = True
            break
        time.sleep(1.0)

    if not done:
        errors.append("Indexing did not complete within 120s")

    # Fire 5 more queries after
    with concurrent.futures.ThreadPoolExecutor(max_workers=5) as exe:
        futs2 = [exe.submit(do_q, i + 100) for i in range(5)]
        after_results = [f.result() for f in concurrent.futures.as_completed(futs2)]

    n_ok_after = sum(1 for s, b in after_results if s == 200 and "error" not in b)
    n_cands_after = sum(
        1 for s, b in after_results
        if s == 200 and b.get("total_candidates", 0) > 0
    )
    if n_ok_after < 5:
        errors.append(f"Only {n_ok_after}/5 queries after indexing succeeded")
    if n_cands_after < 3:
        errors.append(f"Only {n_cands_after}/5 post-index queries have candidates")

    # Sanity check /status
    sc, sb = status_json(port, repo_root)
    if sc != 200 or "error" in sb:
        errors.append(f"/status returned {sc}: {sb}")

    duration_ms = int((time.monotonic() - t0) * 1000)
    passed = len(errors) == 0
    details = f"during={n_ok_during}/5 ok, after={n_ok_after}/5 ok, candidates={n_cands_after}/5"
    return ScenarioResult(name, passed, duration_ms, details, errors)


# ---------------------------------------------------------------------------
# Scenario 4: Hook log concurrent writes
# ---------------------------------------------------------------------------

def scenario4_hook_log_concurrency(port: int, repo_root: Path) -> ScenarioResult:
    name = "S4: Hook log concurrent writes"
    t0 = time.monotonic()
    errors = []

    # Find hook-io.jsonl
    data_base = Path.home() / ".local" / "share" / "budi" / "repos"
    hook_log: Optional[Path] = None
    if data_base.exists():
        for d in data_base.iterdir():
            candidate = d / "hook-io.jsonl"
            if candidate.exists():
                hook_log = candidate
                break
    if hook_log is None:
        # Create an approximate path for counting
        hook_log = data_base / "budi-0d09668386a9" / "hook-io.jsonl"

    baseline_lines = 0
    if hook_log.exists():
        with open(hook_log) as f:
            baseline_lines = sum(1 for _ in f)

    N = 5
    synthetic_payload = json.dumps({
        "hook_event_name": "UserPromptSubmit",
        "session_id": "stress-s4",
        "transcript_path": "",
        "prompt": "stress test concurrent write",
    })

    def run_hook(_: int) -> tuple[bool, str]:
        try:
            result = subprocess.run(
                ["budi", "hook", "user-prompt-submit"],
                input=synthetic_payload,
                capture_output=True,
                text=True,
                timeout=15,
                cwd=str(repo_root),
            )
            return result.returncode == 0, result.stderr
        except Exception as e:
            return False, str(e)

    with concurrent.futures.ThreadPoolExecutor(max_workers=N) as exe:
        futs = [exe.submit(run_hook, i) for i in range(N)]
        hook_results = [f.result() for f in concurrent.futures.as_completed(futs)]

    n_ok = sum(1 for ok, _ in hook_results if ok)
    # Note: hooks may skip (non-code prompt) without writing — count as ok if returncode 0
    if n_ok < N:
        failed_errs = [err for ok, err in hook_results if not ok]
        errors.append(f"Only {n_ok}/{N} hook calls returned 0: {failed_errs[:2]}")

    # Check jsonl integrity
    if hook_log.exists():
        new_lines = 0
        bad_lines = 0
        with open(hook_log) as f:
            lines = f.readlines()
        after_lines = len(lines)
        new_lines = after_lines - baseline_lines
        for line in lines[baseline_lines:]:
            line = line.strip()
            if not line:
                continue
            try:
                json.loads(line)
            except json.JSONDecodeError:
                bad_lines += 1

        if bad_lines > 0:
            errors.append(f"{bad_lines} truncated/invalid JSON lines in hook-io.jsonl")

        # Lock file should be cleaned up
        lock_file = hook_log.with_suffix(".lock") if hook_log.suffix == ".jsonl" else hook_log.parent / "hook-io.jsonl.lock"
        # Check for stale lock (if exists and older than 5s)
        if lock_file.exists():
            age = time.time() - lock_file.stat().st_mtime
            if age > 5:
                errors.append(f"Stale lock file found: {lock_file} (age={age:.1f}s)")

        details = f"{n_ok}/{N} hooks ok, {new_lines} new lines, {bad_lines} bad JSON"
    else:
        details = f"{n_ok}/{N} hooks ok (no hook log found at {hook_log})"

    duration_ms = int((time.monotonic() - t0) * 1000)
    passed = len(errors) == 0
    return ScenarioResult(name, passed, duration_ms, details, errors)


# ---------------------------------------------------------------------------
# Scenario 5: Session dedup under parallel load
# ---------------------------------------------------------------------------

def scenario5_session_dedup(port: int, repo_root: Path) -> ScenarioResult:
    name = "S5: Session dedup under parallel load"
    t0 = time.monotonic()
    errors = []

    session_id = f"stress-s5-{int(time.time())}"
    prompts = [
        "How does the daemon load an index?",
        "What is ensure_loaded in daemon.rs?",
        "Where does the daemon store loaded repos?",
        "How does the daemon handle concurrent queries?",
        "What is DaemonState in the budi daemon?",
        "How does the query function work in daemon?",
        "Where is RwLock used in daemon.rs?",
        "How are query results returned from daemon?",
    ]

    def do_q(prompt: str) -> tuple[int, dict]:
        return post_json(port, "/query", {
            "repo_root": str(repo_root),
            "prompt": prompt,
            "session_id": session_id,
        }, timeout=15.0)

    # Fire all 8 in parallel
    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as exe:
        futs = [exe.submit(do_q, p) for p in prompts]
        results = [f.result() for f in concurrent.futures.as_completed(futs)]

    n_ok = sum(1 for s, b in results if s == 200 and "error" not in b)
    if n_ok < 7:
        errors.append(f"Only {n_ok}/8 parallel session queries succeeded")

    # Collect all injected snippet refs across all responses
    all_refs: list[str] = []
    for _, body in results:
        refs = body.get("snippet_refs", []) or []
        all_refs.extend(str(r) for r in refs)

    # Count duplicates: same path:line should not appear more than once across all
    from collections import Counter
    ref_counts = Counter(all_refs)
    dupes = {r: c for r, c in ref_counts.items() if c > 1}
    if dupes:
        errors.append(f"Session dedup failed — duplicate refs across parallel queries: {list(dupes.items())[:3]}")

    # Sanity: /status still works
    sc, sb = status_json(port, repo_root)
    if sc != 200:
        errors.append(f"/status returned {sc} after session dedup test")

    duration_ms = int((time.monotonic() - t0) * 1000)
    passed = len(errors) == 0
    details = f"{n_ok}/8 ok, {len(all_refs)} total refs, {len(dupes)} duplicate refs"
    return ScenarioResult(name, passed, duration_ms, details, errors)


# ---------------------------------------------------------------------------
# Scenario 6: Session affinity race
# ---------------------------------------------------------------------------

def scenario6_affinity_race(port: int, repo_root: Path) -> ScenarioResult:
    name = "S6: Session affinity race"
    t0 = time.monotonic()
    errors = []

    prompts_sessions = [
        ("How is context budget calculated?", f"stress-s6-a-{int(time.time())}"),
        ("Where is chunking implemented?", f"stress-s6-b-{int(time.time())}"),
        ("How does vector search work?", f"stress-s6-c-{int(time.time())}"),
        ("What is the retrieval pipeline?", f"stress-s6-d-{int(time.time())}"),
        ("How does budi index a file?", f"stress-s6-e-{int(time.time())}"),
    ]

    def do_q(prompt: str, session_id: str) -> tuple[int, dict]:
        return post_json(port, "/query", {
            "repo_root": str(repo_root),
            "prompt": prompt,
            "session_id": session_id,
        }, timeout=15.0)

    with concurrent.futures.ThreadPoolExecutor(max_workers=5) as exe:
        futs = [exe.submit(do_q, p, s) for p, s in prompts_sessions]
        results = [f.result() for f in concurrent.futures.as_completed(futs)]

    n_ok = sum(1 for sc, b in results if sc == 200 and "error" not in b)

    # Give spawn_blocking tasks time to write
    time.sleep(0.5)

    # Find session-affinity.json
    affinity_path: Optional[Path] = None
    data_base = Path.home() / ".local" / "share" / "budi" / "repos"
    if data_base.exists():
        for d in data_base.iterdir():
            candidate = d / "session-affinity.json"
            if candidate.exists():
                affinity_path = candidate
                break

    if affinity_path is None or not affinity_path.exists():
        errors.append("session-affinity.json not found after parallel queries")
    else:
        try:
            with open(affinity_path) as f:
                affinity = json.load(f)
            if not isinstance(affinity, (dict, list)):
                errors.append(f"session-affinity.json has unexpected type: {type(affinity)}")
            elif isinstance(affinity, dict) and len(affinity) == 0:
                errors.append("session-affinity.json is empty after parallel injecting queries")
        except json.JSONDecodeError as e:
            errors.append(f"session-affinity.json is invalid JSON: {e}")

    duration_ms = int((time.monotonic() - t0) * 1000)
    passed = len(errors) == 0
    details = f"{n_ok}/5 queries ok, affinity file={'valid' if not errors else 'INVALID'}"
    return ScenarioResult(name, passed, duration_ms, details, errors)


# ---------------------------------------------------------------------------
# Scenario 7: Rapid file churn
# ---------------------------------------------------------------------------

def scenario7_rapid_file_churn(port: int, repo_root: Path) -> ScenarioResult:
    name = "S7: Rapid file churn"
    t0 = time.monotonic()
    errors = []

    N = 20
    tmp_files: list[Path] = []

    _, status_before = status_json(port, repo_root)
    baseline = status_before.get("updates_applied", 0)

    CHURN_TOKEN = "BUDI_STRESS_CHURN_SENTINEL"
    try:
        for i in range(N):
            p = repo_root / f"_stress_churn_{i}_{int(time.time())}.ts"
            # Use a distinctive sentinel so the chunk scores above min_inject_score
            p.write_text(
                f"// {CHURN_TOKEN}_{i}: rapid file churn stress test\n"
                f"export const {CHURN_TOKEN}_{i} = {i};\n"
                f"// end {CHURN_TOKEN}_{i}\n"
            )
            tmp_files.append(p)
            post_json(port, "/update", {
                "repo_root": str(repo_root),
                "changed_files": [str(p)],
            }, timeout=5.0)
            time.sleep(0.05)

        # Poll for updates_applied to grow (max 30s)
        deadline = time.monotonic() + 30
        final_applied = baseline
        while time.monotonic() < deadline:
            _, s = status_json(port, repo_root)
            final_applied = s.get("updates_applied", baseline)
            if final_applied > baseline:
                break
            time.sleep(0.5)

        if final_applied <= baseline:
            errors.append(f"updates_applied did not increment (baseline={baseline}, final={final_applied})")

        # Check update_retries stays low
        _, s_final = status_json(port, repo_root)
        retries = s_final.get("update_retries", 0)
        if retries > 10:
            errors.append(f"update_retries={retries} is too high (SQLite contention?)")

        # Try to query for one of the churn files (within 30s total window)
        deadline2 = time.monotonic() + 15
        found_any = False
        while time.monotonic() < deadline2 and not found_any:
            _, qbody = post_json(port, "/query", {
                "repo_root": str(repo_root),
                "prompt": f"Where is {CHURN_TOKEN}_10 defined?",
            }, timeout=10.0)
            refs = qbody.get("snippet_refs", []) or []
            if any("_stress_churn_" in (r.get("path", "") if isinstance(r, dict) else str(r)) for r in refs):
                found_any = True
                break
            time.sleep(0.5)

        if not found_any:
            errors.append(f"No churn file appeared in query results within 15s (updates_applied +{final_applied - baseline})")

    finally:
        for p in tmp_files:
            if p.exists():
                p.unlink()
        # Trigger cleanup update
        if tmp_files:
            post_json(port, "/update", {
                "repo_root": str(repo_root),
                "changed_files": [str(p) for p in tmp_files],
            }, timeout=5.0)

    duration_ms = int((time.monotonic() - t0) * 1000)
    passed = len(errors) == 0
    delta = final_applied - baseline if 'final_applied' in dir() else -1
    details = f"updates_applied +{delta}, retries={retries if 'retries' in dir() else '?'}, found_churn={found_any if 'found_any' in dir() else '?'}"
    return ScenarioResult(name, passed, duration_ms, details, errors)


# ---------------------------------------------------------------------------
# Runner
# ---------------------------------------------------------------------------

SCENARIOS = {
    1: scenario1_concurrent_storm,
    2: scenario2_file_change_roundtrip,
    3: scenario3_query_during_indexing,
    4: scenario4_hook_log_concurrency,
    5: scenario5_session_dedup,
    6: scenario6_affinity_race,
    7: scenario7_rapid_file_churn,
}


def print_result(r: ScenarioResult) -> None:
    status = "PASS" if r.passed else "FAIL"
    print(f"  [{status}] {r.name} ({r.duration_ms}ms)")
    if r.details:
        print(f"         {r.details}")
    for e in r.errors:
        print(f"         ERROR: {e}")


def main() -> int:
    parser = argparse.ArgumentParser(description="Budi daemon stress test")
    parser.add_argument("--repo-root", required=True, help="Path to the indexed repo")
    parser.add_argument("--scenario", type=int, choices=list(SCENARIOS.keys()),
                        help="Run a single scenario by number")
    parser.add_argument("--all", action="store_true", help="Run all scenarios")
    args = parser.parse_args()

    repo_root = Path(args.repo_root).resolve()
    if not repo_root.exists():
        print(f"ERROR: repo-root does not exist: {repo_root}")
        return 1

    port = detect_daemon_port(repo_root)

    if not check_daemon(port):
        print(f"ERROR: Budi daemon not reachable at port {port}. Start with: budi daemon start")
        return 1

    print(f"Budi stress test — daemon port={port}, repo={repo_root}\n")

    scenarios_to_run: list[int]
    if args.scenario:
        scenarios_to_run = [args.scenario]
    elif args.all:
        scenarios_to_run = list(SCENARIOS.keys())
    else:
        # Default: run all
        scenarios_to_run = list(SCENARIOS.keys())

    results: list[ScenarioResult] = []
    for num in scenarios_to_run:
        fn = SCENARIOS[num]
        print(f"Running {fn.__name__}...")
        try:
            r = fn(port, repo_root)
        except Exception as e:
            r = ScenarioResult(
                name=f"S{num}: (uncaught exception)",
                passed=False,
                duration_ms=0,
                errors=[str(e)],
            )
        results.append(r)
        print_result(r)
        print()
        # Brief settling delay between scenarios to avoid cascading load
        if scenarios_to_run.index(num) < len(scenarios_to_run) - 1:
            time.sleep(1.0)

    n_pass = sum(1 for r in results if r.passed)
    n_fail = len(results) - n_pass
    print(f"{'=' * 50}")
    print(f"Results: {n_pass}/{len(results)} passed, {n_fail} failed")

    return 0 if n_fail == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
