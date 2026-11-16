//! The recent history of a repository, read in-process.

use crate::git_repo::GitRepo;

/// One commit, as `git log --oneline` would summarize it plus its author and time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitSummary {
    /// Full hex id.
    pub id: String,
    /// The message's first paragraph, joined into one line.
    pub summary: String,
    pub author: String,
    /// Seconds since the Unix epoch, as the committer recorded it.
    pub time: i64,
}

impl GitRepo {
    /// Up to `limit` commits reachable from HEAD, newest first; empty before the first commit.
    pub fn recent_commits(&self, limit: i64) -> Vec<CommitSummary> {
        let Ok(head) = self.repo.head_id() else {
            return Vec::new();
        };
        // Breadth-first from HEAD: newest first along every line of history.
        let Ok(walk) = head.ancestors().all() else {
            return Vec::new();
        };
        walk.filter_map(Result::ok)
            .take(usize::try_from(limit.max(0)).unwrap_or_default())
            .filter_map(|info| {
                let commit = info.object().ok()?;
                let summary = commit.message().ok()?.summary().to_string();
                let author = commit.author().ok()?.name.to_string();
                let time = commit.time().ok()?.seconds;
                Some(CommitSummary {
                    id: info.id.to_string(),
                    summary,
                    author,
                    time,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn git(dir: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_DATE", "2026-01-02T03:04:05Z")
            .env("GIT_COMMITTER_DATE", "2026-01-02T03:04:05Z")
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8(out.stdout).expect("utf8")
    }

    #[test]
    fn recent_commits_are_newest_first_and_limited() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path();
        git(dir, &["init", "-q", "-b", "main"]);
        git(dir, &["config", "user.name", "Ada"]);
        git(dir, &["config", "user.email", "ada@example.com"]);
        let repo = GitRepo::discover(dir).expect("repo");
        assert_eq!(repo.recent_commits(5), Vec::new());

        for (file, message) in [("a", "first\n\nbody"), ("b", "second"), ("c", "third")] {
            std::fs::write(dir.join(file), file).expect("write");
            git(dir, &["add", file]);
            git(dir, &["commit", "-q", "-m", message]);
        }
        let repo = GitRepo::discover(dir).expect("repo");
        let commits = repo.recent_commits(2);
        assert_eq!(
            commits
                .iter()
                .map(|commit| (commit.summary.as_str(), commit.author.as_str(), commit.time))
                .collect::<Vec<_>>(),
            vec![
                ("third", "Ada", 1_767_323_045),
                ("second", "Ada", 1_767_323_045)
            ]
        );
        assert_eq!(commits[0].id, git(dir, &["rev-parse", "HEAD"]).trim());
        assert_eq!(repo.recent_commits(10).len(), 3);
        assert_eq!(repo.recent_commits(10)[2].summary, "first");
    }
}
