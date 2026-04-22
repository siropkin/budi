//! Session → work outcome derivation (R1.5, #293).
//!
//! Given a session's `repo_id`, `git_branch`, and start/end timestamps,
//! correlate the session with local git state to produce a bounded
//! per-session label describing whether the AI-assisted work actually
//! landed. This is a **rule-based** derivation — no remote git/PR API
//! calls, no content capture, no statistics. The entire logic runs
//! against whatever `git` CLI is on PATH (see ADR-0088 §5).
//!
//! ## Labels
//!
//! - [`WorkOutcomeLabel::Committed`] — at least one commit was authored
//!   by the local git user on the session's branch during or shortly
//!   after the session's active window.
//! - [`WorkOutcomeLabel::BranchMerged`] — the session's branch was
//!   merged into the default integration branch (typically `main` or
//!   `master`) after the session; i.e. the work shipped. This implies
//!   `committed` but is strictly more informative, so we surface it
//!   separately.
//! - [`WorkOutcomeLabel::NoCommit`] — the session ended without any
//!   commit landing on its branch inside the correlation window; the
//!   branch still exists locally. This is the "long agent session that
//!   produced no code change" case the R1.5 ticket calls out.
//! - [`WorkOutcomeLabel::Unknown`] — the inputs were insufficient to
//!   apply any rule (missing branch, missing repo root, branch does not
//!   exist locally, `git` is unavailable, etc.). Never treated as a
//!   signal; analytics should show `Unknown` as a separate bucket so
//!   users understand when derivation is possible.
//!
//! ## Correlation window
//!
//! The session window is `[started_at, ended_at + GRACE]` where
//! `GRACE = 24h`. Commits that land within the grace period are still
//! attributed to the session because developers routinely wrap up a
//! task the next morning. This window is intentionally generous — the
//! goal is to reduce false negatives (work landed but we called it
//! `no_commit`), which would be the more damaging error. False positives
//! are bounded by also requiring the commit to touch the same branch.

use std::path::Path;
use std::process::Command;

use chrono::{DateTime, Utc};

/// Correlation grace window applied after `ended_at` when scanning for
/// commits. Keeps the derivation generous enough that developers who
/// wrap up a task the next morning still see `committed`. 24h is a
/// round, debuggable default; callers can override for tests.
pub const DEFAULT_GRACE: chrono::Duration = chrono::Duration::hours(24);

/// Integration branches we use as the *merge target* when asking
/// whether a session's branch shipped. Intentionally narrower than
/// [`crate::pipeline::is_integration_branch`]: `HEAD` is never a
/// meaningful ref to resolve against or run `git merge-base` against
/// here — it would either alias the current checkout (noisy) or fail
/// silently. The "is this a non-feature branch?" check is delegated to
/// the shared helper so ticket extraction and work-outcome correlation
/// can never disagree (#336).
const MERGE_TARGETS: &[&str] = &["main", "master", "develop"];

/// Bounded set of outcome labels. Matches the set listed in #293 plus
/// `Unknown`; analytics queries pivot on the raw string value so the
/// variants serialize via `as_str()` rather than a derive to keep the
/// wire format explicit and easy to reason about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkOutcomeLabel {
    Committed,
    BranchMerged,
    NoCommit,
    Unknown,
}

impl WorkOutcomeLabel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Committed => "committed",
            Self::BranchMerged => "branch_merged",
            Self::NoCommit => "no_commit",
            Self::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for WorkOutcomeLabel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Result of a single session → outcome derivation. Includes a short
/// rationale so operators and tests can see exactly which rule fired
/// without turning on trace logging.
#[derive(Debug, Clone)]
pub struct WorkOutcome {
    pub label: WorkOutcomeLabel,
    /// One-line human-readable explanation (e.g. "3 commits on
    /// `PAVA-2057-fix` between 10:00 and 11:30"). Never contains file
    /// names or commit messages — keep to counts and metadata so the
    /// derivation stays inside ADR-0083.
    pub rationale: String,
}

impl WorkOutcome {
    pub fn unknown(reason: &str) -> Self {
        Self {
            label: WorkOutcomeLabel::Unknown,
            rationale: reason.to_string(),
        }
    }
}

/// Inputs to the derivation. Kept explicit (rather than pulling from a
/// DB row) so tests can exercise the ruleset against fixture git repos
/// without faking a whole session record.
#[derive(Debug, Clone)]
pub struct WorkOutcomeInputs<'a> {
    pub repo_root: &'a Path,
    pub branch: &'a str,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    /// Optional grace window override. Defaults to `DEFAULT_GRACE`.
    pub grace: Option<chrono::Duration>,
}

/// Derive the session work outcome from local git state. Returns
/// [`WorkOutcomeLabel::Unknown`] whenever the inputs cannot support a
/// decision — callers treat `Unknown` as "show nothing" rather than
/// hiding the session entirely.
pub fn derive_work_outcome(inputs: &WorkOutcomeInputs<'_>) -> WorkOutcome {
    let branch = inputs.branch.trim();
    if branch.is_empty() || crate::pipeline::is_integration_branch(branch) {
        // #445 rewrite: the previous message ("no non-integration
        // branch on session — nothing to correlate") was internal
        // jargon — a fresh user has no mental model of what
        // "integration branch" means, and the rationale suggested an
        // action they could take even though the outcome is inherently
        // un-derivable. Plain-language phrasing tells the reader why
        // we showed "unknown" without implying they can change it.
        return WorkOutcome::unknown(
            "session wasn't tied to a feature branch, so no merge outcome can be inferred",
        );
    }
    if !inputs.repo_root.exists() || !inputs.repo_root.join(".git").exists() {
        return WorkOutcome::unknown("repo_root is not a git working tree");
    }
    if !has_git_on_path() {
        return WorkOutcome::unknown("git binary not on PATH");
    }
    if !branch_exists_locally(inputs.repo_root, branch) {
        // If the branch has disappeared locally, it usually means it
        // was merged and then deleted — the classic "shipped" end state.
        // Confirm by looking for a reachable merge commit on a canonical
        // integration branch instead of declaring `unknown`.
        if branch_was_merged(inputs.repo_root, branch) {
            return WorkOutcome {
                label: WorkOutcomeLabel::BranchMerged,
                rationale: format!(
                    "branch `{branch}` no longer exists locally but is reachable from an integration branch",
                ),
            };
        }
        return WorkOutcome::unknown(&format!(
            "branch `{branch}` not present locally; cannot correlate",
        ));
    }

    let grace = inputs.grace.unwrap_or(DEFAULT_GRACE);
    let since = inputs.started_at;
    let until = inputs.ended_at + grace;
    let unique_commits = count_commits_on_branch(inputs.repo_root, branch, since, until);
    let merged = branch_was_merged(inputs.repo_root, branch);
    let is_idle_at_integration = branch_shares_tip_with_integration(inputs.repo_root, branch);

    // BranchMerged wins over Committed so a retrospective view of
    // "this session shipped" shows up even when the merge itself
    // happened slightly outside the window — what matters is that
    // commits were introduced on the branch and the branch is now
    // reachable from an integration branch.
    if merged && !is_idle_at_integration {
        // We still want to confirm that *some* history exists so an
        // empty branch doesn't get credit. `unique_commits` can be 0
        // post-merge (commits are now reachable from main) so rely on
        // the tip difference instead — already established above.
        return WorkOutcome {
            label: WorkOutcomeLabel::BranchMerged,
            rationale: format!(
                "branch `{branch}` is reachable from an integration branch and has its own history",
            ),
        };
    }

    if unique_commits > 0 {
        return WorkOutcome {
            label: WorkOutcomeLabel::Committed,
            rationale: format!(
                "{unique_commits} commit(s) unique to `{branch}` within session window",
            ),
        };
    }

    WorkOutcome {
        label: WorkOutcomeLabel::NoCommit,
        rationale: format!(
            "no new commits on `{branch}` between {} and {}",
            since.to_rfc3339(),
            until.to_rfc3339(),
        ),
    }
}

fn has_git_on_path() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn branch_exists_locally(repo: &Path, branch: &str) -> bool {
    let refname = format!("refs/heads/{branch}");
    Command::new("git")
        .args(["show-ref", "--verify", "--quiet", &refname])
        .current_dir(repo)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Returns true when the branch's tip is the same commit as one of
/// the integration branches. Used to distinguish an idle branch
/// (checked out from main with no new commits) from a merged branch
/// (merged via `--no-ff`, so its tip is strictly behind main's).
fn branch_shares_tip_with_integration(repo: &Path, branch: &str) -> bool {
    let Some(branch_tip) = resolve_ref(repo, branch) else {
        return false;
    };
    for integration in MERGE_TARGETS {
        if let Some(tip) = resolve_ref(repo, integration)
            && tip == branch_tip
        {
            return true;
        }
    }
    false
}

fn resolve_ref(repo: &Path, refname: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--verify", refname])
        .current_dir(repo)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn branch_was_merged(repo: &Path, branch: &str) -> bool {
    // Try each known integration branch; any positive answer is
    // enough. `git merge-base --is-ancestor <branch> <integration>`
    // returns 0 if the branch is reachable from the integration branch,
    // which is the definition we care about. Skip integration refs
    // that don't exist locally so we don't spray `fatal:` messages to
    // stderr on scratch repos that only carry `main`.
    for integration in MERGE_TARGETS {
        if resolve_ref(repo, integration).is_none() {
            continue;
        }
        let ok = Command::new("git")
            .args(["merge-base", "--is-ancestor", branch, integration])
            .current_dir(repo)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return true;
        }
    }
    false
}

fn count_commits_on_branch(
    repo: &Path,
    branch: &str,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> u32 {
    // Count commits *unique to the branch* within the window — i.e.
    // commits reachable from `branch` but not from any integration
    // branch. This keeps idle feature branches (same tip as `main`)
    // from being credited with main's shared history just because the
    // bootstrap commit landed inside the correlation window.
    let mut args: Vec<String> = vec!["log".into(), branch.into()];
    for integration in MERGE_TARGETS {
        // `--not` excludes commits reachable from this ref. Silently
        // skipped when the ref doesn't exist (the command still
        // succeeds), which matches the behavior of scratch repos that
        // only carry `main`.
        let refname = format!("refs/heads/{integration}");
        if Command::new("git")
            .args(["show-ref", "--verify", "--quiet", &refname])
            .current_dir(repo)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
            && branch != *integration
        {
            args.push("--not".into());
            args.push((*integration).to_string());
        }
    }
    args.push("--since".into());
    args.push(since.to_rfc3339());
    args.push("--until".into());
    args.push(until.to_rfc3339());
    args.push("--pretty=format:1".into());

    let output = Command::new("git").args(&args).current_dir(repo).output();
    match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).lines().count() as u32,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn unknown_when_branch_empty() {
        let inputs = WorkOutcomeInputs {
            repo_root: Path::new("/"),
            branch: "",
            started_at: Utc::now(),
            ended_at: Utc::now(),
            grace: None,
        };
        assert_eq!(
            derive_work_outcome(&inputs).label,
            WorkOutcomeLabel::Unknown
        );
    }

    #[test]
    fn unknown_rationale_avoids_integration_branch_jargon() {
        // #445 acceptance: the fresh-user smoke pass found the original
        // rationale ("no non-integration branch on session — nothing
        // to correlate") was unreadable — a new user has no mental
        // model for what "integration branch" is and the wording
        // implied they should take an action when the outcome is
        // inherently un-derivable. Lock the jargon out.
        let inputs = WorkOutcomeInputs {
            repo_root: Path::new("/"),
            branch: "main",
            started_at: Utc::now(),
            ended_at: Utc::now(),
            grace: None,
        };
        let r = derive_work_outcome(&inputs).rationale;
        assert!(
            !r.contains("non-integration"),
            "rationale still contains `non-integration` jargon: {r}"
        );
        assert!(
            !r.contains("nothing to correlate"),
            "rationale still contains the `nothing to correlate` phrasing: {r}"
        );
        // Must still name the underlying reason.
        assert!(
            r.contains("feature branch") || r.contains("merge outcome"),
            "rationale no longer explains the reason: {r}"
        );
    }

    #[test]
    fn unknown_on_integration_branch() {
        // Includes `HEAD` (the detached-HEAD sentinel) so work-outcome
        // correlation agrees with the pipeline ticket extractor's
        // integration-branch set (#336). Under normal ingest the proxy
        // / JSONL path normalizes detached HEAD to empty, but a future
        // importer that lets the literal string through must not be
        // credited as a `branch_merged` via the merge-base fallback.
        for b in ["main", "master", "develop", "HEAD"] {
            let inputs = WorkOutcomeInputs {
                repo_root: Path::new("/"),
                branch: b,
                started_at: Utc::now(),
                ended_at: Utc::now(),
                grace: None,
            };
            assert_eq!(
                derive_work_outcome(&inputs).label,
                WorkOutcomeLabel::Unknown,
                "{b} must not correlate as work outcome"
            );
        }
    }

    #[test]
    fn is_integration_branch_is_shared_across_pipeline_and_work_outcome() {
        // Guard against the asymmetry called out in #336: if someone
        // narrows the work-outcome set to drop `HEAD`, the pipeline
        // ticket extractor would still skip HEAD while work-outcome
        // correlation would attempt to merge-base against it.
        for b in ["main", "master", "develop", "HEAD"] {
            assert!(
                crate::pipeline::is_integration_branch(b),
                "{b} must be treated as an integration branch"
            );
        }
        for b in ["PROJ-1-feature", "03-20-pava-2120_desc", "", "feature/x"] {
            assert!(
                !crate::pipeline::is_integration_branch(b),
                "{b} must not be treated as an integration branch"
            );
        }
    }

    #[test]
    fn labels_serialize_stable_values() {
        assert_eq!(WorkOutcomeLabel::Committed.as_str(), "committed");
        assert_eq!(WorkOutcomeLabel::BranchMerged.as_str(), "branch_merged");
        assert_eq!(WorkOutcomeLabel::NoCommit.as_str(), "no_commit");
        assert_eq!(WorkOutcomeLabel::Unknown.as_str(), "unknown");
    }

    // Integration tests below invoke the `git` binary against a scratch
    // repo. They are gated behind a helper that returns early if `git`
    // isn't available so CI images without it still pass.

    struct ScratchRepo {
        dir: PathBuf,
    }

    impl ScratchRepo {
        fn new(name: &str) -> Option<Self> {
            if !has_git_on_path() {
                return None;
            }
            let base = std::env::temp_dir()
                .join(format!("budi-work-outcome-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&base).ok()?;
            let ok = Command::new("git")
                .args(["init", "-q", "-b", "main"])
                .current_dir(&base)
                .status()
                .ok()?
                .success();
            if !ok {
                return None;
            }
            // Configure a local identity so commits don't fail on
            // hosts without a global git config.
            for (k, v) in [
                ("user.email", "test@example.invalid"),
                ("user.name", "Budi Test"),
                ("commit.gpgsign", "false"),
            ] {
                let _ = Command::new("git")
                    .args(["config", "--local", k, v])
                    .current_dir(&base)
                    .status();
            }
            Some(Self { dir: base })
        }

        fn run(&self, args: &[&str]) -> bool {
            Command::new("git")
                .args(args)
                .current_dir(&self.dir)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        }

        fn commit(&self, file: &str, content: &str, msg: &str) -> bool {
            std::fs::write(self.dir.join(file), content).is_ok()
                && self.run(&["add", file])
                && self.run(&["commit", "-q", "-m", msg])
        }
    }

    impl Drop for ScratchRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn committed_when_feature_branch_has_commits() {
        let Some(repo) = ScratchRepo::new("committed") else {
            return;
        };
        assert!(repo.commit("seed.txt", "seed", "initial"));
        assert!(repo.run(&["checkout", "-q", "-b", "PROJ-1-feature"]));
        let started = Utc::now() - chrono::Duration::minutes(1);
        assert!(repo.commit("work.txt", "hi", "work"));
        let ended = Utc::now();

        let inputs = WorkOutcomeInputs {
            repo_root: &repo.dir,
            branch: "PROJ-1-feature",
            started_at: started,
            ended_at: ended,
            grace: None,
        };
        let outcome = derive_work_outcome(&inputs);
        // Branch never merged into main, so label should be Committed.
        assert_eq!(outcome.label, WorkOutcomeLabel::Committed);
    }

    #[test]
    fn no_commit_when_branch_is_idle() {
        let Some(repo) = ScratchRepo::new("nocommit") else {
            return;
        };
        assert!(repo.commit("seed.txt", "seed", "initial"));
        assert!(repo.run(&["checkout", "-q", "-b", "PROJ-2-idle"]));

        let inputs = WorkOutcomeInputs {
            repo_root: &repo.dir,
            branch: "PROJ-2-idle",
            started_at: Utc::now() - chrono::Duration::hours(1),
            ended_at: Utc::now(),
            grace: Some(chrono::Duration::seconds(1)),
        };
        let outcome = derive_work_outcome(&inputs);
        assert_eq!(outcome.label, WorkOutcomeLabel::NoCommit);
    }

    #[test]
    fn branch_merged_when_reachable_from_integration() {
        let Some(repo) = ScratchRepo::new("merged") else {
            return;
        };
        assert!(repo.commit("seed.txt", "seed", "initial"));
        assert!(repo.run(&["checkout", "-q", "-b", "PROJ-3-merged"]));
        let started = Utc::now() - chrono::Duration::minutes(1);
        assert!(repo.commit("merged.txt", "hi", "merged work"));
        assert!(repo.run(&["checkout", "-q", "main"]));
        assert!(repo.run(&["merge", "-q", "--no-ff", "PROJ-3-merged", "-m", "merge"]));
        let ended = Utc::now();

        let inputs = WorkOutcomeInputs {
            repo_root: &repo.dir,
            branch: "PROJ-3-merged",
            started_at: started,
            ended_at: ended,
            grace: None,
        };
        let outcome = derive_work_outcome(&inputs);
        assert_eq!(outcome.label, WorkOutcomeLabel::BranchMerged);
    }
}
