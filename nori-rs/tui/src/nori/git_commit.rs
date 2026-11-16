//! The request an agent gets to land the working tree's changes as small commits.
//!
//! Committing is not a slash command: it happens after a turn when `[git] auto_commit` is on, or
//! on a hotkey. Either way it is a prompt built here from what changed and the user's rules, and
//! the agent does the work through the `git_status` and `git_commit` tools Nori serves.

use nori_config::ChangeState;
use nori_config::ChangedFile;
use nori_config::GitConfig;

/// The prompt asking an agent to commit `changes`, following the configured rules.
///
/// `current_agent` is the agent receiving it; when `[git] commit_agent` names a different one the
/// prompt tells the receiver to hand the work to that agent.
pub(crate) fn commit_prompt(
    git: &GitConfig,
    changes: &[ChangedFile],
    current_agent: &str,
) -> String {
    let mut prompt = String::from(
        "Land the working tree's uncommitted changes as a series of small, focused commits.\n\n\
         Call `git_status` to see what changed, group the files by purpose (one logical change \
         per commit), then call `git_commit` once per group with exactly those paths and a \
         message that explains why the change was made. Leave out anything that looks like a \
         secret or build output, and do not edit any file. Stop when everything you meant to \
         commit is committed.\n",
    );

    prompt.push_str(&format!("\nChanged files ({}):\n", changes.len()));
    const SHOWN: usize = 40;
    for change in changes.iter().take(SHOWN) {
        let state = match change.state {
            ChangeState::Modified => "modified",
            ChangeState::Added => "added",
            ChangeState::Deleted => "deleted",
        };
        prompt.push_str(&format!("- {state} {}\n", change.path));
    }
    if changes.len() > SHOWN {
        prompt.push_str(&format!(
            "- and {} more (see `git_status`)\n",
            changes.len() - SHOWN
        ));
    }

    if let Some(rules) = &git.commit_rules {
        prompt.push_str(&format!("\nCommit rules:\n{rules}\n"));
    }
    if let Some(rules) = &git.pr_rules {
        prompt.push_str(&format!(
            "\nPull request rules. They apply once the commits are in: follow them for whether this \
             work is ready to upstream, what a pull request should look like, and how to check CI.\n{rules}\n"
        ));
    }

    match &git.commit_agent {
        Some(agent) if agent != current_agent => format!(
            "Do not do this yourself. Call `delegate_task` with agent \"{agent}\" and the \
             instructions below, exactly as written, as its task. Wait for it, then tell me which \
             commits it made.\n\n---\n{prompt}"
        ),
        _ => prompt,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn change(path: &str, state: ChangeState) -> ChangedFile {
        ChangedFile {
            path: path.to_string(),
            state,
        }
    }

    #[test]
    fn lists_the_changes_and_names_the_tools() {
        let prompt = commit_prompt(
            &GitConfig::default(),
            &[
                change("src/a.rs", ChangeState::Modified),
                change("new.txt", ChangeState::Added),
            ],
            "claude-code",
        );
        assert!(prompt.contains("`git_status`") && prompt.contains("`git_commit`"));
        assert!(prompt.contains("Changed files (2):"));
        assert!(prompt.contains("- modified src/a.rs"));
        assert!(prompt.contains("- added new.txt"));
        assert!(!prompt.contains("Commit rules"), "no rules, no section");
        assert!(!prompt.contains("delegate_task"));
    }

    #[test]
    fn carries_the_users_rules() {
        let git = GitConfig {
            commit_rules: Some("Conventional Commits".into()),
            pr_rules: Some("Wait for green CI".into()),
            ..GitConfig::default()
        };
        let prompt = commit_prompt(&git, &[change("a", ChangeState::Added)], "codex");
        assert!(prompt.contains("Commit rules:\nConventional Commits"));
        assert!(prompt.contains("how to check CI.\nWait for green CI"));
    }

    #[test]
    fn a_long_list_is_cut_and_says_so() {
        let many: Vec<ChangedFile> = (0..45)
            .map(|n| change(&format!("f{n}.txt"), ChangeState::Added))
            .collect();
        let prompt = commit_prompt(&GitConfig::default(), &many, "codex");
        assert!(prompt.contains("Changed files (45):"));
        assert!(prompt.contains("- added f39.txt"));
        assert!(!prompt.contains("f40.txt"));
        assert!(prompt.contains("and 5 more"));
    }

    #[test]
    fn a_designated_other_agent_gets_the_work_by_delegation() {
        let git = GitConfig {
            commit_agent: Some("codex".into()),
            ..GitConfig::default()
        };
        let changes = [change("a", ChangeState::Modified)];

        let handed_off = commit_prompt(&git, &changes, "claude-code");
        assert!(handed_off.starts_with("Do not do this yourself."));
        assert!(handed_off.contains("`delegate_task` with agent \"codex\""));
        assert!(handed_off.contains("Land the working tree"));

        let own = commit_prompt(&git, &changes, "codex");
        assert!(!own.contains("delegate_task"), "already the right agent");
        assert_eq!(own.matches("Land the working tree").count(), 1);
    }
}
