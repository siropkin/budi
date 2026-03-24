//! Git enrichment: extract commits, PR references, author info, and diff stats
//! for sessions that have a valid project directory.

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

use anyhow::Result;
use rusqlite::{Connection, params};

/// A git commit associated with a session.
#[derive(Debug, Clone, serde::Serialize)]
pub struct GitCommit {
    pub hash: String,
    pub author_name: String,
    pub author_email: String,
    pub timestamp: String,
    pub message: String,
    pub lines_added: i64,
    pub lines_removed: i64,
    pub pr_number: Option<i64>,
}

/// Enrich sessions with git commit data.
/// Finds sessions that have a project_dir and haven't been enriched since their last update.
/// Returns the number of commits inserted.
pub fn enrich_git_commits(conn: &mut Connection) -> Result<usize> {
    // Find sessions needing enrichment: have project_dir, and either never enriched
    // or enriched before last_seen changed.
    let sessions: Vec<(String, String, String, String, Option<String>)> = {
        let mut stmt = conn.prepare(
            "SELECT session_id, project_dir, first_seen, last_seen, git_branch
             FROM sessions
             WHERE project_dir IS NOT NULL
               AND (git_enriched_at IS NULL OR git_enriched_at < last_seen)",
        )?;
        stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect()
    };

    if sessions.is_empty() {
        return Ok(0);
    }

    let tx = conn.transaction()?;
    let mut total_commits = 0;

    for (session_id, project_dir, first_seen, last_seen, git_branch) in &sessions {
        let dir = Path::new(project_dir);
        if !dir.is_dir() {
            continue;
        }

        // Check if it's a git repo
        if !dir.join(".git").exists() && !is_inside_git_repo(dir) {
            continue;
        }

        // Delete existing commits for this session (re-enrich)
        tx.execute(
            "DELETE FROM commits WHERE session_id = ?1",
            params![session_id],
        )?;

        // Get commits during the session time range
        let commits = git_log_commits(dir, first_seen, last_seen, git_branch.as_deref());
        for commit in &commits {
            tx.execute(
                "INSERT INTO commits (session_id, hash, author_name, author_email, timestamp, message, lines_added, lines_removed, pr_number)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    session_id,
                    commit.hash,
                    commit.author_name,
                    commit.author_email,
                    commit.timestamp,
                    commit.message,
                    commit.lines_added,
                    commit.lines_removed,
                    commit.pr_number,
                ],
            )?;
            total_commits += 1;
        }

        // Get git author for the repo
        let (author_name, author_email) = git_author(dir);

        // Update session with author info and enrichment timestamp
        tx.execute(
            "UPDATE sessions SET git_author_name = COALESCE(?1, git_author_name),
                                 git_author_email = COALESCE(?2, git_author_email),
                                 git_enriched_at = ?3
             WHERE session_id = ?4",
            params![author_name, author_email, last_seen, session_id],
        )?;
    }

    tx.commit()?;
    Ok(total_commits)
}

/// Check if a directory is inside a git repository (for repos where .git is in a parent).
fn is_inside_git_repo(dir: &Path) -> bool {
    Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(dir)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run git log to find commits made during a session's time range.
fn git_log_commits(
    dir: &Path,
    since: &str,
    until: &str,
    branch: Option<&str>,
) -> Vec<GitCommit> {
    // Format: hash<SEP>author_name<SEP>author_email<SEP>ISO timestamp<SEP>subject
    // Followed by numstat lines (added\tremoved\tfile)
    let format = "%H\x1f%an\x1f%ae\x1f%aI\x1f%s";
    let mut args = vec![
        "log".to_string(),
        format!("--since={}", since),
        format!("--until={}", until),
        format!("--format={}", format),
        "--numstat".to_string(),
    ];

    // If branch is specified, use it to scope commits
    if let Some(br) = branch {
        let branch_name = br.strip_prefix("refs/heads/").unwrap_or(br);
        args.push(branch_name.to_string());
    }

    let output = match Command::new("git")
        .args(&args)
        .current_dir(dir)
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return Vec::new(),
    };

    parse_git_log_output(&output)
}

/// Parse the output of git log --format=<hash>\x1f<name>\x1f<email>\x1f<date>\x1f<subject> --numstat
fn parse_git_log_output(output: &str) -> Vec<GitCommit> {
    let mut commits = Vec::new();
    let mut seen_hashes = HashSet::new();
    let mut lines = output.lines().peekable();

    while let Some(line) = lines.next() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let parts: Vec<&str> = line.splitn(5, '\x1f').collect();
        if parts.len() < 5 {
            continue;
        }

        let hash = parts[0].to_string();
        if !looks_like_hash(&hash) || seen_hashes.contains(&hash) {
            continue;
        }
        seen_hashes.insert(hash.clone());

        let author_name = parts[1].to_string();
        let author_email = parts[2].to_string();
        let timestamp = parts[3].to_string();
        let message = parts[4].to_string();

        // Parse numstat lines that follow
        let mut lines_added: i64 = 0;
        let mut lines_removed: i64 = 0;
        while let Some(stat_line) = lines.peek() {
            let stat_line = stat_line.trim();
            if stat_line.is_empty() {
                lines.next();
                continue;
            }
            // numstat format: "added\tremoved\tfilename" or "-\t-\tbinary"
            let stat_parts: Vec<&str> = stat_line.split('\t').collect();
            if stat_parts.len() >= 2 {
                if let (Ok(a), Ok(r)) = (stat_parts[0].parse::<i64>(), stat_parts[1].parse::<i64>())
                {
                    lines_added += a;
                    lines_removed += r;
                    lines.next();
                    continue;
                }
            }
            // If it doesn't look like numstat, it's the next commit
            break;
        }

        let pr_number = extract_pr_number(&message);

        commits.push(GitCommit {
            hash,
            author_name,
            author_email,
            timestamp,
            message,
            lines_added,
            lines_removed,
            pr_number,
        });
    }

    commits
}

/// Check if a string looks like a git commit hash (40 hex chars).
fn looks_like_hash(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Extract a PR number from a commit message.
/// Patterns: "Merge pull request #123", "(#456)", "#789"
pub fn extract_pr_number(message: &str) -> Option<i64> {
    let bytes = message.as_bytes();
    let len = bytes.len();

    // Pattern 1: "Merge pull request #N"
    if let Some(pos) = message.find("pull request #") {
        let start = pos + "pull request #".len();
        return parse_number_at(message, start);
    }

    // Pattern 2: "(#N)" — conventional commit style
    let mut i = 0;
    while i < len {
        if bytes[i] == b'(' && i + 2 < len && bytes[i + 1] == b'#' {
            if let Some(n) = parse_number_at(message, i + 2) {
                return Some(n);
            }
        }
        i += 1;
    }

    // Pattern 3: standalone "#N" (not preceded by &)
    i = 0;
    while i < len {
        if bytes[i] == b'#' && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric()) {
            if let Some(n) = parse_number_at(message, i + 1) {
                return Some(n);
            }
        }
        i += 1;
    }

    None
}

/// Parse a number starting at a given position in the string.
fn parse_number_at(s: &str, start: usize) -> Option<i64> {
    let bytes = s.as_bytes();
    let mut end = start;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if end > start {
        s[start..end].parse().ok()
    } else {
        None
    }
}

/// Get the git author name and email configured for a repository.
fn git_author(dir: &Path) -> (Option<String>, Option<String>) {
    let name = Command::new("git")
        .args(["config", "user.name"])
        .current_dir(dir)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());

    let email = Command::new("git")
        .args(["config", "user.email"])
        .current_dir(dir)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());

    (name, email)
}

/// Query commits for a given session.
pub fn commits_for_session(conn: &Connection, session_id: &str) -> Result<Vec<GitCommit>> {
    let mut stmt = conn.prepare(
        "SELECT hash, author_name, author_email, timestamp, message,
                lines_added, lines_removed, pr_number
         FROM commits WHERE session_id = ?1
         ORDER BY timestamp ASC",
    )?;
    let rows = stmt
        .query_map(params![session_id], |row| {
            Ok(GitCommit {
                hash: row.get(0)?,
                author_name: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                author_email: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                timestamp: row.get(3)?,
                message: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                lines_added: row.get(5)?,
                lines_removed: row.get(6)?,
                pr_number: row.get(7)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

/// Aggregate git stats across all sessions in a date range.
#[derive(Debug, Clone, serde::Serialize)]
pub struct GitSummary {
    pub total_commits: u64,
    pub total_lines_added: i64,
    pub total_lines_removed: i64,
    pub unique_prs: u64,
    pub sessions_with_commits: u64,
    pub top_authors: Vec<(String, u64)>,
}

pub fn git_summary(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<GitSummary> {
    let mut conditions = Vec::new();
    let mut param_values: Vec<String> = Vec::new();

    if let Some(s) = since {
        param_values.push(s.to_string());
        conditions.push(format!("c.timestamp >= ?{}", param_values.len()));
    }
    if let Some(u) = until {
        param_values.push(u.to_string());
        conditions.push(format!("c.timestamp < ?{}", param_values.len()));
    }

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    let sql = format!(
        "SELECT COUNT(*),
                COALESCE(SUM(lines_added), 0),
                COALESCE(SUM(lines_removed), 0),
                COUNT(DISTINCT CASE WHEN pr_number IS NOT NULL THEN pr_number END),
                COUNT(DISTINCT session_id)
         FROM commits c {}",
        where_clause
    );

    let (total_commits, total_lines_added, total_lines_removed, unique_prs, sessions_with_commits): (u64, i64, i64, u64, u64) =
        conn.query_row(&sql, param_refs.as_slice(), |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?;

    // Top authors
    let authors_sql = format!(
        "SELECT author_name, COUNT(*) as cnt
         FROM commits c {}
         GROUP BY author_name
         ORDER BY cnt DESC
         LIMIT 10",
        where_clause
    );
    let mut stmt = conn.prepare(&authors_sql)?;
    let top_authors: Vec<(String, u64)> = stmt
        .query_map(param_refs.as_slice(), |row| Ok((row.get(0)?, row.get(1)?)))?
        .filter_map(|r| r.ok())
        .collect();

    Ok(GitSummary {
        total_commits,
        total_lines_added,
        total_lines_removed,
        unique_prs,
        sessions_with_commits,
        top_authors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_pr_merge_commit() {
        assert_eq!(
            extract_pr_number("Merge pull request #456 from user/branch"),
            Some(456)
        );
    }

    #[test]
    fn extract_pr_conventional() {
        assert_eq!(
            extract_pr_number("fix: resolve auth bug (#789)"),
            Some(789)
        );
    }

    #[test]
    fn extract_pr_hash_ref() {
        assert_eq!(extract_pr_number("Fix login issue #123"), Some(123));
    }

    #[test]
    fn extract_pr_none() {
        assert_eq!(extract_pr_number("Regular commit message"), None);
        assert_eq!(extract_pr_number("No number here"), None);
    }

    #[test]
    fn parse_git_log_basic() {
        let output = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2\x1fJohn\x1fjohn@example.com\x1f2026-03-20T10:00:00-07:00\x1fFix bug (#42)\n5\t3\tsrc/main.rs\n2\t0\tREADME.md\n";
        let commits = parse_git_log_output(output);
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].author_name, "John");
        assert_eq!(commits[0].lines_added, 7);
        assert_eq!(commits[0].lines_removed, 3);
        assert_eq!(commits[0].pr_number, Some(42));
    }

    #[test]
    fn parse_git_log_multiple_commits() {
        let output = "\
a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2\x1fAlice\x1falice@test.com\x1f2026-03-20T10:00:00Z\x1fFirst commit
3\t1\tfile.rs

b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3\x1fBob\x1fbob@test.com\x1f2026-03-20T11:00:00Z\x1fSecond commit (#10)
1\t0\tother.rs
";
        let commits = parse_git_log_output(output);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].message, "First commit");
        assert_eq!(commits[0].pr_number, None);
        assert_eq!(commits[1].message, "Second commit (#10)");
        assert_eq!(commits[1].pr_number, Some(10));
    }

    #[test]
    fn parse_git_log_empty() {
        assert!(parse_git_log_output("").is_empty());
        assert!(parse_git_log_output("\n\n").is_empty());
    }

    #[test]
    fn looks_like_hash_valid() {
        assert!(looks_like_hash(
            "abc123abc123abc123abc123abc123abc123abcd"
        ));
        assert!(!looks_like_hash("short"));
        assert!(!looks_like_hash("toolongtobeahashxyz123abc123abc123abc123abc123"));
    }

    #[test]
    fn commits_for_session_empty_db() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::analytics::migrate_for_test(&conn);
        let commits = commits_for_session(&conn, "nonexistent").unwrap();
        assert!(commits.is_empty());
    }

    #[test]
    fn git_summary_empty_db() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::analytics::migrate_for_test(&conn);
        let summary = git_summary(&conn, None, None).unwrap();
        assert_eq!(summary.total_commits, 0);
        assert_eq!(summary.unique_prs, 0);
    }
}
