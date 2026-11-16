//! Landing changes as commits, in-process.
//!
//! An agent given a prompt's worth of changes should be able to land them as a series of small,
//! focused commits. This is the mechanism: list what changed, then commit exactly the paths of
//! one batch with a message, leaving everything else for the next. No `git` process runs, so a
//! batch costs milliseconds, and git hooks do not run (a hook is a policy of the caller).

use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use gix::bstr::BString;
use gix::object::tree::EntryKind;
use gix::refs::transaction::PreviousValue;

use crate::git_repo::GitRepo;

/// How a path differs from HEAD.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeState {
    /// Tracked, and its content or mode changed.
    Modified,
    /// Not in HEAD: new, whether staged or untracked.
    Added,
    /// In HEAD, gone from the working tree.
    Deleted,
}

/// One changed path, relative to the working tree root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    pub path: String,
    pub state: ChangeState,
}

#[derive(Debug, thiserror::Error)]
pub enum CommitError {
    #[error("not inside a git working tree")]
    NoWorkTree,
    #[error("`{0}` is not a path inside the repository")]
    BadPath(String),
    #[error("`{0}` is a directory; list its files")]
    Directory(String),
    #[error("none of those paths differ from HEAD")]
    NothingToCommit,
    #[error("git identity is not configured: set user.name and user.email")]
    NoIdentity,
    #[error("{0}")]
    Git(String),
}

fn git_error(error: impl std::fmt::Display) -> CommitError {
    CommitError::Git(error.to_string())
}

impl GitRepo {
    /// Every path that differs from HEAD, staged, unstaged or untracked, sorted by path.
    pub fn changed_files(&self) -> Vec<ChangedFile> {
        let Some(workdir) = self.repo.workdir().map(Path::to_path_buf) else {
            return Vec::new();
        };
        let Ok(platform) = self.repo.status(gix::progress::Discard) else {
            return Vec::new();
        };
        let Ok(items) = platform
            .untracked_files(gix::status::UntrackedFiles::Files)
            .into_iter(Vec::<BString>::new())
        else {
            return Vec::new();
        };
        let mut paths = std::collections::BTreeSet::new();
        for item in items.flatten() {
            match item {
                gix::status::Item::TreeIndex(change) => {
                    paths.insert(change.location().to_owned());
                }
                gix::status::Item::IndexWorktree(
                    gix::status::index_worktree::Item::Modification { rela_path, .. },
                ) => {
                    paths.insert(rela_path);
                }
                gix::status::Item::IndexWorktree(
                    gix::status::index_worktree::Item::DirectoryContents { entry, .. },
                ) if entry.status == gix::dir::entry::Status::Untracked
                    && entry.disk_kind == Some(gix::dir::entry::Kind::File) =>
                {
                    paths.insert(entry.rela_path.clone());
                }
                _ => {}
            }
        }
        let head_tree = self.repo.head_tree().ok();
        paths
            .into_iter()
            .filter_map(|path| {
                let text = path.to_string();
                let exists = workdir.join(&text).symlink_metadata().is_ok();
                let in_head = head_tree.as_ref().is_some_and(|tree| {
                    tree.lookup_entry_by_path(&text)
                        .ok()
                        .flatten()
                        .is_some_and(|entry| !entry.mode().is_tree())
                });
                let state = match (in_head, exists) {
                    (true, true) => ChangeState::Modified,
                    (true, false) => ChangeState::Deleted,
                    (false, true) => ChangeState::Added,
                    (false, false) => return None,
                };
                Some(ChangedFile { path: text, state })
            })
            .collect()
    }

    /// Commit the working tree's version of exactly `paths` on top of HEAD, as one commit with
    /// `message`, and return the new commit's id.
    ///
    /// A path that no longer exists is recorded as a deletion. Nothing else is touched in the
    /// working tree; afterwards the index matches the new HEAD, so whatever was not committed
    /// shows as unstaged. Uses the author and committer git is configured with.
    pub fn commit_paths(&self, paths: &[String], message: &str) -> Result<String, CommitError> {
        let workdir = self
            .repo
            .workdir()
            .ok_or(CommitError::NoWorkTree)?
            .to_path_buf();
        // Identity and hooks-free config come from the full configuration, not the isolated one
        // used for reads.
        let repo = gix::discover(&workdir).map_err(git_error)?;
        if repo.author().is_none() || repo.committer().is_none() {
            return Err(CommitError::NoIdentity);
        }

        let parent = repo.head_id().ok().map(gix::Id::detach);
        let base_tree = match &parent {
            Some(_) => repo.head_tree_id().map_err(git_error)?.detach(),
            None => repo.empty_tree().id,
        };
        let mut editor = repo.edit_tree(base_tree).map_err(git_error)?;
        for path in paths {
            let relative = checked_relative(path)?;
            let full = workdir.join(&relative);
            match full.symlink_metadata() {
                Ok(meta) if meta.is_dir() => return Err(CommitError::Directory(path.clone())),
                Ok(meta) => {
                    let (kind, bytes) = if meta.file_type().is_symlink() {
                        let target = std::fs::read_link(&full).map_err(git_error)?;
                        let target = gix::path::into_bstr(target).map_err(git_error)?;
                        (EntryKind::Link, target.into_owned().to_vec())
                    } else {
                        let bytes = std::fs::read(&full).map_err(git_error)?;
                        (blob_kind(&meta), bytes)
                    };
                    let blob = repo.write_blob(&bytes).map_err(git_error)?.detach();
                    editor
                        .upsert(path.as_str(), kind, blob)
                        .map_err(git_error)?;
                }
                Err(_) => {
                    editor.remove(path.as_str()).map_err(git_error)?;
                }
            }
        }
        let tree = editor.write().map_err(git_error)?.detach();
        if tree == base_tree {
            return Err(CommitError::NothingToCommit);
        }

        let id = repo
            .commit("HEAD", message, tree, parent)
            .map_err(git_error)?
            .detach();
        let _ = PreviousValue::Any;

        // Leave the index describing the new HEAD, so only uncommitted work reads as changed.
        let mut index = repo.index_from_tree(&tree).map_err(git_error)?;
        index.write(Default::default()).map_err(git_error)?;
        Ok(id.to_string())
    }
}

fn blob_kind(meta: &std::fs::Metadata) -> EntryKind {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 != 0 {
            return EntryKind::BlobExecutable;
        }
    }
    let _ = meta;
    EntryKind::Blob
}

/// `path` as a relative path with no `..` and no root, or an error.
fn checked_relative(path: &str) -> Result<PathBuf, CommitError> {
    let candidate = Path::new(path);
    let clean = !path.is_empty()
        && candidate
            .components()
            .all(|part| matches!(part, Component::Normal(_)));
    if clean {
        Ok(candidate.to_path_buf())
    } else {
        Err(CommitError::BadPath(path.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::process::Command;

    /// `git` is used here only as the judge of what we wrote.
    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).expect("utf8")
    }

    fn repo_with_base() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().canonicalize().expect("canonical");
        git(&dir, &["init", "-q", "-b", "main"]);
        git(&dir, &["config", "user.name", "Test"]);
        git(&dir, &["config", "user.email", "test@example.com"]);
        std::fs::write(dir.join("a.txt"), "a\n").expect("a");
        std::fs::write(dir.join("b.txt"), "b\n").expect("b");
        std::fs::write(dir.join("gone.txt"), "gone\n").expect("gone");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-q", "-m", "base"]);
        (temp, dir)
    }

    fn open(dir: &Path) -> GitRepo {
        GitRepo::discover(dir).expect("repo")
    }

    #[test]
    fn lists_what_changed_with_its_state() {
        let (_temp, dir) = repo_with_base();
        std::fs::write(dir.join("a.txt"), "changed\n").expect("edit");
        std::fs::remove_file(dir.join("gone.txt")).expect("rm");
        std::fs::create_dir_all(dir.join("new")).expect("mkdir");
        std::fs::write(dir.join("new/file.rs"), "fn main() {}\n").expect("new");

        let changed = open(&dir).changed_files();
        let summary: Vec<(&str, ChangeState)> =
            changed.iter().map(|c| (c.path.as_str(), c.state)).collect();
        assert_eq!(
            summary,
            vec![
                ("a.txt", ChangeState::Modified),
                ("gone.txt", ChangeState::Deleted),
                ("new/file.rs", ChangeState::Added),
            ]
        );
    }

    #[test]
    fn commits_exactly_the_chosen_paths_and_leaves_the_rest() {
        let (_temp, dir) = repo_with_base();
        std::fs::write(dir.join("a.txt"), "changed\n").expect("edit a");
        std::fs::write(dir.join("b.txt"), "changed too\n").expect("edit b");
        std::fs::write(dir.join("new.txt"), "brand new\n").expect("new");
        let repo = open(&dir);

        let first = repo
            .commit_paths(
                &["a.txt".into(), "new.txt".into()],
                "feat: change a, add new",
            )
            .expect("commit");

        assert_eq!(git(&dir, &["rev-parse", "HEAD"]).trim(), first);
        assert_eq!(
            git(&dir, &["log", "-1", "--format=%an <%ae>|%s"]).trim(),
            "Test <test@example.com>|feat: change a, add new"
        );
        let touched = git(&dir, &["show", "--stat", "--format=", "HEAD"]);
        assert!(
            touched.contains("a.txt") && touched.contains("new.txt"),
            "{touched}"
        );
        assert!(
            !touched.contains("b.txt"),
            "b.txt must not be in the commit: {touched}"
        );
        // What was left over is exactly b.txt, still modified in the working tree.
        assert_eq!(git(&dir, &["status", "--porcelain"]).trim(), "M b.txt");
        assert_eq!(
            std::fs::read_to_string(dir.join("b.txt")).expect("b"),
            "changed too\n"
        );

        // The second batch lands the rest as its own commit.
        repo.commit_paths(&["b.txt".into()], "fix: change b")
            .expect("second");
        assert_eq!(git(&dir, &["status", "--porcelain"]).trim(), "");
        assert_eq!(git(&dir, &["rev-list", "--count", "HEAD"]).trim(), "3");
        git(&dir, &["fsck", "--strict"]);
    }

    #[test]
    fn a_missing_path_is_committed_as_a_deletion() {
        let (_temp, dir) = repo_with_base();
        std::fs::remove_file(dir.join("gone.txt")).expect("rm");

        open(&dir)
            .commit_paths(&["gone.txt".into()], "chore: drop gone")
            .expect("commit");

        assert_eq!(git(&dir, &["status", "--porcelain"]).trim(), "");
        assert!(
            git(&dir, &["ls-tree", "-r", "--name-only", "HEAD"])
                .lines()
                .all(|l| l != "gone.txt")
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_executable_bit_survives_a_commit() {
        use std::os::unix::fs::PermissionsExt;
        let (_temp, dir) = repo_with_base();
        std::fs::write(dir.join("run.sh"), "#!/bin/sh\n").expect("script");
        std::fs::set_permissions(dir.join("run.sh"), std::fs::Permissions::from_mode(0o755))
            .expect("chmod");

        open(&dir)
            .commit_paths(&["run.sh".into()], "add script")
            .expect("commit");

        assert!(git(&dir, &["ls-tree", "HEAD", "run.sh"]).starts_with("100755"));
        assert_eq!(git(&dir, &["status", "--porcelain"]).trim(), "");
    }

    #[test]
    fn the_first_commit_in_an_empty_repository_works() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().canonicalize().expect("canonical");
        git(&dir, &["init", "-q", "-b", "main"]);
        git(&dir, &["config", "user.name", "Test"]);
        git(&dir, &["config", "user.email", "test@example.com"]);
        std::fs::write(dir.join("first.txt"), "hello\n").expect("file");

        open(&dir)
            .commit_paths(&["first.txt".into()], "initial")
            .expect("commit");

        assert_eq!(git(&dir, &["log", "--format=%s"]).trim(), "initial");
        assert_eq!(git(&dir, &["status", "--porcelain"]).trim(), "");
        git(&dir, &["fsck", "--strict"]);
    }

    #[test]
    fn refuses_what_it_cannot_commit_safely() {
        let (_temp, dir) = repo_with_base();
        let repo = open(&dir);
        std::fs::create_dir_all(dir.join("d")).expect("dir");

        assert!(matches!(
            repo.commit_paths(&["../escape".into()], "x"),
            Err(CommitError::BadPath(_))
        ));
        assert!(matches!(
            repo.commit_paths(&["/etc/passwd".into()], "x"),
            Err(CommitError::BadPath(_))
        ));
        assert!(matches!(
            repo.commit_paths(&["d".into()], "x"),
            Err(CommitError::Directory(_))
        ));
        assert!(matches!(
            repo.commit_paths(&["a.txt".into()], "x"),
            Err(CommitError::NothingToCommit)
        ));
        assert_eq!(
            git(&dir, &["rev-list", "--count", "HEAD"]).trim(),
            "1",
            "nothing was committed"
        );
    }
}
