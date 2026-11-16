//! Utility to compute the current Git diff for the working directory.
//!
//! Returns a PR-like diff: changes since the merge-base with the default
//! branch, plus any untracked files. When the current directory is not inside
//! a Git repository, the function returns `Ok((false, String::new()))`.
//!
//! The repository is read in-process; nothing here starts `git`.

use std::io;
use std::path::Path;
use std::path::PathBuf;

use nori_config::GitRepo;

/// Return value of [`get_git_diff`].
///
/// * `bool` – Whether the directory is inside a Git repo.
/// * `String` – The concatenated diff (may be empty).
pub(crate) async fn get_git_diff(dir: Option<&Path>) -> io::Result<(bool, String)> {
    let dir = match dir {
        Some(dir) => dir.to_path_buf(),
        None => std::env::current_dir()?,
    };
    tokio::task::spawn_blocking(move || diff_in(&dir))
        .await
        .map_err(io::Error::other)
}

fn diff_in(dir: &PathBuf) -> (bool, String) {
    let Some(repo) = GitRepo::discover(dir).filter(|repo| repo.work_dir().is_some()) else {
        return (false, String::new());
    };
    let base = repo.pr_diff_base();
    (true, repo.diff_text(&base, true).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use tempfile::TempDir;

    /// Create a temp git repo with an initial commit and return the TempDir.
    async fn create_temp_git_repo() -> TempDir {
        let dir = TempDir::new().expect("failed to create temp dir");
        let path = dir.path();

        // git init + initial commit
        run_in(path, &["git", "init"]).await;
        run_in(path, &["git", "config", "user.email", "test@test.com"]).await;
        run_in(path, &["git", "config", "user.name", "Test"]).await;
        std::fs::write(path.join("file.txt"), "initial\n").unwrap();
        run_in(path, &["git", "add", "."]).await;
        run_in(path, &["git", "commit", "-m", "init"]).await;

        dir
    }

    async fn run_in(dir: &Path, args: &[&str]) {
        let status = tokio::process::Command::new(args[0])
            .args(&args[1..])
            .current_dir(dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .unwrap();
        assert!(status.success(), "command failed: {args:?}");
    }

    #[tokio::test]
    async fn diff_with_explicit_dir_shows_changes() {
        let dir = create_temp_git_repo().await;
        let path = dir.path();

        // Modify the tracked file
        std::fs::write(path.join("file.txt"), "modified\n").unwrap();

        let (is_git, diff_text) = get_git_diff(Some(path)).await.unwrap();
        assert!(is_git, "should detect git repo");
        assert!(
            !diff_text.is_empty(),
            "diff should be non-empty when there are changes"
        );
        assert!(
            diff_text.contains("file.txt"),
            "diff should mention the changed file"
        );
    }

    #[tokio::test]
    async fn diff_with_explicit_dir_includes_untracked_files() {
        let dir = create_temp_git_repo().await;
        let path = dir.path();

        // Create an untracked file
        std::fs::write(path.join("new_file.txt"), "new content\n").unwrap();

        let (is_git, diff_text) = get_git_diff(Some(path)).await.unwrap();
        assert!(is_git);
        assert!(
            diff_text.contains("new_file.txt"),
            "diff should include untracked files"
        );
    }

    #[tokio::test]
    async fn diff_with_non_git_dir_returns_not_git() {
        let dir = TempDir::new().expect("failed to create temp dir");
        let (is_git, diff_text) = get_git_diff(Some(dir.path())).await.unwrap();
        assert!(!is_git, "non-git dir should return false");
        assert!(diff_text.is_empty());
    }

    #[tokio::test]
    async fn diff_includes_branch_commits_not_just_uncommitted() {
        let dir = create_temp_git_repo().await;
        let path = dir.path();

        // Create a "main" branch reference at current HEAD
        run_in(path, &["git", "branch", "-M", "main"]).await;

        // Create a feature branch and make a committed change
        run_in(path, &["git", "checkout", "-b", "feature"]).await;
        std::fs::write(path.join("file.txt"), "changed on feature\n").unwrap();
        run_in(path, &["git", "add", "."]).await;
        run_in(path, &["git", "commit", "-m", "feature change"]).await;

        // No uncommitted changes, but there's a committed diff vs main
        let (is_git, diff_text) = get_git_diff(Some(path)).await.unwrap();
        assert!(is_git);
        assert!(
            !diff_text.is_empty(),
            "diff should show committed changes vs merge-base (PR-like diff)"
        );
        assert!(
            diff_text.contains("file.txt"),
            "diff should show the file changed on the feature branch"
        );
    }

    #[tokio::test]
    async fn diff_on_main_with_no_changes_is_empty() {
        let dir = create_temp_git_repo().await;
        let path = dir.path();
        run_in(path, &["git", "branch", "-M", "main"]).await;

        let (is_git, diff_text) = get_git_diff(Some(path)).await.unwrap();
        assert!(is_git);
        assert!(
            diff_text.is_empty(),
            "no changes on main should produce empty diff"
        );
    }
}
