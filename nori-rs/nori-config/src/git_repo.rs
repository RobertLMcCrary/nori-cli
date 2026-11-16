//! In-process answers to the questions Nori asks of a git repository.
//!
//! Session start, an agent switch and the footer all want the same handful of facts: where the
//! working tree is, which branch and commit are checked out, what the remote is. Each used to be
//! a `git` process, tens of milliseconds apiece and a dependency on the binary being installed.
//! They are read here with `gix` instead.

use std::path::Path;
use std::path::PathBuf;

/// An open repository.
pub struct GitRepo {
    repo: gix::Repository,
}

impl GitRepo {
    /// The repository `path` is in, found by searching upwards like `git` does.
    ///
    /// Only the repository's own configuration is read, never the system or user files.
    pub fn discover(path: &Path) -> Option<Self> {
        let start = if path.is_dir() { path } else { path.parent()? };
        let repo =
            gix::discover_opts(start, Default::default(), gix::open::Options::isolated()).ok()?;
        Some(Self { repo })
    }

    /// The working tree's root (`git rev-parse --show-toplevel`); `None` for a bare repository.
    pub fn work_dir(&self) -> Option<PathBuf> {
        self.repo.workdir().map(Path::to_path_buf)
    }

    /// This working tree's git directory (`--git-dir`).
    pub fn git_dir(&self) -> &Path {
        self.repo.git_dir()
    }

    /// The git directory shared by every worktree (`--git-common-dir`).
    pub fn common_dir(&self) -> &Path {
        self.repo.common_dir()
    }

    /// Whether this is a linked worktree rather than the main working tree.
    pub fn is_linked_worktree(&self) -> bool {
        self.repo.kind() == gix::repository::Kind::LinkedWorkTree
    }

    /// The checked-out branch's short name; `None` when HEAD is detached or has no commit yet.
    pub fn branch(&self) -> Option<String> {
        self.head_commit()?;
        let name = self.repo.head_name().ok().flatten()?;
        (name.category() == Some(gix::reference::Category::LocalBranch))
            .then(|| name.shorten().to_string())
    }

    /// The commit HEAD points at, as full hex; `None` before the first commit.
    pub fn head_commit(&self) -> Option<String> {
        self.repo.head_id().ok().map(|id| id.to_string())
    }

    /// The URL of `remote`, with `url.<base>.insteadOf` rewrites applied like
    /// `git remote get-url`.
    ///
    /// The repository is reopened with the user's configuration for this one question, because
    /// those rewrites live there and the project id hashes the result. A different answer would
    /// quietly give an existing project a new identity.
    pub fn remote_url(&self, remote: &str) -> Option<String> {
        let work = self.repo.workdir().unwrap_or_else(|| self.repo.git_dir());
        let repo = gix::discover(work).ok()?;
        let remote = repo.find_remote(remote).ok()?;
        let url = remote.url(gix::remote::Direction::Fetch)?;
        let text = url.to_bstring().to_string();
        (!text.is_empty()).then_some(text)
    }

    /// The branch the remote's HEAD points at (`refs/remotes/<remote>/HEAD`), e.g. `main`.
    pub fn remote_default_branch(&self, remote: &str) -> Option<String> {
        let reference = self
            .repo
            .find_reference(&format!("refs/remotes/{remote}/HEAD"))
            .ok()?;
        let target = reference.target();
        let name = target.try_name()?;
        let prefix = format!("refs/remotes/{remote}/");
        name.as_bstr()
            .to_string()
            .strip_prefix(&prefix)
            .map(str::to_string)
    }

    /// Whether `name` is a local branch.
    pub fn has_local_branch(&self, name: &str) -> bool {
        self.repo
            .find_reference(&format!("refs/heads/{name}"))
            .is_ok()
    }

    /// Whether the working tree has files git does not track and does not ignore
    /// (`git ls-files --others --exclude-standard` printing anything).
    pub fn has_untracked_files(&self) -> bool {
        let Ok(platform) = self.repo.status(gix::progress::Discard) else {
            return false;
        };
        let Ok(items) = platform
            .untracked_files(gix::status::UntrackedFiles::Collapsed)
            .into_index_worktree_iter(Vec::<gix::bstr::BString>::new())
        else {
            return false;
        };
        items.flatten().any(|item| {
            matches!(
                item,
                gix::status::index_worktree::Item::DirectoryContents { entry, .. }
                    if entry.status == gix::dir::entry::Status::Untracked
            )
        })
    }

    /// Lines added and removed in tracked files since HEAD, staged or not
    /// (`git diff HEAD --shortstat`). `None` before the first commit or outside a working tree.
    ///
    /// A file moved without edits counts as nothing, as it does for git's rename detection.
    pub fn diff_head_stats(&self) -> Option<(u32, u32)> {
        let changes = self.head_changes()?;
        let (mut added, mut removed) = (0u32, 0u32);
        let removed_only: Vec<&[u8]> = changes
            .iter()
            .filter_map(|c| c.new.is_none().then_some(c.old.as_deref()).flatten())
            .collect();
        let added_only: Vec<&[u8]> = changes
            .iter()
            .filter_map(|c| c.old.is_none().then_some(c.new.as_deref()).flatten())
            .collect();
        for change in &changes {
            let (old, new) = (change.old.as_deref(), change.new.as_deref());
            // An unedited move shows as one deletion and one addition of the same content.
            let moved = match (old, new) {
                (Some(old), None) => added_only.contains(&old),
                (None, Some(new)) => removed_only.contains(&new),
                _ => false,
            };
            if moved {
                continue;
            }
            let (a, r) = count_lines(old.unwrap_or_default(), new.unwrap_or_default());
            added = added.saturating_add(a);
            removed = removed.saturating_add(r);
        }
        Some((added, removed))
    }

    /// The tracked files that differ between HEAD and the working tree, with both versions.
    fn head_changes(&self) -> Option<Vec<FileChange>> {
        let head_tree = self.repo.head_tree().ok()?;
        let workdir = self.repo.workdir()?.to_path_buf();
        let items = self
            .repo
            .status(gix::progress::Discard)
            .ok()?
            .untracked_files(gix::status::UntrackedFiles::None)
            .into_iter(Vec::<gix::bstr::BString>::new())
            .ok()?;

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
                _ => {}
            }
        }

        let mut changes = Vec::new();
        for path in paths {
            let Ok(relative) = gix::path::from_bstr(path.as_ref() as &gix::bstr::BStr) else {
                continue;
            };
            let old = head_tree
                .lookup_entry_by_path(&relative)
                .ok()
                .flatten()
                .filter(|entry| entry.mode().is_blob())
                .and_then(|entry| entry.object().ok())
                .map(|object| object.data.clone());
            let new = std::fs::read(workdir.join(&relative)).ok();
            if old != new {
                changes.push(FileChange { old, new });
            }
        }
        Some(changes)
    }

    /// The best common ancestor of two revisions (`git merge-base a b`), as full hex.
    pub fn merge_base(&self, a: &str, b: &str) -> Option<String> {
        let a = self.repo.rev_parse_single(a).ok()?.detach();
        let b = self.repo.rev_parse_single(b).ok()?.detach();
        self.repo
            .merge_base(a, b)
            .ok()
            .flatten()
            .map(|id| id.to_string())
    }
}

/// One tracked file's content at HEAD and in the working tree; `None` when it does not exist there.
struct FileChange {
    old: Option<Vec<u8>>,
    new: Option<Vec<u8>>,
}

/// Lines added and removed going from `old` to `new`. Binary content counts as no lines, as in
/// `git diff --shortstat`.
fn count_lines(old: &[u8], new: &[u8]) -> (u32, u32) {
    if old.contains(&0) || new.contains(&0) {
        return (0, 0);
    }
    let input = gix::diff::blob::InternedInput::new(old, new);
    let diff = gix::diff::blob::Diff::compute(gix::diff::blob::Algorithm::Histogram, &input);
    (diff.count_additions(), diff.count_removals())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// A repository with one commit on `main`, built without the `git` binary.
    fn repo_with_commit(dir: &Path) -> gix::Repository {
        let mut repo = gix::init(dir).expect("init");
        {
            let mut config = repo.config_snapshot_mut();
            config.set_raw_value(&"user.name", "Test").expect("name");
            config
                .set_raw_value(&"user.email", "test@example.com")
                .expect("email");
        }
        let tree = repo.empty_tree().id;
        repo.commit("refs/heads/main", "first", tree, None::<gix::ObjectId>)
            .expect("commit");
        std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/main\n").expect("head");
        repo
    }


    /// Commit `files` on `main` and check them out into the index, without the `git` binary.
    fn commit_files(repo: &gix::Repository, files: &[(&str, &str)]) {
        let dir = repo.workdir().expect("workdir").to_path_buf();
        let mut editor = repo.edit_tree(repo.empty_tree().id).expect("editor");
        for (path, content) in files {
            let blob = repo.write_blob(content.as_bytes()).expect("blob").detach();
            editor
                .upsert(*path, gix::object::tree::EntryKind::Blob, blob)
                .expect("upsert");
            let full = dir.join(path);
            std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
            std::fs::write(full, content).expect("write");
        }
        let tree = editor.write().expect("tree");
        let parent = repo.head_id().ok().map(gix::Id::detach);
        repo.commit("refs/heads/main", "files", tree, parent)
            .expect("commit");
        let mut index = repo.index_from_tree(&tree).expect("index");
        index.write(Default::default()).expect("write index");
    }

    #[test]
    fn reads_the_facts_of_a_repository() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().canonicalize().expect("canonical");
        let repo = repo_with_commit(&dir);
        let head = repo.head_id().expect("head").to_string();

        let git = GitRepo::discover(&dir).expect("discovered");
        assert_eq!(git.work_dir(), Some(dir.clone()));
        assert_eq!(git.branch().as_deref(), Some("main"));
        assert_eq!(git.head_commit(), Some(head));
        assert!(!git.is_linked_worktree());
        assert!(git.has_local_branch("main"));
        assert!(!git.has_local_branch("other"));
        assert_eq!(git.remote_url("origin"), None);
    }

    #[test]
    fn a_repository_with_no_commit_has_no_branch_or_head() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().canonicalize().expect("canonical");
        gix::init(&dir).expect("init");

        let git = GitRepo::discover(&dir).expect("discovered");
        assert_eq!(git.branch(), None);
        assert_eq!(git.head_commit(), None);
    }

    #[test]
    fn a_directory_outside_any_repository_is_not_one() {
        let temp = tempfile::tempdir().expect("tempdir");
        assert!(GitRepo::discover(temp.path()).is_none());
    }

    #[test]
    fn a_subdirectory_finds_its_repository() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().canonicalize().expect("canonical");
        repo_with_commit(&dir);
        let nested = dir.join("a/b");
        std::fs::create_dir_all(&nested).expect("mkdir");

        let git = GitRepo::discover(&nested).expect("discovered");
        assert_eq!(git.work_dir(), Some(dir));
    }

    #[test]
    fn the_remote_url_and_its_default_branch_are_read() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().canonicalize().expect("canonical");
        let repo = repo_with_commit(&dir);
        let head = repo.head_id().expect("head");
        std::fs::write(
            dir.join(".git/config"),
            std::fs::read_to_string(dir.join(".git/config")).expect("config")
                + "[remote \"origin\"]\n\turl = git@example.com:team/repo.git\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n",
        )
        .expect("write config");
        repo.reference(
            "refs/remotes/origin/main",
            head,
            gix::refs::transaction::PreviousValue::Any,
            "test",
        )
        .expect("remote branch");
        std::fs::create_dir_all(dir.join(".git/refs/remotes/origin")).expect("mkdir");
        std::fs::write(
            dir.join(".git/refs/remotes/origin/HEAD"),
            "ref: refs/remotes/origin/main\n",
        )
        .expect("origin head");

        let git = GitRepo::discover(&dir).expect("discovered");
        assert_eq!(
            git.remote_url("origin").as_deref(),
            Some("git@example.com:team/repo.git")
        );
        assert_eq!(git.remote_default_branch("origin").as_deref(), Some("main"));
        assert_eq!(git.remote_url("missing"), None);
    }

    #[test]
    fn finds_the_merge_base_of_two_commits() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().canonicalize().expect("canonical");
        let repo = repo_with_commit(&dir);
        let base = repo.head_id().expect("head");
        let tree = repo.empty_tree().id;
        let child = repo
            .commit("refs/heads/feature", "child", tree, [base.detach()])
            .expect("child");

        let git = GitRepo::discover(&dir).expect("discovered");
        assert_eq!(
            git.merge_base("main", "feature"),
            Some(base.to_string()),
            "child is {child}"
        );
    }

    #[test]
    fn untracked_files_are_noticed_and_tracked_ones_are_not() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().canonicalize().expect("canonical");
        let repo = repo_with_commit(&dir);
        commit_files(&repo, &[("tracked.txt", "one\n")]);

        let git = GitRepo::discover(&dir).expect("discovered");
        assert!(!git.has_untracked_files());

        std::fs::write(dir.join("new.txt"), "x").expect("write");
        assert!(git.has_untracked_files());
    }

    #[test]
    fn diff_stats_count_lines_changed_since_head() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().canonicalize().expect("canonical");
        let repo = repo_with_commit(&dir);
        commit_files(&repo, &[("a.txt", "1\n2\n3\n"), ("b.txt", "keep\n")]);

        let git = GitRepo::discover(&dir).expect("discovered");
        assert_eq!(git.diff_head_stats(), Some((0, 0)));

        // One line replaced and one added in a.txt; b.txt deleted; an untracked file ignored.
        std::fs::write(dir.join("a.txt"), "1\nTWO\n3\n4\n").expect("edit");
        std::fs::remove_file(dir.join("b.txt")).expect("delete");
        std::fs::write(dir.join("untracked.txt"), "ignored\nby the stat\n").expect("untracked");
        assert_eq!(git.diff_head_stats(), Some((2, 2)));
    }

    #[test]
    fn an_unstaged_move_shows_only_its_deletion_like_git() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().canonicalize().expect("canonical");
        let repo = repo_with_commit(&dir);
        commit_files(&repo, &[("old.txt", "a\nb\nc\n")]);

        std::fs::rename(dir.join("old.txt"), dir.join("moved.txt")).expect("move");
        let git = GitRepo::discover(&dir).expect("discovered");
        // The new name is untracked, so only the deletion counts, exactly as in
        // `git diff HEAD --shortstat` before the move is staged.
        let stats = git.diff_head_stats().expect("stats");
        assert_eq!(stats.1, 3);
    }

    #[test]
    fn there_are_no_stats_before_the_first_commit() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().canonicalize().expect("canonical");
        gix::init(&dir).expect("init");
        assert_eq!(GitRepo::discover(&dir).expect("repo").diff_head_stats(), None);
    }
}
