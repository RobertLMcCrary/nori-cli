//! The completion wake: how finished tasks reach a parent that is not waiting on them.

use std::time::Duration;
use std::time::Instant;

use super::Inner;
use super::MAX_WAIT;
use super::Orchestrator;
use super::WakeTurn;
use super::task::Delivery;
use super::task::TaskId;
use super::task::TaskRecord;
use super::task::TaskSnapshot;
use super::task::Wake;

/// Results are inlined in a wake until together they reach this size; the rest are named, to be
/// read with `task_status`.
const INLINE_RESULT_BYTES: usize = 16 * 1024;

/// The hint returned for a task that runs in the background.
pub(super) const DELIVERED_LATER: &str = "The result will be delivered to you as a message when the task finishes. Keep working on \
     something else or end your turn; do not poll.";

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

impl Orchestrator {
    /// A completion wake, if results are owed: every owed result is marked delivered.
    pub(crate) async fn take_wake(&self) -> Option<WakeTurn> {
        let mut state = self.inner.state.lock().await;
        let owed: Vec<&mut TaskRecord> = state
            .tasks
            .iter_mut()
            .filter(|task| task.status.is_ended() && task.delivery == Delivery::Owed)
            .filter(|task| task.wake == Wake::Always)
            .collect();
        if owed.is_empty() {
            return None;
        }
        let now = Instant::now();
        let text = wake_text(owed.iter().map(|task| task.snapshot(now)));
        let tasks = owed
            .into_iter()
            .map(|task| {
                task.delivery = Delivery::Delivered;
                nori_protocol::DeliveredTask {
                    task_id: task.id.to_string(),
                    title: task.title.clone(),
                    agent: task.agent_name.clone(),
                }
            })
            .collect();
        Some(WakeTurn {
            text,
            delivered: nori_protocol::TaskResultsDelivered { tasks },
        })
    }
}

impl Inner {
    /// Wait until every task in `ids` has ended or `timeout` passes, then report them. Ended
    /// tasks are delivered by this; those still running will wake the parent when they end.
    pub(super) async fn wait(
        &self,
        ids: &[TaskId],
        timeout: Duration,
    ) -> Result<Vec<TaskSnapshot>, String> {
        let deadline = tokio::time::Instant::now() + timeout.min(MAX_WAIT);
        let mut changed = self.changed.subscribe();
        {
            // Whatever happens to this call, a task it was waiting on is not lost: it wakes the
            // parent unless this call returns its result.
            let mut state = self.state.lock().await;
            for id in ids {
                let task = state
                    .tasks
                    .iter_mut()
                    .find(|task| &task.id == id && !task.adoption_anchor)
                    .ok_or_else(|| format!("there is no task `{id}`"))?;
                if task.wake != Wake::Never {
                    task.wake = Wake::Always;
                }
            }
        }
        loop {
            // A child waiting for the user's approval cannot finish until the user answers, so
            // the waiter hears of it now.
            let settled = {
                let state = self.state.lock().await;
                let waited = || state.tasks.iter().filter(|task| ids.contains(&task.id));
                waited().all(|task| task.status.is_ended())
                    || waited().any(|task| task.awaiting_approval)
            };
            if settled
                || tokio::time::timeout_at(deadline, changed.changed())
                    .await
                    .is_err()
            {
                break;
            }
        }
        let mut state = self.state.lock().await;
        let now = Instant::now();
        let mut snapshots = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(task) = state.tasks.iter_mut().find(|task| &task.id == id) {
                let mut snapshot = task.snapshot(now);
                if task.status.is_ended() {
                    if task.delivery == Delivery::Owed {
                        task.delivery = Delivery::Delivered;
                    }
                } else {
                    snapshot.next = Some(STILL_RUNNING);
                }
                snapshots.push(snapshot);
            }
        }
        Ok(snapshots)
    }
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
