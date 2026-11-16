//! Worktrees for tasks that must not share the parent's checkout.

use std::path::Path;
use std::path::PathBuf;

/// A new worktree of the repository `cwd` is in, detached at its current commit.
pub(super) async fn create(cwd: &Path) -> Result<PathBuf, String> {
    let root = nori_config::resolve_root_git_project_for_trust(cwd).ok_or_else(|| {
        "`isolation: worktree` needs a git repository, and this is not one".to_owned()
    })?;
    let path = root.join(".worktrees").join(format!(
        "nori-task-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    ));
    let repo_root = root.clone();
    let worktree = path.clone();
    tokio::task::spawn_blocking(move || codex_git::create_detached_worktree(&repo_root, &worktree))
        .await
        .map_err(|error| format!("adding a worktree was interrupted: {error}"))?
        .map_err(|error| format!("could not add a worktree: {error}"))?;
    exclude_worktrees(cwd);
    Ok(path)
}

/// Keep task worktrees out of `git status` with the repository's local exclude file, which is
/// never committed.
fn exclude_worktrees(cwd: &Path) {
    const PATTERN: &str = "/.worktrees/";
    let Some(facts) = nori_config::GitFacts::read(cwd) else {
        return;
    };
    let exclude = facts.common_dir().join("info").join("exclude");
    let existing = std::fs::read_to_string(&exclude).unwrap_or_default();
    if existing.lines().any(|line| line.trim() == PATTERN) {
        return;
    }
    if let Some(parent) = exclude.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let separator = if existing.is_empty() || existing.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let _ = std::fs::write(&exclude, format!("{existing}{separator}{PATTERN}\n"));
}
