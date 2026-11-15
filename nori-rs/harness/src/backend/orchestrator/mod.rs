//! Delegated tasks: any agent Nori runs can hand work to a child session on any configured agent,
//! so a Claude session can delegate to Codex and the other way round.
//!
//! The parent agent reaches this through the task tools of its `nori-client` MCP server. A task
//! is one turn of work on a child session, and a child session is a full harness session of its
//! own: its own agent process, approvals, and `nori-client` server, so a child can delegate in
//! turn, up to the depth limit. The user sees children as the parent's subagents: a child's
//! updates are relayed into the parent's event stream under a session id unique to the task, and
//! its permission requests are asked in the parent conversation, named after the child.
//!
//! Results come back without polling. A task that finishes while no tool call is waiting on it is
//! delivered to the parent agent in a hidden turn as soon as the parent is idle: the completion
//! wake. Reading a finished task with `task_status` or `task_wait` delivers it instead, so every
//! result reaches the parent once. A child never gets broader permissions than its parent: it
//! starts with the parent's current approval policy.

mod approvals;
mod brief;
mod capabilities;
mod catalog;
mod child;
mod launch;
mod models;
mod parent;
mod probe;
mod task;
#[cfg(test)]
mod tests;
mod tools;
mod wake;
mod worktree;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use nori_config::AskForApproval;
use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;
use nori_protocol::SubagentState;
use nori_protocol::SubagentUpdate;
use nori_protocol::SubagentWire;
use nori_protocol::acp::v1 as acp;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::watch;

use super::BackendEvent;
use crate::connection::DelegatedRequest;
pub(crate) use catalog::remember as remember_models;
use task::Delivery;
use task::TaskEnd;
pub(crate) use task::TaskId;
use task::TaskRecord;
use task::TaskSnapshot;
use task::TaskStatus;
use task::Wake;
pub(crate) use tools::INSTRUCTIONS as TASK_INSTRUCTIONS;

/// Tasks running at once across a whole tree of delegations.
const MAX_RUNNING_TASKS: i64 = 8;
/// How deep delegations nest: the root's children are at depth 1, and a session at this depth
/// cannot delegate.
const MAX_DEPTH: i64 = 3;
/// The longest a tool call blocks waiting on tasks. Agents drop MCP calls that stay silent much
/// longer (Codex after 60 seconds); a longer task is delivered by the completion wake.
const MAX_WAIT: Duration = Duration::from_secs(50);
const DEFAULT_WAIT: Duration = Duration::from_secs(45);
/// The most result text one `task_wait` answer carries.
const MAX_RETURNED_BYTES: usize = 64 * 1024;

/// Limits shared by every session in one tree of delegations.
#[derive(Debug)]
pub(crate) struct Tree {
    running: AtomicI64,
}

/// What a session needs to delegate: the configuration its children start from and where it sits
/// in its tree of delegations.
#[derive(Debug, Clone)]
pub(crate) struct Seed {
    pub(crate) config: Arc<crate::config::NoriConfig>,
    pub(crate) cli_version: String,
    pub(crate) depth: i64,
    pub(crate) tree: Arc<Tree>,
}

impl Seed {
    /// A session the user started, at the root of its own tree.
    pub(crate) fn root(config: Arc<crate::config::NoriConfig>, cli_version: String) -> Self {
        Self {
            config,
            cli_version,
            depth: 0,
            tree: Arc::new(Tree {
                running: AtomicI64::new(0),
            }),
        }
    }

    /// The seed of a child of this session, which runs with `config`.
    pub(crate) fn child(&self, config: Arc<crate::config::NoriConfig>) -> Self {
        Self {
            config,
            cli_version: self.cli_version.clone(),
            depth: self.depth + 1,
            tree: Arc::clone(&self.tree),
        }
    }

    /// Whether the session is a child, which keeps no transcript of its own: the parent's shows
    /// what it did.
    pub(crate) fn is_child(&self) -> bool {
        self.depth > 0
    }
}

/// A completion wake: the hidden prompt, and what it hands over.
pub(crate) struct WakeTurn {
    pub(crate) text: String,
    pub(crate) delivered: nori_protocol::TaskResultsDelivered,
}

/// Agents a typing user may soon delegate to are re-read when their entry is this old.
const CATALOG_FRESH_FOR: Duration = Duration::from_secs(600);

/// Re-read, in the background, what each installed agent offers, for agents not read in the last
/// ten minutes: called as the user starts typing, so delegation sees current models.
pub fn refresh_agent_catalog(config: Arc<crate::config::NoriConfig>, cli_version: String) {
    tokio::spawn(probe::refresh(config, cli_version, CATALOG_FRESH_FOR));
}

/// What the orchestrator needs the parent's backend to do.
pub(crate) enum Signal {
    /// A child asks for permission; the parent asks the user in its own conversation, and the
    /// answer goes back to the child.
    Approval(Box<DelegatedRequest>),
    /// Results are owed to the parent agent; deliver them when the parent is idle.
    ResultsOwed,
    /// These relayed permission requests belong to a task that has ended; nobody needs them
    /// answered any more.
    Withdraw(Vec<acp::RequestId>),
}

/// The parent session's side of the orchestrator, bound once the session exists.
pub(crate) struct Binding {
    pub(crate) session_id: acp::SessionId,
    pub(crate) approval_policy: watch::Receiver<AskForApproval>,
}

/// The delegated tasks of one session.
#[derive(Clone)]
pub(crate) struct Orchestrator {
    inner: Arc<Inner>,
}

/// A child session, by the order it was started in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct ChildKey(i64);

pub(super) struct Inner {
    seed: Seed,
    events: mpsc::Sender<BackendEvent>,
    binding: std::sync::Mutex<Option<Binding>>,
    signals: std::sync::Mutex<Option<mpsc::UnboundedSender<Signal>>>,
    state: Mutex<State>,
    /// Bumped whenever a task changes, for tool calls waiting on tasks.
    changed: watch::Sender<i64>,
}

#[derive(Default)]
struct State {
    next_task: i64,
    next_child: i64,
    tasks: Vec<TaskRecord>,
    children: HashMap<ChildKey, ChildSlot>,
    by_client_request: HashMap<String, TaskId>,
    shut_down: bool,
}

enum ChildSlot {
    Starting,
    Ready(child::Child),
}

impl Orchestrator {
    /// The orchestrator of a session, and the signals its backend must act on.
    pub(crate) fn new(
        seed: Seed,
        events: mpsc::Sender<BackendEvent>,
    ) -> (Self, mpsc::UnboundedReceiver<Signal>) {
        let (signals_tx, signals_rx) = mpsc::unbounded_channel();
        let inner = Inner {
            seed,
            events,
            binding: std::sync::Mutex::new(None),
            signals: std::sync::Mutex::new(Some(signals_tx)),
            state: Mutex::new(State::default()),
            changed: watch::channel(0).0,
        };
        (
            Self {
                inner: Arc::new(inner),
            },
            signals_rx,
        )
    }

    /// Bind the parent session once it exists, and again when `/compact` replaces it.
    pub(crate) fn bind(&self, binding: Binding) {
        if let Ok(mut slot) = self.inner.binding.lock() {
            *slot = Some(binding);
        }
    }

    /// The user stopped the parent: cancel every running task, and hold results that came in
    /// before the stop until the user's next turn, so none of them starts a turn of its own.
    pub(crate) async fn stop(&self) {
        let running: Vec<TaskId> = {
            let mut state = self.inner.state.lock().await;
            for task in &mut state.tasks {
                if task.status.is_ended() && task.delivery == Delivery::Owed {
                    task.wake = Wake::AfterNextTurn;
                }
            }
            state
                .tasks
                .iter()
                .filter(|task| !task.status.is_ended())
                .map(|task| task.id.clone())
                .collect()
        };
        futures::future::join_all(running.iter().map(|task| self.inner.cancel(task))).await;
    }

    /// The user sent a prompt: results held by a stop are owed again, after this turn.
    pub(crate) async fn release_held(&self) {
        let mut state = self.inner.state.lock().await;
        for task in &mut state.tasks {
            if task.wake == Wake::AfterNextTurn {
                task.wake = Wake::Always;
            }
        }
    }

    /// End every child and stop signalling the backend.
    pub(crate) async fn shutdown(&self) {
        // First, so nothing starts while the running tasks are cancelled.
        self.inner.state.lock().await.shut_down = true;
        self.stop().await;
        let children: Vec<child::Child> = {
            let mut state = self.inner.state.lock().await;
            state
                .children
                .drain()
                .filter_map(|(_, slot)| match slot {
                    ChildSlot::Ready(child) => Some(child),
                    ChildSlot::Starting => None,
                })
                .collect()
        };
        // Together, so quitting takes as long as the slowest child rather than all of them.
        futures::future::join_all(children.iter().map(|child| child.harness.shutdown())).await;
        if let Ok(mut signals) = self.inner.signals.lock() {
            signals.take();
        }
        if let Ok(mut binding) = self.inner.binding.lock() {
            binding.take();
        }
    }

    pub(super) fn inner(&self) -> &Arc<Inner> {
        &self.inner
    }
}

impl Inner {
    pub(super) async fn emit(&self, event: SessionEvent) {
        let _ = self.events.send(BackendEvent::Public(event)).await;
    }

    fn signal(&self, signal: Signal) {
        if let Ok(signals) = self.signals.lock()
            && let Some(signals) = signals.as_ref()
        {
            let _ = signals.send(signal);
        }
    }

    fn parent_session(&self) -> Option<acp::SessionId> {
        self.binding
            .lock()
            .ok()?
            .as_ref()
            .map(|binding| binding.session_id.clone())
    }

    /// The parent's approval policy now: a child never gets a broader one.
    fn approval_policy(&self) -> AskForApproval {
        self.binding
            .lock()
            .ok()
            .and_then(|binding| {
                binding
                    .as_ref()
                    .map(|binding| *binding.approval_policy.borrow())
            })
            .unwrap_or(self.seed.config.approval_policy)
    }

    fn touch(&self) {
        self.changed.send_modify(|revision| *revision += 1);
    }

    async fn announce(
        &self,
        view_id: &acp::SessionId,
        title: String,
        label: String,
        state: SubagentState,
    ) {
        let Some(parent_session_id) = self.parent_session() else {
            return;
        };
        self.emit(SessionEvent::Nori(NoriEvent::Subagent(
            SubagentWire::Update(SubagentUpdate {
                parent_session_id,
                session_id: view_id.clone(),
                title: nori_protocol::Patch::Set(title),
                description: nori_protocol::Patch::Set(label),
                state: nori_protocol::Patch::Set(state),
            }),
        )))
        .await;
    }

    pub(super) async fn task_has_ended(&self, id: &TaskId) -> bool {
        let state = self.state.lock().await;
        state
            .tasks
            .iter()
            .any(|task| &task.id == id && task.status.is_ended())
    }

    pub(super) async fn task_running(&self, id: &TaskId) {
        let mut state = self.state.lock().await;
        if let Some(task) = state.tasks.iter_mut().find(|task| &task.id == id)
            && task.status == TaskStatus::Starting
        {
            task.status = TaskStatus::Running;
        }
        drop(state);
        self.touch();
    }

    /// Record how a task ended, tell the user's view, and owe the result to the parent.
    pub(super) async fn task_ended(&self, id: &TaskId, end: TaskEnd, final_text: String) {
        let (view_id, title, label, outcome, wake, withdrawn) = {
            let mut state = self.state.lock().await;
            let Some(task) = state.tasks.iter_mut().find(|task| &task.id == id) else {
                return;
            };
            if task.status.is_ended() {
                return;
            }
            task.result = task::bounded_result(&final_text);
            task.delivery = match end {
                TaskEnd::Cancelled => Delivery::Waived,
                TaskEnd::Completed { .. } | TaskEnd::Failed { .. } => Delivery::Owed,
            };
            task.status = TaskStatus::Ended(end);
            task.ended_at = Some(Instant::now());
            task.awaiting_approval = false;
            self.seed.tree.running.fetch_sub(1, Ordering::AcqRel);
            (
                task.view_id.clone(),
                task.view_title(),
                task.title.clone(),
                task.subagent_outcome(),
                task.delivery == Delivery::Owed && task.wake == Wake::Always,
                std::mem::take(&mut task.relayed_approvals),
            )
        };
        if !withdrawn.is_empty() {
            self.signal(Signal::Withdraw(withdrawn));
        }
        if let Some(outcome) = outcome {
            self.announce(&view_id, title, label, SubagentState::Ended(outcome))
                .await;
        }
        self.touch();
        if wake {
            self.signal(Signal::ResultsOwed);
        }
    }

    /// Stop a task. A task that has ended keeps its result.
    pub(super) async fn cancel(&self, id: &TaskId) -> Result<TaskSnapshot, String> {
        let child = {
            let state = self.state.lock().await;
            let task = state
                .tasks
                .iter()
                .find(|task| &task.id == id)
                .ok_or_else(|| format!("there is no task `{id}`"))?;
            if task.status.is_ended() {
                return Ok(task.snapshot(Instant::now()));
            }
            match state.children.get(&task.child) {
                Some(ChildSlot::Ready(child)) => Some(child.clone()),
                Some(ChildSlot::Starting) | None => None,
            }
        };
        self.task_ended(id, TaskEnd::Cancelled, String::new()).await;
        if let Some(child) = child {
            let _ = child.harness.cancel().await;
        }
        let state = self.state.lock().await;
        state
            .tasks
            .iter()
            .find(|task| &task.id == id)
            .map(|task| task.snapshot(Instant::now()))
            .ok_or_else(|| format!("there is no task `{id}`"))
    }
}
