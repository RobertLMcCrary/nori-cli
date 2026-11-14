//! The completion wake: how finished tasks reach a parent that is not waiting on them.

use super::task::TaskSnapshot;

/// Results are inlined in a wake until together they reach this size; the rest are named, to be
/// read with `task_status`.
const INLINE_RESULT_BYTES: usize = 16 * 1024;

/// The hint returned for a task that runs in the background.
pub(super) const DELIVERED_LATER: &str = "The result will be delivered to you as a message when the task finishes. Keep working on \
     something else or end your turn; do not poll.";

/// The hint returned for a finished task whose result did not fit in the answer.
pub(super) const READ_SEPARATELY: &str =
    "Finished, but its result did not fit in this answer: read it with task_status.";

/// The hint returned for a task a wait gave up on.
pub(super) const STILL_RUNNING: &str = "Still running. Its result will be delivered to you as a message when it finishes; keep \
     working or end your turn rather than waiting again.";

/// The hidden prompt that hands the parent the results of tasks that finished unread.
pub(super) fn wake_text(tasks: impl Iterator<Item = TaskSnapshot>) -> String {
    let mut text = String::from(
        "<delegated_task_results>\nTasks you delegated have finished. Continue your work with \
         their results; the user sees each child's work already, so do not repeat it back in \
         full.\n",
    );
    let mut inlined = 0;
    for task in tasks {
        let mut attributes = format!(
            "id=\"{}\" agent=\"{}\" status=\"{}\"",
            task.task_id,
            escape(&task.agent),
            task.status
        );
        if let Some(model) = &task.model {
            attributes.push_str(&format!(" model=\"{}\"", escape(model)));
        }
        if let Some(stop_reason) = task.stop_reason {
            attributes.push_str(&format!(" stop_reason=\"{stop_reason}\""));
        }
        attributes.push_str(&format!(" title=\"{}\"", escape(&task.title)));
        if let Some(worktree) = &task.worktree {
            attributes.push_str(&format!(" worktree=\"{}\"", escape(worktree)));
        }
        text.push_str(&format!("\n<task {attributes}>\n"));
        if let Some(error) = &task.error {
            text.push_str(&format!("Failed: {error}\n"));
        }
        if !task.files_changed.is_empty() {
            text.push_str(&format!(
                "Files changed: {}\n",
                task.files_changed.join(", ")
            ));
        }
        let result = task.result.as_deref().unwrap_or_default().trim();
        if result.is_empty() {
            text.push_str("(The child ended without a final message.)\n");
        } else if inlined + result.len() <= INLINE_RESULT_BYTES {
            inlined += result.len();
            text.push_str(result);
            text.push('\n');
        } else {
            text.push_str(&format!(
                "(Result not inlined: {} bytes. Read it with task_status.)\n",
                result.len()
            ));
        }
        text.push_str("</task>\n");
    }
    text.push_str("</delegated_task_results>");
    text
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::orchestrator::task::TaskId;
    use pretty_assertions::assert_eq;

    fn finished(id: i64, result: &str) -> TaskSnapshot {
        TaskSnapshot {
            task_id: TaskId::numbered(id),
            agent: "codex".to_owned(),
            model: Some("GPT-5.1".to_owned()),
            title: "Read \"a.txt\"".to_owned(),
            status: "completed",
            stop_reason: Some("end_turn"),
            error: None,
            result: Some(result.to_owned()),
            awaiting_approval: None,
            activity: None,
            tool_calls: 1,
            files_changed: Vec::new(),
            elapsed_seconds: 3,
            worktree: None,
            next: None,
        }
    }

    #[test]
    fn results_are_inlined_until_the_budget_is_spent() {
        let big = "x".repeat(INLINE_RESULT_BYTES);
        let text = wake_text([finished(1, "alpha"), finished(2, &big)].into_iter());
        assert!(text.contains(
            "<task id=\"task-1\" agent=\"codex\" status=\"completed\" model=\"GPT-5.1\" stop_reason=\"end_turn\" title=\"Read &quot;a.txt&quot;\">\nalpha\n</task>"
        ));
        assert!(text.contains("(Result not inlined: 16384 bytes. Read it with task_status.)"));
        assert_eq!(text.lines().last(), Some("</delegated_task_results>"));
    }
}
