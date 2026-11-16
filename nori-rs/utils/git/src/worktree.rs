//! Linked worktrees for auto-worktree sessions, managed in-process with `gix`.
//!
//! `gix` can read worktrees but has no call that creates one, so the pieces of
//! `git worktree add -b` are done here: the branch, the administrative directory under
//! `<common-dir>/worktrees/`, the `.git` file, and a checkout of HEAD with its index.

use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use gix::refs::transaction::Change;
use gix::refs::transaction::LogChange;
use gix::refs::transaction::PreviousValue;
use gix::refs::transaction::RefEdit;

use crate::errors::GitToolingError;

const ADJECTIVES: &[&str] = &[
    "swift", "bright", "calm", "bold", "keen", "warm", "cool", "fair", "glad", "kind", "neat",
    "pure", "rich", "safe", "tall", "vast", "wise", "able", "busy", "deep", "easy", "fast", "good",
    "high", "just", "lean", "mild", "open", "real", "slim",
];

const NOUNS: &[&str] = &[
    "oak", "fox", "elm", "bay", "owl", "ash", "bee", "cod", "dew", "elk", "fig", "gem", "hen",
    "ivy", "jay", "kit", "log", "map", "net", "orb", "pen", "ray", "sun", "tea", "urn", "van",
    "web", "yak", "zen", "arc",
];

/// Information about a git worktree (excluding the main worktree).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub branch: Option<String>,
}

fn gix_error(error: impl std::fmt::Display) -> GitToolingError {
    GitToolingError::Gix(error.to_string())
}

/// List all git worktrees (excluding the main worktree) for the given repo root.
pub fn list_worktrees(repo_root: &Path) -> Result<Vec<WorktreeInfo>, GitToolingError> {
    let repo = open(repo_root)?;
    let mut worktrees = Vec::new();
    for proxy in repo.worktrees().map_err(gix_error)? {
        let branch = std::fs::read_to_string(proxy.git_dir().join("HEAD"))
            .ok()
            .and_then(|head| {
                head.trim()
                    .strip_prefix("ref: refs/heads/")
                    .map(str::to_string)
            });
        let path = proxy.base().map_err(gix_error)?;
        worktrees.push(WorktreeInfo { path, branch });
    }
    worktrees.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(worktrees)
}

fn open(repo_root: &Path) -> Result<gix::Repository, GitToolingError> {
    gix::open(repo_root).map_err(|_| GitToolingError::NotAGitRepository {
        path: repo_root.to_path_buf(),
    })
}

/// Create a git worktree at the specified path with the given branch name, as
/// `git worktree add <path> -b <branch>` does: the branch starts at HEAD and is checked out
/// there.
pub fn create_worktree(
    repo_root: &Path,
    worktree_path: &Path,
    branch_name: &str,
) -> Result<PathBuf, GitToolingError> {
    let repo = open(repo_root)?;
    let head = repo.head_commit().map_err(gix_error)?;
    let branch_ref = format!("refs/heads/{branch_name}");
    let worktree_path = std::path::absolute(worktree_path)?;
    if worktree_path.exists() && std::fs::read_dir(&worktree_path)?.next().is_some() {
        return Err(GitToolingError::Gix(format!(
            "{} already exists and is not empty",
            worktree_path.display()
        )));
    }

    // The branch comes first: if it already exists, nothing else has been touched. gix treats
    // creating a reference with the value it already has as a no-op, so ask explicitly.
    if repo.find_reference(branch_ref.as_str()).is_ok() {
        return Err(GitToolingError::Gix(format!(
            "a branch named '{branch_name}' already exists"
        )));
    }
    repo.reference(
        branch_ref.as_str(),
        head.id,
        PreviousValue::MustNotExist,
        format!(
            "branch: Created from HEAD for worktree {}",
            worktree_path.display()
        ),
    )
    .map_err(gix_error)?;

    let result = add_checkout(
        &repo,
        &worktree_path,
        &format!("ref: {branch_ref}"),
        head.tree_id().map_err(gix_error)?.detach(),
    );
    if result.is_err() {
        // Leave no half-made branch behind for a retry to trip on.
        let _ = repo.find_reference(branch_ref.as_str()).map(|r| r.delete());
    }
    result?;
    Ok(worktree_path)
}

/// Create a worktree at `worktree_path` with HEAD detached at the current commit, as
/// `git worktree add --detach <path> HEAD` does. No branch is made.
pub fn create_detached_worktree(
    repo_root: &Path,
    worktree_path: &Path,
) -> Result<PathBuf, GitToolingError> {
    let repo = open(repo_root)?;
    let head = repo.head_commit().map_err(gix_error)?;
    let worktree_path = std::path::absolute(worktree_path)?;
    add_checkout(
        &repo,
        &worktree_path,
        &head.id.to_string(),
        head.tree_id().map_err(gix_error)?.detach(),
    )?;
    Ok(worktree_path)
}

/// Make `worktree_path` a linked worktree whose HEAD file reads `head_line`, with `tree` checked
/// out and indexed.
fn add_checkout(
    repo: &gix::Repository,
    worktree_path: &Path,
    head_line: &str,
    tree: gix::ObjectId,
) -> Result<(), GitToolingError> {
    let common_dir = repo.common_dir().to_path_buf();
    let name = unique_admin_name(&common_dir, worktree_path);
    let admin_dir = common_dir.join("worktrees").join(&name);

    std::fs::create_dir_all(worktree_path)?;
    std::fs::create_dir_all(&admin_dir)?;
    let dot_git = worktree_path.join(".git");
    std::fs::write(&dot_git, format!("gitdir: {}\n", admin_dir.display()))?;
    std::fs::write(admin_dir.join("HEAD"), format!("{head_line}\n"))?;
    std::fs::write(admin_dir.join("commondir"), "../..\n")?;
    std::fs::write(admin_dir.join("gitdir"), format!("{}\n", dot_git.display()))?;

    let linked = gix::open(worktree_path).map_err(gix_error)?;
    let mut index = linked.index_from_tree(&tree).map_err(gix_error)?;
    let options = linked
        .checkout_options(gix::worktree::stack::state::attributes::Source::IdMapping)
        .map_err(gix_error)?;
    let objects = linked
        .objects
        .clone()
        .into_arc()
        .map_err(|error| GitToolingError::Gix(error.to_string()))?;
    gix::worktree::state::checkout(
        &mut index,
        worktree_path,
        objects,
        &gix::progress::Discard,
        &gix::progress::Discard,
        &AtomicBool::new(false),
        options,
    )
    .map_err(gix_error)?;
    index.write(Default::default()).map_err(gix_error)?;
    Ok(())
}

/// The name of the administrative directory: the worktree's directory name, made unique the way
/// git does it by appending a number.
fn unique_admin_name(common_dir: &Path, worktree_path: &Path) -> String {
    let base = worktree_path.file_name().map_or_else(
        || "worktree".to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let taken = |name: &str| common_dir.join("worktrees").join(name).exists();
    if !taken(&base) {
        return base;
    }
    (1..)
        .map(|n| format!("{base}{n}"))
        .find(|name| !taken(name))
        .unwrap_or(base)
}

/// Ensure an entry exists in the repo's .gitignore file.
pub fn ensure_gitignore_entry(repo_root: &Path, entry: &str) -> Result<(), GitToolingError> {
    let gitignore_path = repo_root.join(".gitignore");

    if gitignore_path.exists() {
        let content = std::fs::read_to_string(&gitignore_path)?;
        // Check if entry already exists (as a whole line)
        if content.lines().any(|line| line.trim() == entry) {
            return Ok(());
        }
        // Append the entry, ensuring it starts on a new line
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&gitignore_path)?;
        if !content.ends_with('\n') && !content.is_empty() {
            writeln!(file)?;
        }
        writeln!(file, "{entry}")?;
    } else {
        std::fs::write(&gitignore_path, format!("{entry}\n"))?;
    }

    Ok(())
}

/// Generate a unique worktree branch name with random words and a timestamp.
pub fn generate_worktree_branch_name() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::Hash;
    use std::hash::Hasher;
    use std::time::SystemTime;

    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();

    // Use time-based hashing for pseudo-random word selection
    let mut hasher = DefaultHasher::new();
    now.as_nanos().hash(&mut hasher);
    let hash = hasher.finish();

    let adj = ADJECTIVES[hash as usize % ADJECTIVES.len()];
    let noun = NOUNS[(hash >> 16) as usize % NOUNS.len()];

    let timestamp = format_timestamp();
    format!("auto/{adj}-{noun}-{timestamp}")
}

/// Generate a worktree branch name from a prompt summary string.
///
/// Converts the summary to a lowercase slug, appends a timestamp, and prefixes
/// with `auto/`. If the summary is empty or whitespace-only after sanitization,
/// falls back to [`generate_worktree_branch_name`].
///
/// # Examples
/// - `"Fix auth bug"` → `"auto/fix-auth-bug-20260202-120000"`
/// - `""` → falls back to random name like `"auto/swift-oak-20260202-120000"`
pub fn summary_to_branch_name(summary: &str) -> String {
    // Sanitize: lowercase, replace non-alphanumeric with hyphens, collapse, trim
    let slug: String = summary
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();

    // Collapse consecutive hyphens and trim leading/trailing
    let mut collapsed = String::with_capacity(slug.len());
    let mut last_was_hyphen = true; // start true to trim leading hyphens
    for c in slug.chars() {
        if c == '-' {
            if !last_was_hyphen {
                collapsed.push('-');
            }
            last_was_hyphen = true;
        } else {
            collapsed.push(c);
            last_was_hyphen = false;
        }
    }
    // Trim trailing hyphen
    let collapsed = collapsed.trim_end_matches('-');

    if collapsed.is_empty() {
        return generate_worktree_branch_name();
    }

    // Truncate to 40 chars at a word boundary (hyphen)
    let truncated = if collapsed.len() > 40 {
        match collapsed[..40].rfind('-') {
            Some(pos) => &collapsed[..pos],
            None => &collapsed[..40],
        }
    } else {
        collapsed
    };

    // Append timestamp
    let timestamp = format_timestamp();
    format!("auto/{truncated}-{timestamp}")
}

/// Rename a git worktree's branch in place, as `git branch -m` does.
///
/// The worktree directory is left unchanged so that processes running inside it are not
/// disrupted. Every worktree whose HEAD names the old branch follows it to the new name.
pub fn rename_worktree_branch(
    repo_root: &Path,
    old_branch: &str,
    new_branch: &str,
) -> Result<(), GitToolingError> {
    let repo = open(repo_root)?;
    let old_ref = format!("refs/heads/{old_branch}");
    let new_ref = format!("refs/heads/{new_branch}");

    let old = repo
        .find_reference(old_ref.as_str())
        .map_err(|_| GitToolingError::Gix(format!("branch '{old_branch}' not found")))?;
    let target = old.id().detach();
    if repo.find_reference(new_ref.as_str()).is_ok() {
        return Err(GitToolingError::Gix(format!(
            "a branch named '{new_branch}' already exists"
        )));
    }

    let log = || LogChange {
        message: format!("branch: renamed {old_ref} to {new_ref}").into(),
        ..Default::default()
    };
    repo.edit_references([
        RefEdit {
            change: Change::Update {
                log: log(),
                expected: PreviousValue::MustNotExist,
                new: gix::refs::Target::Object(target),
            },
            name: new_ref.as_str().try_into().map_err(gix_error)?,
            deref: false,
        },
        RefEdit {
            change: Change::Delete {
                expected: PreviousValue::MustExistAndMatch(gix::refs::Target::Object(target)),
                log: gix::refs::transaction::RefLog::AndReference,
            },
            name: old_ref.as_str().try_into().map_err(gix_error)?,
            deref: false,
        },
    ])
    .map_err(gix_error)?;

    // HEAD files that pointed at the old name, in the main working tree and every linked one.
    let mut heads = vec![repo.common_dir().join("HEAD")];
    if let Ok(proxies) = repo.worktrees() {
        heads.extend(proxies.iter().map(|proxy| proxy.git_dir().join("HEAD")));
    }
    let (from, to) = (format!("ref: {old_ref}"), format!("ref: {new_ref}\n"));
    for head in heads {
        if std::fs::read_to_string(&head).is_ok_and(|text| text.trim() == from) {
            std::fs::write(&head, &to)?;
        }
    }
    Ok(())
}

/// Format the current UTC timestamp as YYYYMMDD-HHMMSS.
fn format_timestamp() -> String {
    use std::time::SystemTime;

    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();

    let secs = now.as_secs();
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;
    let (year, month, day) = days_to_ymd(days);

    format!("{year:04}{month:02}{day:02}-{hours:02}{minutes:02}{seconds:02}")
}

/// Convert days since Unix epoch to (year, month, day).
fn days_to_ymd(days: u64) -> (i32, i32, i32) {
    // Algorithm from https://howardhinnant.github.io/date_algorithms.html
    let z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as i32, d as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::process::Command;

    fn init_temp_repo() -> tempfile::TempDir {
        let temp_dir = tempfile::TempDir::new().unwrap();
        Command::new("git")
            .args(["init"])
            .current_dir(temp_dir.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(temp_dir.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(temp_dir.path())
            .output()
            .unwrap();
        // Create an initial commit so HEAD exists (required for worktree add)
        std::fs::write(temp_dir.path().join("README.md"), "init").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(temp_dir.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(temp_dir.path())
            .output()
            .unwrap();
        temp_dir
    }

    #[test]
    fn test_create_worktree_creates_directory() {
        let temp = init_temp_repo();
        let worktree_path = temp.path().join(".worktrees").join("test-branch");
        std::fs::create_dir_all(temp.path().join(".worktrees")).unwrap();

        let result = create_worktree(temp.path(), &worktree_path, "test-branch");
        assert!(result.is_ok(), "create_worktree should succeed");
        let path = result.unwrap();
        assert!(path.exists(), "worktree directory should exist");

        // Verify the branch was created
        let output = Command::new("git")
            .args(["branch", "--list", "test-branch"])
            .current_dir(temp.path())
            .output()
            .unwrap();
        let branches = String::from_utf8_lossy(&output.stdout);
        assert!(
            branches.contains("test-branch"),
            "branch 'test-branch' should exist"
        );
    }

    #[test]
    fn test_ensure_gitignore_adds_entry_when_missing() {
        let temp = init_temp_repo();
        // Create .gitignore without the entry
        std::fs::write(temp.path().join(".gitignore"), "node_modules/\n").unwrap();

        let result = ensure_gitignore_entry(temp.path(), ".worktrees/");
        assert!(result.is_ok());

        let content = std::fs::read_to_string(temp.path().join(".gitignore")).unwrap();
        assert!(
            content.contains(".worktrees/"),
            "gitignore should contain .worktrees/"
        );
        // Original content should be preserved
        assert!(
            content.contains("node_modules/"),
            "original gitignore content should be preserved"
        );
    }

    #[test]
    fn test_ensure_gitignore_no_duplicate() {
        let temp = init_temp_repo();
        std::fs::write(temp.path().join(".gitignore"), ".worktrees/\n").unwrap();

        let result = ensure_gitignore_entry(temp.path(), ".worktrees/");
        assert!(result.is_ok());

        let content = std::fs::read_to_string(temp.path().join(".gitignore")).unwrap();
        let count = content.matches(".worktrees/").count();
        assert_eq!(count, 1, ".worktrees/ should appear exactly once");
    }

    #[test]
    fn test_ensure_gitignore_creates_file_when_missing() {
        let temp = init_temp_repo();
        // Do NOT create .gitignore
        assert!(!temp.path().join(".gitignore").exists());

        let result = ensure_gitignore_entry(temp.path(), ".worktrees/");
        assert!(result.is_ok());

        let content = std::fs::read_to_string(temp.path().join(".gitignore")).unwrap();
        assert!(content.contains(".worktrees/"));
    }

    #[test]
    fn test_list_worktrees_returns_extra_worktrees() {
        let temp = init_temp_repo();
        let worktree_path = temp.path().join(".worktrees").join("test-wt");
        std::fs::create_dir_all(temp.path().join(".worktrees")).unwrap();
        create_worktree(temp.path(), &worktree_path, "test-wt-branch").unwrap();

        let worktrees = list_worktrees(temp.path()).unwrap();
        assert_eq!(worktrees.len(), 1, "should find one extra worktree");
        assert!(
            worktrees[0].path.to_string_lossy().contains("test-wt"),
            "worktree path should contain 'test-wt'"
        );
        assert_eq!(worktrees[0].branch, Some("test-wt-branch".to_string()));
    }

    #[test]
    fn test_list_worktrees_no_extras() {
        let temp = init_temp_repo();
        let worktrees = list_worktrees(temp.path()).unwrap();
        assert_eq!(
            worktrees.len(),
            0,
            "fresh repo should have no extra worktrees"
        );
    }

    #[test]
    fn test_generate_worktree_branch_name_format() {
        let name = generate_worktree_branch_name();
        assert!(
            name.starts_with("auto/"),
            "branch name should start with 'auto/', got: {name}"
        );
        // Should contain at least two hyphens (word-word-date-time)
        let after_prefix = &name["auto/".len()..];
        assert!(
            after_prefix.split('-').count() >= 4,
            "branch name should have at least 4 hyphen-separated parts after 'auto/', got: {name}"
        );
    }

    #[test]
    fn test_summary_to_branch_name_normal_summary() {
        let name = summary_to_branch_name("Fix auth bug");
        assert!(
            name.starts_with("auto/fix-auth-bug-"),
            "branch name should start with 'auto/fix-auth-bug-', got: {name}"
        );
        // Should end with a timestamp (YYYYMMDD-HHMMSS)
        let after_slug = name.strip_prefix("auto/fix-auth-bug-").unwrap();
        assert_eq!(
            after_slug.len(),
            15,
            "timestamp portion should be 15 chars (YYYYMMDD-HHMMSS), got: {after_slug}"
        );
    }

    #[test]
    fn test_summary_to_branch_name_strips_special_chars() {
        let name = summary_to_branch_name("Add dark mode!!!");
        assert!(
            name.starts_with("auto/add-dark-mode-"),
            "special chars should be stripped, got: {name}"
        );
    }

    #[test]
    fn test_summary_to_branch_name_empty_falls_back_to_random() {
        let name = summary_to_branch_name("");
        assert!(
            name.starts_with("auto/"),
            "empty summary should fall back to random name, got: {name}"
        );
        // Should look like a random name (adjective-noun-date-time), not just "auto/-date-time"
        let after_prefix = &name["auto/".len()..];
        assert!(
            after_prefix.split('-').count() >= 4,
            "fallback should have at least 4 parts, got: {name}"
        );
        // Should not start with a digit (should start with an adjective)
        assert!(
            after_prefix.chars().next().unwrap().is_alphabetic(),
            "fallback should start with a letter, got: {name}"
        );
    }

    #[test]
    fn test_summary_to_branch_name_truncates_long_summary() {
        let name = summary_to_branch_name(
            "Implement comprehensive user authentication with OAuth2 and JWT tokens",
        );
        assert!(name.starts_with("auto/"));
        // The slug portion (before timestamp) should be capped
        let after_prefix = &name["auto/".len()..];
        // Total after prefix should be slug + hyphen + timestamp (15 chars)
        // So slug should be at most ~40 chars
        let parts: Vec<&str> = after_prefix.rsplitn(3, '-').collect();
        // parts[0] = HHMMSS, parts[1] = YYYYMMDD, parts[2] = slug
        assert!(
            parts.len() == 3,
            "should have slug-YYYYMMDD-HHMMSS structure, got: {after_prefix}"
        );
        let slug = parts[2];
        assert!(
            slug.len() <= 40,
            "slug should be at most 40 chars, got {} chars: {slug}",
            slug.len()
        );
    }

    #[test]
    fn test_summary_to_branch_name_collapses_consecutive_hyphens() {
        let name = summary_to_branch_name("fix   multiple   spaces");
        assert!(
            name.starts_with("auto/fix-multiple-spaces-"),
            "consecutive hyphens should be collapsed, got: {name}"
        );
    }

    #[test]
    fn test_rename_worktree_branch_renames_branch_only() {
        let temp = init_temp_repo();
        let worktrees_dir = temp.path().join(".worktrees");
        std::fs::create_dir_all(&worktrees_dir).unwrap();

        // Create initial worktree
        let wt_path = worktrees_dir.join("old-name");
        create_worktree(temp.path(), &wt_path, "auto/old-name").unwrap();
        assert!(wt_path.exists());

        // Rename the branch only
        let result = rename_worktree_branch(temp.path(), "auto/old-name", "auto/new-name");
        assert!(
            result.is_ok(),
            "rename_worktree_branch should succeed: {:?}",
            result.err()
        );

        // Directory should remain at original path
        assert!(
            wt_path.exists(),
            "worktree directory should still exist at original path"
        );

        // New branch should exist
        let output = Command::new("git")
            .args(["branch", "--list", "auto/new-name"])
            .current_dir(temp.path())
            .output()
            .unwrap();
        let branches = String::from_utf8_lossy(&output.stdout);
        assert!(
            branches.contains("auto/new-name"),
            "new branch name should exist"
        );

        // Old branch should not exist
        let output = Command::new("git")
            .args(["branch", "--list", "auto/old-name"])
            .current_dir(temp.path())
            .output()
            .unwrap();
        let branches = String::from_utf8_lossy(&output.stdout);
        assert!(
            !branches.contains("auto/old-name"),
            "old branch name should not exist"
        );
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// The worktree we build must be one the `git` binary accepts as its own and finds clean.
    #[test]
    fn test_created_worktree_is_a_real_clean_checkout() {
        let temp = init_temp_repo();
        std::fs::create_dir_all(temp.path().join("src/deep")).unwrap();
        std::fs::write(temp.path().join("src/deep/a.txt"), "deep\n").unwrap();
        std::fs::write(temp.path().join("run.sh"), "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                temp.path().join("run.sh"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        git(temp.path(), &["add", "."]);
        git(temp.path(), &["commit", "-m", "more"]);

        let path = temp.path().join(".worktrees").join("feature");
        std::fs::create_dir_all(temp.path().join(".worktrees")).unwrap();
        create_worktree(temp.path(), &path, "auto/feature").unwrap();

        assert_eq!(
            std::fs::read_to_string(path.join("README.md")).unwrap(),
            "init"
        );
        assert_eq!(
            std::fs::read_to_string(path.join("src/deep/a.txt")).unwrap(),
            "deep\n"
        );
        assert_eq!(
            git(&path, &["status", "--porcelain"]),
            "",
            "checkout must be clean"
        );
        assert_eq!(
            git(&path, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
            "auto/feature"
        );
        assert_eq!(
            git(&path, &["rev-parse", "HEAD"]),
            git(temp.path(), &["rev-parse", "HEAD"]),
            "the branch starts at HEAD"
        );
        let listed = git(temp.path(), &["worktree", "list", "--porcelain"]);
        assert!(
            listed.contains("branch refs/heads/auto/feature"),
            "{listed}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path.join("run.sh"))
                .unwrap()
                .permissions()
                .mode();
            assert_ne!(mode & 0o111, 0, "the executable bit is kept");
        }
    }

    #[test]
    fn test_create_worktree_refuses_an_existing_branch_and_changes_nothing() {
        let temp = init_temp_repo();
        git(temp.path(), &["branch", "taken"]);
        let path = temp.path().join(".worktrees").join("taken");
        std::fs::create_dir_all(temp.path().join(".worktrees")).unwrap();

        assert!(create_worktree(temp.path(), &path, "taken").is_err());
        assert!(!path.exists(), "no directory is left behind");
        assert_eq!(
            git(temp.path(), &["worktree", "list", "--porcelain"])
                .matches("worktree ")
                .count(),
            1
        );
    }

    #[test]
    fn test_two_worktrees_with_the_same_directory_name_get_distinct_admin_dirs() {
        let temp = init_temp_repo();
        let a = temp.path().join("a").join("same");
        let b = temp.path().join("b").join("same");
        create_worktree(temp.path(), &a, "branch-a").unwrap();
        create_worktree(temp.path(), &b, "branch-b").unwrap();

        assert_eq!(
            git(&a, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
            "branch-a"
        );
        assert_eq!(
            git(&b, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
            "branch-b"
        );
        assert_eq!(list_worktrees(temp.path()).unwrap().len(), 2);
    }

    #[test]
    fn test_rename_follows_the_worktree_that_has_the_branch_checked_out() {
        let temp = init_temp_repo();
        let path = temp.path().join("wt");
        create_worktree(temp.path(), &path, "old-name").unwrap();

        rename_worktree_branch(temp.path(), "old-name", "new-name").unwrap();

        assert_eq!(
            git(&path, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
            "new-name"
        );
        assert_eq!(
            git(temp.path(), &["branch", "--list", "old-name"]).trim(),
            ""
        );
        let info = list_worktrees(temp.path()).unwrap();
        assert_eq!(info[0].branch.as_deref(), Some("new-name"));
    }

    #[test]
    fn test_rename_refuses_a_taken_name() {
        let temp = init_temp_repo();
        git(temp.path(), &["branch", "one"]);
        git(temp.path(), &["branch", "two"]);
        assert!(rename_worktree_branch(temp.path(), "one", "two").is_err());
        assert!(rename_worktree_branch(temp.path(), "missing", "x").is_err());
    }

    #[test]
    fn test_detached_worktree_is_a_clean_checkout_at_head_with_no_branch() {
        let temp = init_temp_repo();
        let path = temp.path().join(".worktrees").join("task");
        create_detached_worktree(temp.path(), &path).unwrap();

        assert_eq!(git(&path, &["status", "--porcelain"]), "");
        assert_eq!(
            git(&path, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
            "HEAD"
        );
        assert_eq!(
            git(&path, &["rev-parse", "HEAD"]),
            git(temp.path(), &["rev-parse", "HEAD"])
        );
        assert_eq!(git(temp.path(), &["branch", "--list"]).lines().count(), 1);
    }
}
