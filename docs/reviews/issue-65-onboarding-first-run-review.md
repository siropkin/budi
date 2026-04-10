# Issue #65 review: onboarding flow and first-run documentation

## Findings (ordered by severity)

1. **No single "first 5 minutes" path in README**
   - The README had all required commands, but install, init, first sync, app restart, and first verification were spread across sections.
   - New users could finish install and still be unsure what to do next.
   - Fixed in this PR by adding a single first-run checklist with ordered steps and expected outcomes.

2. **PATH/duplicate binary recovery guidance was fragmented**
   - README warned about mixed installs, but did not give a compact command set to confirm which binary was actually being executed.
   - This increases confusion during upgrade/reinstall troubleshooting.
   - Fixed in this PR by adding explicit macOS/Linux and Windows commands to verify active binaries.

3. **Cursor extension first-run validation was implicit**
   - Extension README described troubleshooting, but did not provide a short smoke test sequence for the happy path.
   - Fixed in this PR by adding a first-run smoke check with expected UI behavior.

## Changes made

- `README.md`
  - Added "First run checklist (5 minutes)" with install/init/integration selection/sync/verification/restart flow.
  - Added "PATH and duplicate binary checks" section with concrete commands for Unix and Windows.
- `extensions/cursor-budi/README.md`
  - Added "First-run smoke check" with a quick sequence to validate hooks, session tracking, and panel updates.

## Notable risks

- Documentation still assumes users can run shell commands directly; users in locked-down corporate environments may need an additional "restricted shell" path.
- The README remains long; even with a checklist, future growth can reintroduce onboarding drift if first-run content is not kept near install instructions.

## Validation and review method

- Reviewed issue scope and relevant files:
  - `README.md`
  - `SOUL.md`
  - `scripts/install.sh`
  - `scripts/install-standalone.sh`
  - `scripts/install-standalone.ps1`
  - `crates/budi-cli/src/commands/init.rs`
  - `crates/budi-cli/src/commands/integrations.rs`
  - `extensions/cursor-budi/README.md`
  - `frontend/dashboard/README.md`
- Confirmed documented first-run behavior aligns with current code:
  - `budi init` integration prompt flow, daemon start, sync behavior, and next-step messaging
  - installer auto-init behavior and PATH messaging
  - Cursor extension install + refresh workflow

## Follow-ups (not included in this PR)

1. Add an automated docs smoke test that verifies README command snippets against current CLI flags.
2. Add a short troubleshooting decision tree ("symptom -> next command") for top onboarding failures.
3. Consider splitting README onboarding into a dedicated `docs/getting-started.md` and keeping README as a concise entry point.
