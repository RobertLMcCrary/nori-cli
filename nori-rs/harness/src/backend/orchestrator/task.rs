//! One delegated task: a turn of work on a child session, from request to delivered result.

use std::time::Instant;

use nori_protocol::SubagentOutcome;
use nori_protocol::acp::v1 as acp;
use serde::Serialize;

/// A task's id as the parent agent sees it: `task-1`, `task-2`, ... in the order they were
/// delegated. Unique within one parent session.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub(crate) struct TaskId(String);

impl TaskId {
    pub(super) fn numbered(number: i64) -> Self {
        Self(format!("task-{number}"))
    }

    pub(super) fn parse(text: &str) -> Self {
        Self(text.trim().to_owned())
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Where a task is in its life. A task that has ended never runs again: a follow-up message to its
/// child is a new task on the same session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TaskStatus {
    /// The child's agent is being started or its session created.
    Starting,
    Running,
    Ended(TaskEnd),
}

/// How a task ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TaskEnd {
    Completed {
        stop_reason: acp::StopReason,
    },
    Failed {
        error: String,
    },
    /// Stopped by the parent agent, the user, or the parent session ending.
    Cancelled,
}

impl TaskStatus {
    pub(super) fn is_ended(&self) -> bool {
        matches!(self, Self::Ended(_))
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Ended(TaskEnd::Completed {
                stop_reason: acp::StopReason::EndTurn,
            }) => "completed",
            // Stopped by a limit or a refusal rather than finishing.
            Self::Ended(TaskEnd::Completed { .. }) => "incomplete",
            Self::Ended(TaskEnd::Failed { .. }) => "failed",
            Self::Ended(TaskEnd::Cancelled) => "cancelled",
        }
    }
}

/// Whether a finished task's result still has to reach the parent agent.
///
/// Each result reaches the parent exactly once: as the answer to a tool call that read it, or in a
/// completion wake. A cancelled task is never delivered: whoever cancelled it knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Delivery {
    /// Not finished, or finished and not yet read.
    Owed,
    /// The parent agent has the result.
    Delivered,
    /// Nobody is owed the result.
    Waived,
}

/// When a finished task wakes the parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Wake {
    /// Whenever it finishes unread. The default for tasks that run in the background.
    Always,
    /// Only if the call waiting on it gave up first: a wait that returns the result delivers it.
    IfUnwaited,
    /// A user-created history fork does not send its result to the parent.
    Never,
    /// Held by the user stopping the parent; owed again once the user's next turn ends.
    AfterNextTurn,
}

/// What a running task's child is doing, written by its actor as the child's events arrive.
#[derive(Debug, Default)]
pub(super) struct Progress {
    /// The latest tool call's title, or the last line the child wrote.
    pub(super) activity: String,
    pub(super) tool_calls: i64,
    /// Files the child's tool calls changed, in the order they were first changed.
    pub(super) files_changed: Vec<String>,
}

pub(super) type SharedProgress = std::sync::Arc<std::sync::Mutex<Progress>>;

/// Everything known about one task.
#[derive(Debug)]
pub(super) struct TaskRecord {
    pub(super) id: TaskId,
    /// An idle adopted session needs a follow-up anchor, but is not an agent task.
    pub(super) adoption_anchor: bool,
    /// The child session the task runs on; follow-ups reuse it.
    pub(super) child: super::ChildKey,
    /// The session id the user's view shows this task under, unique to the task.
    pub(super) view_id: acp::SessionId,
    /// The visible parent when a native child owns this fork.
    pub(super) parent_view: Option<acp::SessionId>,
    pub(super) agent: String,
    pub(super) agent_name: String,
    pub(super) model: Option<String>,
    pub(super) title: String,
    pub(super) status: TaskStatus,
    /// The child's final message: what it said after its last tool call, or its last words before
    /// it failed.
    pub(super) result: String,
    pub(super) delivery: Delivery,
    pub(super) wake: Wake,
    /// A permission request of the child's is waiting for the user.
    pub(super) awaiting_approval: bool,
    /// The parent's ids for the child's permission requests asked of the user.
    pub(super) relayed_approvals: Vec<acp::RequestId>,
    pub(super) started_at: Instant,
    pub(super) ended_at: Option<Instant>,
    /// The worktree the child works in, when it was given one.
    pub(super) worktree: Option<std::path::PathBuf>,
    pub(super) progress: SharedProgress,
}

impl TaskRecord {
    /// A new task on the same child, not started yet, shown under `view_id`.
    pub(super) fn follow_up(&self, view_id: acp::SessionId) -> Self {
        Self {
            id: self.id.clone(),
            adoption_anchor: false,
            child: self.child,
            view_id,
            parent_view: self.parent_view.clone(),
            agent: self.agent.clone(),
            agent_name: self.agent_name.clone(),
            model: self.model.clone(),
            title: self.title.clone(),
            status: TaskStatus::Starting,
            result: String::new(),
            delivery: Delivery::Owed,
            wake: self.wake,
            awaiting_approval: false,
            relayed_approvals: Vec::new(),
            started_at: Instant::now(),
            ended_at: None,
            worktree: self.worktree.clone(),
            progress: SharedProgress::default(),
        }
    }

    /// The child's agent and model, shown next to the task's title.
    pub(super) fn view_title(&self) -> String {
        match &self.model {
            Some(model) => format!("{} · {model}", self.agent_name),
            None => self.agent_name.clone(),
        }
    }

    /// How the user's view shows the task's end, once it has ended.
    pub(super) fn subagent_outcome(&self) -> Option<SubagentOutcome> {
        match &self.status {
            TaskStatus::Ended(TaskEnd::Completed { stop_reason }) => {
                Some(SubagentOutcome::Completed {
                    stop_reason: Some(stop_reason_label(*stop_reason).to_owned()),
                })
            }
            TaskStatus::Ended(TaskEnd::Failed { error }) => Some(SubagentOutcome::Failed {
                reason: Some(error.clone()),
            }),
            TaskStatus::Ended(TaskEnd::Cancelled) => Some(SubagentOutcome::Cancelled),
            TaskStatus::Starting | TaskStatus::Running => None,
        }
    }

    /// What a tool call that reads this task returns.
    pub(super) fn snapshot(&self, now: Instant) -> TaskSnapshot {
        let elapsed = self.ended_at.unwrap_or(now) - self.started_at;
        let (stop_reason, error) = match &self.status {
            TaskStatus::Ended(TaskEnd::Completed { stop_reason }) => {
                (Some(stop_reason_label(*stop_reason)), None)
            }
            TaskStatus::Ended(TaskEnd::Failed { error }) => (None, Some(error.clone())),
            TaskStatus::Starting | TaskStatus::Running | TaskStatus::Ended(TaskEnd::Cancelled) => {
                (None, None)
            }
        };
        TaskSnapshot {
            task_id: self.id.clone(),
            agent: self.agent.clone(),
            model: self.model.clone(),
            title: self.title.clone(),
            status: self.status.label(),
            stop_reason,
            error,
            result: self.status.is_ended().then(|| self.result.clone()),
            awaiting_approval: self.awaiting_approval.then_some(true),
            activity: (!self.status.is_ended())
                .then(|| {
                    self.progress
                        .lock()
                        .map(|progress| progress.activity.clone())
                        .ok()
                })
                .flatten()
                .filter(|activity| !activity.is_empty()),
            tool_calls: self
                .progress
                .lock()
                .map_or(0, |progress| progress.tool_calls),
            files_changed: self
                .progress
                .lock()
                .map(|progress| progress.files_changed.clone())
                .unwrap_or_default(),
            elapsed_seconds: elapsed.as_secs_f64().round() as i64,
            worktree: self
                .worktree
                .as_ref()
                .map(|path| path.display().to_string()),
            next: None,
        }
    }
}

/// A task as the tools report it.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct TaskSnapshot {
    pub(crate) task_id: TaskId,
    pub(crate) agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    pub(crate) title: String,
    pub(crate) status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stop_reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
    /// The child's final message, once the task has ended.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) awaiting_approval: Option<bool>,
    /// While it runs: the child's latest tool call or line of text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) activity: Option<String>,
    pub(crate) tool_calls: i64,
    /// Files the child's edit tools changed; edits made by its shell commands are not seen.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) files_changed: Vec<String>,
    pub(crate) elapsed_seconds: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) worktree: Option<String>,
    /// What the parent should do next, when that is not obvious from the status.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) next: Option<&'static str>,
}

pub(super) fn stop_reason_label(reason: acp::StopReason) -> &'static str {
    match reason {
        acp::StopReason::EndTurn => "end_turn",
        acp::StopReason::MaxTokens => "max_tokens",
        acp::StopReason::MaxTurnRequests => "max_turn_requests",
        acp::StopReason::Refusal => "refusal",
        acp::StopReason::Cancelled => "cancelled",
        // The schema's stop reasons are not exhaustive.
        _ => "other",
    }
}
