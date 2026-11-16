//! Where the checked-out branch is pushed, and how much a range of commits changes.

use gix::object::tree::diff::ChangeDetached;

use crate::git_repo::GitRepo;
use crate::git_repo::count_lines;

/// What a range of commits changes, as `git diff --shortstat base head` counts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiffStats {
    pub files: i64,
    pub added: i64,
    pub removed: i64,
}

impl GitRepo {
    /// The remote the checked-out branch pushes to: its `pushRemote`, else `remote.pushDefault`,
    /// else its `remote`, else `origin`.
    pub fn push_remote(&self) -> String {
        self.repo
            .head_name()
            .ok()
            .flatten()
            .filter(|name| name.category() == Some(gix::reference::Category::LocalBranch))
            .and_then(|name| {
                self.repo
                    .branch_remote_name(name.shorten(), gix::remote::Direction::Push)
                    .map(|remote| remote.as_bstr().to_string())
            })
            .unwrap_or_else(|| "origin".to_owned())
    }

    /// The commit `remote`'s `branch` was at when it was last fetched or pushed
    /// (`refs/remotes/<remote>/<branch>`).
    pub fn tracking_commit(&self, remote: &str, branch: &str) -> Option<String> {
        let mut reference = self
            .repo
            .find_reference(&format!("refs/remotes/{remote}/{branch}"))
            .ok()?;
        reference.peel_to_id().ok().map(|id| id.to_string())
    }

    /// What the commits from `base` to `head` change. A binary file counts as a changed file
    /// without lines, and a rename without edits as no change.
    pub fn range_stats(&self, base: &str, head: &str) -> Option<DiffStats> {
        let tree = |rev: &str| {
            self.repo
                .rev_parse_single(rev)
                .ok()?
                .object()
                .ok()?
                .peel_to_tree()
                .ok()
        };
        let (old, new) = (tree(base)?, tree(head)?);
        let changes = self.repo.diff_tree_to_tree(&old, &new, None).ok()?;
        let blob = |mode: gix::object::tree::EntryMode, id: &gix::ObjectId| {
            mode.is_blob()
                .then(|| self.repo.find_object(*id).ok())
                .flatten()
                .map(|object| object.data.clone())
        };
        let mut stats = DiffStats::default();
        for change in &changes {
            let (old, new) = match change {
                ChangeDetached::Addition { entry_mode, id, .. } => (None, blob(*entry_mode, id)),
                ChangeDetached::Deletion { entry_mode, id, .. } => (blob(*entry_mode, id), None),
                ChangeDetached::Modification {
                    previous_entry_mode,
                    previous_id,
                    entry_mode,
                    id,
                    ..
                } => (
                    blob(*previous_entry_mode, previous_id),
                    blob(*entry_mode, id),
                ),
                ChangeDetached::Rewrite {
                    source_entry_mode,
                    source_id,
                    entry_mode,
                    id,
                    ..
                } => {
                    if source_id == id {
                        continue;
                    }
                    (blob(*source_entry_mode, source_id), blob(*entry_mode, id))
                }
            };
            if old.is_none() && new.is_none() {
                // A directory or a submodule.
                continue;
            }
            let (added, removed) = count_lines(
                old.as_deref().unwrap_or_default(),
                new.as_deref().unwrap_or_default(),
            );
            stats.files += 1;
            stats.added += i64::from(added);
            stats.removed += i64::from(removed);
        }
        Some(stats)
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
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    #[test]
    fn a_range_counts_files_and_lines_like_shortstat() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path();
        git(dir, &["init", "-q", "-b", "main"]);
        git(dir, &["config", "user.name", "T"]);
        git(dir, &["config", "user.email", "t@example.com"]);
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").expect("write");
        std::fs::write(dir.join("gone.txt"), "x\ny\n").expect("write");
        git(dir, &["add", "."]);
        git(dir, &["commit", "-q", "-m", "base"]);
        std::fs::write(dir.join("a.txt"), "one\n2\nthree\n").expect("edit");
        std::fs::remove_file(dir.join("gone.txt")).expect("remove");
        std::fs::create_dir(dir.join("sub")).expect("dir");
        std::fs::write(dir.join("sub/new.txt"), "n\n").expect("write");
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", "change"]);

        let repo = GitRepo::discover(dir).expect("repo");
        assert_eq!(
            repo.range_stats("HEAD~1", "HEAD"),
            Some(DiffStats {
                files: 3,
                added: 3,
                removed: 3
            })
        );
        assert_eq!(
            git(dir, &["diff", "--shortstat", "HEAD~1", "HEAD"]),
            "3 files changed, 3 insertions(+), 3 deletions(-)"
        );
    }

    #[test]
    fn the_push_remote_and_tracking_commit_come_from_the_branch() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path();
        git(dir, &["init", "-q", "-b", "topic"]);
        git(dir, &["config", "user.name", "T"]);
        git(dir, &["config", "user.email", "t@example.com"]);
        git(dir, &["commit", "-q", "--allow-empty", "-m", "base"]);
        let repo = GitRepo::discover(dir).expect("repo");
        assert_eq!(repo.push_remote(), "origin");
        assert_eq!(repo.tracking_commit("fork", "topic"), None);

        git(dir, &["config", "branch.topic.remote", "fork"]);
        let head = git(dir, &["rev-parse", "HEAD"]);
        git(dir, &["update-ref", "refs/remotes/fork/topic", &head]);
        let repo = GitRepo::discover(dir).expect("repo");
        assert_eq!(repo.push_remote(), "fork");
        assert_eq!(repo.tracking_commit("fork", "topic"), Some(head));
    }
}
