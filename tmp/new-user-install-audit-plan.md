# Fresh-User Install Audit (Claude + Cursor, Homebrew + Standalone)

## Goal
Simulate a first-time user who only has access to public information from:
- GitHub README
- Public release assets/install scripts

Validate whether installation and "what to do next" are clear, and capture unclear moments with concrete improvements.

## Constraints
- Use only public setup instructions and public commands.
- No internal code knowledge during persona simulation.

## Environment Used (This Run)
- Date: 2026-04-01 (PDT)
- OS: macOS
- Shell: zsh
- Homebrew: present
- Repo for notes: `https://github.com/siropkin/budi`

---

## Exact Test Matrix

| Pass | Install Method | Persona | Validation Commands |
|---|---|---|---|
| A | Homebrew | Claude Code | `budi doctor`, `budi integrations list`, `budi open`, `budi stats`, `budi sync` |
| A | Homebrew | Cursor | same + `budi stats -p all --provider cursor --format json`, `GET /health/integrations` |
| B | Standalone script | Claude Code | `budi doctor`, `budi integrations list`, `budi open`, `budi stats`, `budi sync` |
| B | Standalone script | Cursor | same + `budi stats -p all --provider cursor --format json`, `GET /health/integrations` |

Precondition before each pass:
1. `budi uninstall --yes`
2. `brew uninstall budi` (if present)
3. `rm ~/.local/bin/budi ~/.local/bin/budi-daemon` (if present)
4. Verify clean state:
   - `command -v budi` -> empty
   - `command -v budi-daemon` -> empty
   - `curl http://127.0.0.1:7878/health` fails

---

## Step-by-Step Runbook (Reusable)

### 1) Clean state
```bash
budi uninstall --yes || true
brew uninstall budi || true
rm -f ~/.local/bin/budi ~/.local/bin/budi-daemon
command -v budi || true
command -v budi-daemon || true
curl -sS http://127.0.0.1:7878/health || true
```

### 2) Pass A (Homebrew)
```bash
brew install siropkin/budi/budi
budi init
```
Then run persona checks.

### 3) Reset to clean state
Run step (1) again.

### 4) Pass B (Standalone)
```bash
curl -fsSL https://raw.githubusercontent.com/siropkin/budi/main/scripts/install-standalone.sh | bash
```
Then run persona checks.

### 5) Persona checks (both personas)
```bash
budi doctor
budi integrations list
budi open
budi stats
budi sync
budi stats -p all --provider cursor --format json
curl -sS http://127.0.0.1:7878/health
curl -sS http://127.0.0.1:7878/health/integrations
```

---

## Observation Template (Per Step)

| Field | Fill-in |
|---|---|
| Step ID | e.g. A-INIT-01 |
| Command / Action | exact command or UI action |
| User expectation | what a new user expects |
| Actual result | what happened |
| Clarity score (1-5) | 1 very unclear, 5 very clear |
| Friction type | discoverability / wording / failure recovery / timing / platform mismatch |
| Why confusing | short explanation |
| Suggested fix | concrete text/UX change |
| Owner area | README / CLI output / installer / post-install UX |
| Severity | Critical / High / Medium / Low |

---

## Scoring Rubric

Score each pass from 1-5 for:
1. Install discoverability
2. Command clarity
3. Failure guidance quality
4. Post-install next-step clarity
5. Confidence to continue without external help

Interpretation:
- 5: friction-free for typical user
- 4: minor friction, clear recovery
- 3: noticeable friction, recoverable
- 2: confusing, likely to stall users
- 1: blocker-level confusion

---

## Prioritized Improvement Log Format

| Priority | Severity | Where encountered | Why confusing | Recommended fix | Owner area |
|---|---|---|---|---|---|
| P0/P1/P2/P3 | Critical/High/Medium/Low | step/command | 1-2 lines | concrete copy/behavior change | README/CLI/installer/post-install |

Prioritization rule:
- Quick wins first (copy, ordering, next-step hints)
- Then deeper UX/reliability changes

---

## Findings From This Execution

## Pass A (Homebrew) Results
- Clean uninstall flow worked as documented.
- `brew install siropkin/budi/budi && budi init` worked.
- Initial sync duration was long: ~136.9s (`371733 messages from 22096 files`).
- Init completion message had clear next steps (dashboard URL + `budi stats` + editor restart).

### Notable friction in this pass
- Intermittent daemon availability was observed during validation:
  - `budi doctor` reported daemon started.
  - `budi sync` then failed with `cannot reach daemon`.
  - Daemon log recorded `Address already in use (os error 48)`.
- From a new user perspective, this is confusing because health appears green first, then sync immediately fails.

## Pass B (Standalone Script) Results
- Clean reset + standalone install worked as documented.
- Installer behavior was clear:
  - downloads asset
  - verifies checksum
  - installs binaries
  - auto-runs `budi init`
- Initial sync duration: ~123.4s.
- Post-install persona checks succeeded:
  - `budi doctor`, `budi sync`, `/health`, `/health/integrations` all okay.

## Clarity Assessment
- Install completion clarity: **Yes** (both methods provide clear next actions).
- Post-install next-step clarity: **Mostly yes** (dashboard/stats/restart guidance is explicit).
- Failure guidance quality: **Mixed** (good `run budi doctor` hints, but daemon flaps can still feel opaque).

---

## Prioritized Improvements (Actionable)

| Priority | Severity | Where encountered | Why confusing | Recommended fix | Owner area |
|---|---|---|---|---|---|
| P1 | High | Homebrew pass: `doctor` then `sync` | User sees "daemon started" then immediate "cannot reach daemon". | In `budi sync` error, append: `If this repeats: run 'budi doctor' then 'budi init'` and optionally include last daemon log path. | CLI output |
| P1 | High | Daemon startup conflict (`Address already in use`) | No user-facing hint explains what process held the port at failure moment. | When bind fails, print a concise actionable hint in CLI path (`port 7878 in use, try pkill/taskkill + retry`) with platform-specific command. | post-install UX |
| P2 | Medium | First init sync duration (~2+ min) | "may take a few minutes" is correct but lacks progress feel. | Add periodic progress heartbeat during long sync (e.g., scanned files/messages counters every N seconds). | installer / CLI output |
| P2 | Medium | Dual install methods in README | New users may not realize standalone auto-runs `init` while Homebrew needs manual `init`. | Add a short comparison table under Install: `Homebrew -> run init`, `Standalone -> init runs automatically`. | README |
| P3 | Low | No-data first run scenario | Current run had existing transcripts; guidance for "empty data" not explicitly shown right after init. | Add post-init tip when message count is 0: "No transcript data yet; open Claude/Cursor and send one prompt, then run budi sync." | post-install UX |

---

## Acceptance Criteria Status

1. Clean uninstall verified before each pass: **PASS**
2. Install completion clarity: **PASS**
3. Post-install next-step clarity: **PASS (with minor gaps)**
4. Failure guidance quality: **PARTIAL PASS** (improve daemon conflict messaging)
5. Actionable improvement report produced: **PASS**

---

## Next Session Focus
1. Reproduce and isolate daemon port-flap case from Homebrew path in a minimal sequence.
2. Validate first-run "no data yet" UX by forcing a clean environment with no Claude/Cursor transcripts.
3. Implement P1 quick wins in CLI messaging first, then P2 docs/progress polish.
