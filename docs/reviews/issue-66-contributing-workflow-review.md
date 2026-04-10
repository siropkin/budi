# Issue #66 review: contributing workflow and development documentation

## What was reviewed

- Contributor setup and validation docs: `CONTRIBUTING.md`, `README.md`
- Architecture/source-of-truth notes: `SOUL.md`
- Review process templates: `.github/pull_request_template.md`
- Intake quality templates: `.github/ISSUE_TEMPLATE/bug_report.yml`, `.github/ISSUE_TEMPLATE/feature_request.yml`
- Referenced workflow scripts: `scripts/build-dashboard.sh`, `scripts/install.sh`

## Findings (highest impact first)

1. **Architecture doc inconsistency:** `SOUL.md` described "Six tables" while listing eight tables. This creates avoidable confusion for migration and analytics reviewers.
2. **Validation guidance drift:** contributor docs did not clearly separate CI-equivalent checks from fast local loops, and frontend examples mixed `npm install`/`npm ci`.
3. **Review ambiguity:** PR template did not explicitly ask for review scope/findings/follow-ups, which leads to variable PR quality for review-driven issues.
4. **Issue intake gaps:** bug/feature templates were missing structured prompts for diagnostics, impact, scope boundaries, and suggested validation.

## Changes made

- Fixed the SQLite table-count mismatch in `SOUL.md`.
- Strengthened `CONTRIBUTING.md` with:
  - prerequisites and area-scoped validation guidance,
  - CI-equivalent rustfmt check command,
  - consistent `npm ci` guidance,
  - findings-first PR review expectations,
  - contributor troubleshooting quick hits.
- Expanded `README.md` contributing section with:
  - direct architecture link (`SOUL.md`),
  - compact validation matrix for Rust/dashboard/extension changes.
- Updated `.github/pull_request_template.md` to require:
  - review scope and findings context,
  - deferred follow-ups disclosure,
  - validation evidence and issue linkage.
- Updated issue templates:
  - bug report: added diagnostics and impact/urgency prompts,
  - feature request: added scope/non-goals and suggested validation prompts.

## Risks / trade-offs

- These are documentation/template updates only; no runtime behavior or binary artifacts changed.
- Slightly more structured templates increase authoring overhead, but reduce review back-and-forth and missing context.

## Follow-ups not included in this PR

- Add a lightweight `scripts/validate-changed-area.sh` helper to automate area-specific checks (Rust vs dashboard vs extension) and keep docs + CI permanently aligned.
- Consider adding `.github/ISSUE_TEMPLATE/config.yml` with contact links and issue-form defaults if triage volume increases.
