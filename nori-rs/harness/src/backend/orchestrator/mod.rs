//! Delegated tasks: any agent Nori runs can hand work to a child session on any configured agent,
//! so a Claude session can delegate to Codex and the other way round.
//!
//! The parent agent reaches this through the task tools of its `nori-client` MCP server. A task
//! is one turn of work on a child session, and a child session is a full harness session of its
//! own: its own agent process, approvals, and `nori-client` server, so a child can delegate in
//! turn. The user sees children as the parent's subagents: a child's
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
mod child_control;
mod launch;
mod managed_journal;
mod models;
mod native_rebind;
#[cfg(test)]
mod native_rebind_nested_tests;
mod parent;
mod probe;
mod program;
mod task;
#[cfg(test)]
mod tests;
mod tools;
mod wake;
mod worktree;

pub(crate) use native_rebind::NativeRebindSpec;

use std::collections::HashMap;
use std::collections::HashSet;
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
pub(crate) use tools::always_load;

/// The longest a tool call blocks waiting on tasks. Agents drop MCP calls that stay silent much
/// longer (Codex after 60 seconds); a longer task is delivered by the completion wake.
const MAX_WAIT: Duration = Duration::from_secs(50);
/// Default wait returns after this polling interval; the task continues and delivers its result.
const DEFAULT_WAIT: Duration = Duration::from_secs(45);

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
    pub(crate) record_transcript: bool,
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
            record_transcript: true,
        }
    }

    /// The seed of a child of this session, which runs with `config`.
    pub(crate) fn child(&self, config: Arc<crate::config::NoriConfig>) -> Self {
        Self {
            config,
            cli_version: self.cli_version.clone(),
            depth: self.depth.saturating_add(1),
            tree: Arc::clone(&self.tree),
            record_transcript: self.record_transcript,
        }
    }

    /// Mark a short-lived catalog probe as a session without a conversation log.
    pub(crate) fn without_transcript(mut self) -> Self {
        self.record_transcript = false;
        self
    }

    /// Whether the session was started as a delegated child.
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

/// Refresh installed agents except for the one whose primary startup session is being prepared.
/// That session records its advertised configuration directly when it starts.
pub fn refresh_agent_catalog_except(
    config: Arc<crate::config::NoriConfig>,
    cli_version: String,
    excluded_agent: String,
) {
    tokio::spawn(probe::refresh_excluding(
        config,
        cli_version,
        CATALOG_FRESH_FOR,
        Some(excluded_agent),
    ));
}

/// A model previously advertised by an agent in an ACP session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedAgentModel {
    pub value: String,
    pub name: String,
}

/// Return only models actually advertised by this agent in a previous session.
pub fn cached_agent_models(home: &std::path::Path, agent: &str) -> Vec<CachedAgentModel> {
    catalog::remembered(home, agent)
        .models
        .into_iter()
        .map(|model| CachedAgentModel {
            value: model.value,
            name: model.name,
        })
        .collect()
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
    program_backend: std::sync::OnceLock<std::sync::Weak<super::AcpBackend>>,
    events: mpsc::Sender<BackendEvent>,
    binding: std::sync::Mutex<Option<Binding>>,
    signals: std::sync::Mutex<Option<mpsc::UnboundedSender<Signal>>>,
    state: Mutex<State>,
    admission: Mutex<()>,
    generation: AtomicI64,
    /// Bumped whenever a task changes, for tool calls waiting on tasks.
    changed: watch::Sender<i64>,
}

/// Route a public descendant ID back to the session ID on its owning connection.
#[derive(Clone)]
pub(crate) struct DescendantRoute {
    pub(crate) handle: crate::runtime::HarnessHandle,
    pub(crate) session_id: acp::SessionId,
}

#[derive(Default)]
struct State {
    next_task: i64,
    next_child: i64,
    tasks: Vec<TaskRecord>,
    children: HashMap<ChildKey, ChildSlot>,
    /// The immediate child whose event stream announced each descendant view.
    descendants: HashMap<acp::SessionId, DescendantRoute>,
    /// Adopted view mappings kept inaccessible by the caller's gate until durability succeeds.
    pending_adoptions: HashSet<acp::SessionId>,
    by_client_request: HashMap<String, TaskId>,
    shut_down: bool,
}

enum ChildSlot {
    Starting,
    Ready(child::Child),
}

impl Orchestrator {
    pub(crate) async fn rewind_admission(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.inner.admission.lock().await
    }

    pub(crate) async fn has_pending_work(&self) -> bool {
        self.inner.state.lock().await.tasks.iter().any(|task| {
            !task.status.is_ended() || (task.delivery == Delivery::Owed && task.wake != Wake::Never)
        })
    }

    pub(crate) fn advance_generation(&self) {
        self.inner.generation.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn admission_generation(&self) -> i64 {
        self.inner.generation.load(Ordering::Acquire)
    }
    /// The orchestrator of a session, and the signals its backend must act on.
    pub(crate) fn new(
        seed: Seed,
        events: mpsc::Sender<BackendEvent>,
    ) -> (Self, mpsc::UnboundedReceiver<Signal>) {
        let (signals_tx, signals_rx) = mpsc::unbounded_channel();
        let inner = Inner {
            seed,
            program_backend: std::sync::OnceLock::new(),
            events,
            binding: std::sync::Mutex::new(None),
            signals: std::sync::Mutex::new(Some(signals_tx)),
            state: Mutex::new(State::default()),
            admission: Mutex::new(()),
            generation: AtomicI64::new(0),
            changed: watch::channel(0).0,
        };
        (
            Self {
                inner: Arc::new(inner),
            },
            signals_rx,
        )
    }

    /// Whether this is a delegated child's session, which works to its parent's task.
    pub(crate) fn is_child(&self) -> bool {
        self.inner.seed.is_child()
    }

    /// Whether the user trusts the session's project, so its own scripts may run.
    pub(crate) fn project_trusted(&self) -> bool {
        self.inner.seed.config.active_project.trust_level == Some(nori_config::TrustLevel::Trusted)
    }

    /// Hand `task` to `agent` (on `model`) in the background, as a delegation of this session's:
    /// how a workflow's commit agent writes the commits while the session's agent goes on.
    pub(crate) async fn delegate_background(
        &self,
        generation: i64,
        title: String,
        task: String,
        agent: String,
        model: Option<String>,
        review: bool,
    ) -> Result<(), String> {
        let _admission = self.rewind_admission().await;
        if generation != self.admission_generation() {
            return Err("The conversation was rewound before the workflow task started".to_owned());
        }
        self.inner
            .delegate_locked(launch::Delegation {
                task,
                agent: Some(agent),
                model,
                title: Some(title),
                role: Some(if review {
                    launch::Role::Review
                } else {
                    launch::Role::Implementation
                }),
                isolation: launch::Isolation::Shared,
                mode: launch::Mode::Background,
                timeout: None,
                client_request_id: None,
                options: Vec::new(),
                fork_context: None,
                fork_source: None,
                fork_entries: Vec::new(),
                parent_view: None,
            })
            .await
            .map(drop)
    }

    /// Continue a selected conversation prefix on a separate child session.
    pub(crate) async fn fork_history(
        &self,
        context: String,
        source: Option<(String, i64, i64)>,
        entries: Vec<crate::transcript::TranscriptLine>,
    ) -> Result<(), String> {
        self.inner
            .delegate_locked(launch::Delegation {
                task: "Continue from the conversation above.".to_owned(),
                agent: None,
                model: None,
                title: Some("History fork".to_owned()),
                role: None,
                isolation: launch::Isolation::Shared,
                mode: launch::Mode::Background,
                timeout: None,
                client_request_id: None,
                options: Vec::new(),
                fork_context: Some(context),
                fork_source: source,
                fork_entries: entries,
                parent_view: None,
            })
            .await
            .map(drop)
    }

    pub(crate) async fn owns_view(&self, session_id: &acp::SessionId) -> bool {
        let state = self.inner.state.lock().await;
        !state.pending_adoptions.contains(session_id)
            && state.tasks.iter().any(|task| &task.view_id == session_id)
    }

    /// Send a user's follow-up to an orchestrated child, queueing behind its active turn.
    pub(crate) async fn send_user_content_message_locked(
        &self,
        session_id: acp::SessionId,
        content: Vec<acp::ContentBlock>,
    ) -> Result<(), String> {
        self.send_user_content_locked(session_id, content, false)
            .await
    }

    pub(crate) async fn steer_user_content_message_locked(
        &self,
        session_id: acp::SessionId,
        content: Vec<acp::ContentBlock>,
    ) -> Result<(), String> {
        self.send_user_content_locked(session_id, content, true)
            .await
    }

    async fn send_user_content_locked(
        &self,
        session_id: acp::SessionId,
        content: Vec<acp::ContentBlock>,
        steer: bool,
    ) -> Result<(), String> {
        let text = content
            .iter()
            .filter_map(|block| match block {
                acp::ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let text = if text.trim().is_empty()
            && content
                .iter()
                .any(|block| !matches!(block, acp::ContentBlock::Text(_)))
        {
            "Attachment".to_owned()
        } else {
            text
        };
        let mut changed = self.inner.changed.subscribe();
        let previous = loop {
            let state = self.inner.state.lock().await;
            let task = state
                .tasks
                .iter()
                .rev()
                .find(|task| task.view_id == session_id)
                .ok_or_else(|| format!("No controlled child session {session_id}"))?;
            match state.children.get(&task.child) {
                Some(ChildSlot::Ready(_)) => break task.id.clone(),
                Some(ChildSlot::Starting) => {}
                None => return Err("The child session is no longer available".to_owned()),
            }
            drop(state);
            changed
                .changed()
                .await
                .map_err(|_| "The child session is no longer available".to_owned())?;
        };
        self.inner
            .send_locked(
                &previous,
                text,
                launch::Mode::Background,
                None,
                if steer {
                    launch::FollowUp::Steer
                } else {
                    launch::FollowUp::User
                },
                Some(content),
            )
            .await
            .map(drop)
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
            state.descendants.clear();
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

    async fn parent_for_view(&self, view: &acp::SessionId) -> Option<acp::SessionId> {
        let parent = self
            .state
            .lock()
            .await
            .tasks
            .iter()
            .rev()
            .find(|task| &task.view_id == view)
            .and_then(|task| task.parent_view.clone());
        parent.or_else(|| self.parent_session())
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
        parent_view: Option<acp::SessionId>,
        agent: String,
        title: String,
        label: String,
        state: SubagentState,
    ) {
        let Some(parent_session_id) = parent_view.or_else(|| self.parent_session()) else {
            return;
        };
        self.emit(SessionEvent::Nori(NoriEvent::Subagent(
            SubagentWire::Update(SubagentUpdate {
                parent_session_id,
                session_id: view_id.clone(),
                agent: nori_protocol::Patch::Set(agent),
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

    /// A user interruption ends the current turn but keeps its child view available.
    pub(super) async fn user_interrupted(&self, id: &TaskId) {
        if let Some(task) = self
            .state
            .lock()
            .await
            .tasks
            .iter_mut()
            .find(|task| &task.id == id)
        {
            task.wake = Wake::Never;
        }
    }

    pub(super) async fn task_running(&self, id: &TaskId) {
        let mut state = self.state.lock().await;
        if let Some(task) = state.tasks.iter_mut().find(|task| &task.id == id)
            && task.status == TaskStatus::Starting
        {
            task.status = TaskStatus::Running;
        }
        drop(state);
        self.announce_task(id).await;
        self.touch();
    }

    /// Record how a task ended, tell the user's view, and owe the result to the parent.
    pub(super) async fn task_ended(&self, id: &TaskId, end: TaskEnd, final_text: String) {
        let (view_id, parent_view, agent, title, label, outcome, wake, wake_never, withdrawn) = {
            let mut state = self.state.lock().await;
            let Some(task) = state.tasks.iter_mut().find(|task| &task.id == id) else {
                return;
            };
            if task.status.is_ended() {
                return;
            }
            task.result = final_text;
            task.delivery = match (task.wake, &end) {
                (Wake::Never, _) => Delivery::Waived,
                (_, TaskEnd::Cancelled) => Delivery::Waived,
                (_, TaskEnd::Completed { .. } | TaskEnd::Failed { .. }) => Delivery::Owed,
            };
            task.status = TaskStatus::Ended(end);
            task.ended_at = Some(Instant::now());
            task.awaiting_approval = false;
            self.seed.tree.running.fetch_sub(1, Ordering::AcqRel);
            (
                task.view_id.clone(),
                task.parent_view.clone(),
                task.agent.clone(),
                task.view_title(),
                task.title.clone(),
                task.subagent_outcome(),
                task.delivery == Delivery::Owed && task.wake == Wake::Always,
                task.wake == Wake::Never,
                std::mem::take(&mut task.relayed_approvals),
            )
        };
        if !withdrawn.is_empty() {
            self.signal(Signal::Withdraw(withdrawn));
        }
        if let Some(outcome) = outcome {
            let another_active = self
                .state
                .lock()
                .await
                .tasks
                .iter()
                .any(|task| task.view_id == view_id && !task.status.is_ended());
            let state = if another_active {
                SubagentState::Running
            } else if wake_never
                && !matches!(outcome, nori_protocol::SubagentOutcome::Failed { .. })
            {
                SubagentState::Idle
            } else {
                SubagentState::Ended(outcome)
            };
            self.announce(&view_id, parent_view, agent, title, label, state)
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
                .find(|task| &task.id == id && !task.adoption_anchor)
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
