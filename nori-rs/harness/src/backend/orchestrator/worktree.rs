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
    // gix cannot add worktrees; git does, off the hot path.
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["worktree", "add", "--detach"])
        .arg(&path)
        .arg("HEAD")
        .output()
        .await
        .map_err(|error| format!("could not run git to add a worktree: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git could not add a worktree: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
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
