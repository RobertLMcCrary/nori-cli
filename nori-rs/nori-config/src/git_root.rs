//! Git repository identity used by project trust settings and the footer.
//!
//! The repository is opened in-process with `gix`, reading only its own configuration: no system
//! or user config files and never the `git` binary. Startup asks these questions before its first
//! frame, where a `git` process each would cost tens of milliseconds.

use std::path::Path;
use std::path::PathBuf;

/// What Nori reads about the repository a path is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitFacts {
    /// The repository's shared git directory (`git rev-parse --git-common-dir`).
    common_dir: PathBuf,
    kind: gix::repository::Kind,
    /// The checked-out branch; `None` when HEAD is detached or unborn on no branch.
    branch: Option<String>,
}

impl GitFacts {
    /// The repository `path` is in, found by searching upwards like `git` does.
    pub fn read(path: &Path) -> Option<Self> {
        let start = if path.is_dir() { path } else { path.parent()? };
        let repo =
            gix::discover_opts(start, Default::default(), gix::open::Options::isolated()).ok()?;
        let branch = repo
            .head_name()
            .ok()
            .flatten()
            .filter(|name| name.category() == Some(gix::reference::Category::LocalBranch))
            .map(|name| name.shorten().to_string());
        Some(Self {
            common_dir: repo.common_dir().to_path_buf(),
            kind: repo.kind(),
            branch,
        })
    }

    /// Whether this is a linked worktree rather than the main working tree.
    pub fn is_linked_worktree(&self) -> bool {
        self.kind == gix::repository::Kind::LinkedWorkTree
    }

    /// The checked-out branch, as `git branch --show-current` reports it.
    pub fn current_branch(&self) -> Option<&str> {
        self.branch.as_deref()
    }
}

/// Resolve a path to the primary repository working directory.
///
/// The common git directory points linked worktrees back to the main repository,
/// so a single trust decision applies throughout the project.
pub fn resolve_root_git_project_for_trust(cwd: &Path) -> Option<PathBuf> {
    let facts = GitFacts::read(cwd)?;
    let common_dir = std::fs::canonicalize(&facts.common_dir).unwrap_or(facts.common_dir);
    common_dir.parent().map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap_or_else(|error| panic!("git {args:?}: {error}"));
        assert!(output.status.success(), "git {args:?} failed");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        git(dir.path(), &["init", "-q", "-b", "main"]);
        git(dir.path(), &["config", "user.email", "t@example.com"]);
        git(dir.path(), &["config", "user.name", "T"]);
        std::fs::write(dir.path().join("f"), "x").unwrap_or_else(|error| panic!("{error}"));
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-q", "-m", "init"]);
        dir
    }

    fn canonical(path: &Path) -> PathBuf {
        std::fs::canonicalize(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
    }

    #[test]
    fn outside_a_repository_there_is_nothing() {
        let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        assert_eq!(resolve_root_git_project_for_trust(dir.path()), None);
    }

    #[test]
    fn a_repository_and_its_linked_worktree_agree_with_git() {
        let repo = repo();
        let nested = repo.path().join("a/b");
        std::fs::create_dir_all(&nested).unwrap_or_else(|error| panic!("{error}"));
        let root = canonical(repo.path());
        assert_eq!(
            resolve_root_git_project_for_trust(&nested),
            Some(root.clone())
        );

        let main = GitFacts::read(&nested).unwrap_or_else(|| panic!("repository found"));
        assert!(!main.is_linked_worktree());
        assert_eq!(main.current_branch(), Some("main"));

        let worktree = repo.path().join("wt");
        git(
            repo.path(),
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "side",
                worktree.to_str().unwrap_or_default(),
            ],
        );
        let linked = GitFacts::read(&worktree).unwrap_or_else(|| panic!("worktree found"));
        assert!(linked.is_linked_worktree());
        assert_eq!(linked.current_branch(), Some("side"));
        assert_eq!(
            canonical(&linked.common_dir),
            canonical(Path::new(&git(
                &worktree,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"]
            )))
        );
        assert_eq!(resolve_root_git_project_for_trust(&worktree), Some(root));

        git(&worktree, &["switch", "-q", "--detach"]);
        let detached = GitFacts::read(&worktree).unwrap_or_else(|| panic!("worktree found"));
        assert_eq!(detached.current_branch(), None);
    }
}
