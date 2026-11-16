//! Renders a [`Workflow`] as plain-language rules for the agent's first prompt.

use crate::model::CommitAgent;
use crate::model::CommitBatch;
use crate::model::CommitRules;
use crate::model::CommitStyle;
use crate::model::HookEvent;
use crate::model::PrRules;
use crate::model::Upstream;
use crate::model::Workflow;

impl Workflow {
    /// The workflow's rules as concise instructions for the agent's initial
    /// prompt. Empty when the workflow sets nothing the agent needs to know.
    pub fn instructions(&self) -> String {
        let mut rules: Vec<String> = Vec::new();
        if let Some(turns) = self.max_turns {
            rules.push(match turns {
                0 => "Stop after each prompt turn; the user decides what happens next.".to_string(),
                1 => {
                    "You may run 1 prompt turn on your own before the user takes over.".to_string()
                }
                n => format!(
                    "You may run up to {n} prompt turns on your own before the user takes over."
                ),
            });
        }
        if let Some(commit) = &self.commit {
            rules.extend(commit_rules(commit));
        }
        if let Some(pr) = &self.pr {
            rules.extend(pr_rules(pr));
        }
        for event in HookEvent::ALL {
            let prompts: Vec<&str> = self.hook_prompts(event).collect();
            if !prompts.is_empty() {
                rules.push(format!(
                    "{}, Nori will tell you: {}",
                    event_moment(event),
                    prompts.join(" ")
                ));
            }
        }
        rules.extend(self.instructions.iter().cloned());
        if rules.is_empty() {
            return String::new();
        }
        let name = &self.name;
        let mut text = format!("Follow the \"{name}\" workflow:\n");
        for rule in rules {
            text.push_str("- ");
            text.push_str(&rule);
            text.push('\n');
        }
        text
    }
}

fn event_moment(event: HookEvent) -> &'static str {
    match event {
        HookEvent::AfterChange => "After a turn that changes files",
        HookEvent::AfterTurn => "After every turn",
        HookEvent::AfterCommit => "After each commit",
        HookEvent::BeforePr => "Before you open a pull request",
        HookEvent::BeforeStop => "Before you stop",
    }
}

fn commit_rules(commit: &CommitRules) -> Vec<String> {
    let CommitRules {
        style,
        max_files,
        max_subject,
        atomic,
        batch,
        require_body,
        agent,
    } = commit;
    let mut message = Vec::new();
    match style {
        Some(CommitStyle::Conventional) => message.push(
            "Commit messages follow Conventional Commits: `type(scope): summary`.".to_string(),
        ),
        Some(CommitStyle::Plain) => {
            message.push("Commit subjects are a plain imperative summary.".to_string())
        }
        None => {}
    }
    if let Some(max) = max_subject {
        message.push(format!("Keep the subject line within {max} characters."));
    }
    if *require_body {
        message.push("Every commit has a body that explains why the change was made.".to_string());
    }
    let mut shape = Vec::new();
    if *atomic {
        shape.push(
            "Keep commits atomic: one logical change per commit that builds and passes tests on its own."
                .to_string(),
        );
    }
    if let Some(max) = max_files {
        shape.push(format!("A commit touches at most {max} files."));
    }
    match batch {
        Some(CommitBatch::PerChange) => {
            shape.push("Commit each logical change as soon as it is complete.".to_string())
        }
        Some(CommitBatch::PerTurn) => {
            shape.push("Commit at the end of every turn that changed files.".to_string())
        }
        Some(CommitBatch::PerTask) => shape.push(
            "Leave changes uncommitted until the task is complete, then commit them together."
                .to_string(),
        ),
        None => {}
    }
    let mut rules: Vec<String> = [message, shape]
        .into_iter()
        .filter(|sentences| !sentences.is_empty())
        .map(|sentences| sentences.join(" "))
        .collect();
    if let Some(CommitAgent { agent, model }) = agent {
        let who = match model {
            Some(model) => format!("{agent} ({model})"),
            None => agent.clone(),
        };
        rules.push(format!(
            "Do not run `git commit` yourself: Nori's commit agent, {who}, writes and batches the commits by these rules."
        ));
    }
    rules
}

fn pr_rules(pr: &PrRules) -> Vec<String> {
    let PrRules {
        title,
        sections,
        max_lines,
        max_files,
        base,
        draft,
        upstream,
    } = pr;
    let mut content = Vec::new();
    if let Some(title) = title {
        content.push(format!("Pull request titles follow `{title}`."));
    }
    if !sections.is_empty() {
        let names = sections
            .iter()
            .map(|section| format!("\"{section}\""))
            .collect::<Vec<_>>()
            .join(", ");
        content.push(format!(
            "The description has these sections, in order: {names}."
        ));
    }
    let size = match (max_lines, max_files) {
        (Some(lines), Some(files)) => Some(format!("{lines} changed lines and {files} files")),
        (Some(lines), None) => Some(format!("{lines} changed lines")),
        (None, Some(files)) => Some(format!("{files} files")),
        (None, None) => None,
    };
    if let Some(size) = size {
        content.push(format!(
            "Keep each pull request within {size}; split larger work into a series of pull requests."
        ));
    }
    let mut delivery = Vec::new();
    if let Some(base) = base {
        delivery.push(format!("Target the `{base}` branch."));
    }
    if *draft {
        delivery.push("Open pull requests as drafts.".to_string());
    }
    match upstream {
        Some(Upstream::Never) => delivery
            .push("Never push or open a pull request; the user upstreams the work.".to_string()),
        Some(Upstream::Ask) => {
            delivery.push("Ask the user before you push or open a pull request.".to_string())
        }
        Some(Upstream::TestsPass) => delivery
            .push("Once the tests pass, push the branch and open the pull request.".to_string()),
        Some(Upstream::Done) => delivery.push(
            "When the task is complete, push the branch and open the pull request.".to_string(),
        ),
        None => {}
    }
    [content, delivery]
        .into_iter()
        .filter(|sentences| !sentences.is_empty())
        .map(|sentences| sentences.join(" "))
        .collect()
}

#[cfg(test)]
#[path = "render_tests.rs"]
mod tests;
