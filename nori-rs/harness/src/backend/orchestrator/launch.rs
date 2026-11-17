//! Starting tasks: on a new child, or as a follow-up on the child of an earlier task.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use nori_protocol::SubagentState;
use nori_protocol::acp::v1 as acp;

use super::ChildKey;
use super::ChildSlot;
use super::DEFAULT_WAIT;
use super::Inner;
use super::brief;
use super::child;
use super::task::Delivery;
use super::task::TaskEnd;
use super::task::TaskId;
use super::task::TaskRecord;
use super::task::TaskSnapshot;
use super::task::TaskStatus;
use super::task::Wake;
use super::wake;
use super::worktree;

/// What kind of work a task is, which frames the child's instructions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(rename_all = "snake_case")]
pub(super) enum Role {
    Implementation,
    Research,
    Review,
    Design,
    Test,
    General,
}

/// Where the child works.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, rmcp::schemars::JsonSchema,
)]
#[schemars(crate = "rmcp::schemars")]
#[serde(rename_all = "snake_case")]
pub(super) enum Isolation {
    /// In the parent's working directory.
    #[default]
    Shared,
    /// In a new git worktree checked out at the parent's current commit.
    Worktree,
}

/// Whether the call that starts a task waits for it.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, rmcp::schemars::JsonSchema,
)]
#[schemars(crate = "rmcp::schemars")]
#[serde(rename_all = "snake_case")]
pub(super) enum Mode {
    /// Return at once; the result arrives in a completion wake.
    #[default]
    Background,
    /// Block until the task ends or the timeout passes.
    Wait,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum FollowUp {
    Tool,
    User,
    Steer,
}

/// A task to start on a new child.
pub(super) struct Delegation {
    pub(super) task: String,
    pub(super) agent: Option<String>,
    pub(super) model: Option<String>,
    pub(super) title: Option<String>,
    pub(super) role: Option<Role>,
    pub(super) isolation: Isolation,
    pub(super) mode: Mode,
    pub(super) timeout: Option<Duration>,
    pub(super) client_request_id: Option<String>,
    /// Other settings of the child's agent, by setting and value.
    pub(super) options: Vec<(String, String)>,
    pub(super) fork_context: Option<String>,
    pub(super) fork_source: Option<(String, i64, i64)>,
    pub(super) fork_entries: Vec<crate::transcript::TranscriptLine>,
    pub(super) parent_view: Option<acp::SessionId>,
}

impl Inner {
    /// Count a running task for status reporting. This is not an admission limit.
    pub(super) fn count_running_task(&self) {
        self.seed.tree.running.fetch_add(1, Ordering::AcqRel);
    }

    /// Start a task on a new child.
    pub(super) async fn delegate(
        self: &Arc<Self>,
        delegation: Delegation,
    ) -> Result<TaskSnapshot, String> {
        let generation = self.generation.load(std::sync::atomic::Ordering::Acquire);
        let _admission = self.admission.lock().await;
        if generation != self.generation.load(std::sync::atomic::Ordering::Acquire) {
            return Err("The conversation was rewound before delegation started".to_owned());
        }
        self.delegate_locked(delegation).await
    }

    pub(super) async fn delegate_locked(
        self: &Arc<Self>,
        delegation: Delegation,
    ) -> Result<TaskSnapshot, String> {
        if delegation.task.trim().is_empty() {
            return Err("`task` is empty: say what the child should do".to_owned());
        }
        if let Some(existing) = self
            .requested_before(delegation.client_request_id.as_deref())
            .await
        {
            return self.status(&existing, false).await;
        }
        let agent = delegation
            .agent
            .clone()
            .unwrap_or_else(|| self.seed.config.active_agent.clone());
        let agent_info = crate::registry::list_available_agents()
            .into_iter()
            .find(|candidate| {
                candidate.agent_name.eq_ignore_ascii_case(agent.trim())
                    || candidate.display_name.eq_ignore_ascii_case(agent.trim())
            })
            .ok_or_else(|| {
                let known: Vec<String> = crate::registry::list_available_agents()
                    .into_iter()
                    .filter(|candidate| candidate.is_installed)
                    .map(|candidate| format!("`{}`", candidate.agent_name))
                    .collect();
                format!(
                    "there is no agent `{agent}`; the agents are {}",
                    known.join(", ")
                )
            })?;
        if !agent_info.is_installed {
            return Err(format!("{} is not installed", agent_info.display_name));
        }
        let injects_model = crate::registry::get_agent_config(&agent_info.agent_name)
            .is_ok_and(|agent| agent.supports_model_injection());
        // A model the agent is known not to offer, and cannot be forced to run, fails before an
        // agent is started for it. Known from a recent reading only: a model the agent has added
        // since is the started session's to accept.
        if let Some(model) = &delegation.model {
            let remembered =
                super::catalog::remembered(&self.seed.config.nori_home, &agent_info.agent_name);
            let recent = super::catalog::now_seconds() - remembered.refreshed_at
                < super::CATALOG_FRESH_FOR.as_secs() as i64;
            let offered = remembered.models;
            if recent
                && !injects_model
                && !offered.is_empty()
                && super::models::find(&offered, model).is_none()
            {
                return Err(format!(
                    "{} has no model `{model}`; it offers: {}",
                    agent_info.display_name,
                    super::models::describe(&offered)
                ));
            }
        }
        let cwd = match delegation.isolation {
            Isolation::Shared => self.seed.config.cwd.clone(),
            Isolation::Worktree => worktree::create(&self.seed.config.cwd).await?,
        };
        let mut config = (*self.seed.config).clone();
        config.active_agent = agent_info.agent_name.clone();
        config.cwd = cwd.clone();
        config.approval_policy = self.approval_policy();
        // A model is forced at spawn too, for agents that only take one that way; by its value,
        // when the agent has said which models it offers.
        if let Some(model) = &delegation.model {
            let remembered =
                super::catalog::remembered(&self.seed.config.nori_home, &agent_info.agent_name)
                    .models;
            let value = super::models::find(&remembered, model)
                .map_or_else(|| model.trim().to_owned(), |offered| offered.value.clone());
            config
                .default_models
                .insert(agent_info.agent_name.clone(), value);
        }
        let spec = crate::runtime::AgentPrepareSpec {
            handoff: None,
            config: Arc::new(config),
            cli_version: self.seed.cli_version.clone(),
            session_context: None,
            initial_context: delegation.fork_context.clone(),
        };
        let (id, key, view) = {
            let mut state = self.state.lock().await;
            if state.shut_down {
                return Err("this session is ending".to_owned());
            }
            let repeated = delegation
                .client_request_id
                .as_ref()
                .and_then(|request| state.by_client_request.get(request).cloned());
            if let Some(existing) = repeated {
                drop(state);
                return self.status(&existing, false).await;
            }
            self.count_running_task();
            state.next_task += 1;
            state.next_child += 1;
            let id = TaskId::numbered(state.next_task);
            let key = ChildKey(state.next_child);
            state.children.insert(key, ChildSlot::Starting);
            let view = view_id();
            state.tasks.push(TaskRecord {
                id: id.clone(),
                adoption_anchor: false,
                child: key,
                view_id: view.clone(),
                parent_view: delegation.parent_view.clone(),
                agent: agent_info.agent_name.clone(),
                agent_name: agent_info.display_name.clone(),
                model: delegation.model.clone(),
                title: brief::task_title(delegation.title.as_deref(), &delegation.task),
                status: TaskStatus::Starting,
                result: String::new(),
                delivery: Delivery::Owed,
                wake: if delegation.fork_context.is_some() {
                    Wake::Never
                } else {
                    wake_for(delegation.mode)
                },
                awaiting_approval: false,
                relayed_approvals: Vec::new(),
                started_at: Instant::now(),
                ended_at: None,
                worktree: (delegation.isolation == Isolation::Worktree).then(|| cwd.clone()),
                progress: Default::default(),
            });
            if let Some(request) = &delegation.client_request_id {
                state.by_client_request.insert(request.clone(), id.clone());
            }
            (id, key, view)
        };
        let is_fork = delegation.fork_context.is_some();
        if !is_fork {
            self.announce_task(&id).await;
        }
        let prompt = if delegation.fork_context.is_some() {
            delegation.task.clone()
        } else {
            brief::child_prompt(delegation.role, &delegation.task)
        };
        let orchestrator = Arc::clone(self);
        let task = id.clone();
        let model = delegation.model.clone();
        let options = delegation.options.clone();
        let source = delegation.fork_source;
        let fork_context = delegation.fork_context;
        let fork_entries = delegation.fork_entries;
        let parent_view = delegation.parent_view;
        let child_agent = agent_info.agent_name.clone();
        tokio::spawn(async move {
            match child::launch(
                &orchestrator,
                spec,
                model.as_deref(),
                &options,
                is_fork,
                view.clone(),
            )
            .await
            {
                Ok(launched) => {
                    if let Some((session_id, start, end)) = source
                        && let Err(error) = launched
                            .child
                            .harness
                            .record_fork_source(
                                session_id,
                                start,
                                end,
                                fork_context.unwrap_or_default(),
                                fork_entries,
                            )
                            .await
                    {
                        let _ = launched.child.harness.shutdown().await;
                        orchestrator.state.lock().await.children.remove(&key);
                        orchestrator
                            .task_ended(
                                &task,
                                TaskEnd::Failed {
                                    error: error.to_string(),
                                },
                                String::new(),
                            )
                            .await;
                        return;
                    }
                    if is_fork {
                        let result = orchestrator
                            .persist_managed_fork(&launched.child, &view, parent_view, child_agent)
                            .await
                            .and_then(|()| launched.child.attach(view.clone()));
                        if let Err(error) = result {
                            let error = match orchestrator.remove_managed_fork_binding(&view).await
                            {
                                Ok(()) => error,
                                Err(rollback) => format!(
                                    "{error}; failed to remove managed child binding: {rollback}"
                                ),
                            };
                            let _ = launched.child.harness.shutdown().await;
                            orchestrator.state.lock().await.children.remove(&key);
                            orchestrator
                                .task_ended(&task, TaskEnd::Failed { error }, String::new())
                                .await;
                            return;
                        }
                        orchestrator.announce_task(&task).await;
                        if let Err(error) = launched.child.activate() {
                            let error = match orchestrator.remove_managed_fork_binding(&view).await
                            {
                                Ok(()) => error,
                                Err(rollback) => format!(
                                    "{error}; failed to remove managed child binding: {rollback}"
                                ),
                            };
                            let _ = launched.child.harness.shutdown().await;
                            orchestrator.state.lock().await.children.remove(&key);
                            orchestrator
                                .task_ended(&task, TaskEnd::Failed { error }, String::new())
                                .await;
                            return;
                        }
                    }
                    if let Ok(Some(recorder)) = launched.child.harness.conversation_recorder().await
                        && let Ok(events) = recorder.replay_session_events().await
                    {
                        for event in events {
                            if let nori_protocol::SessionEvent::Acp(
                                nori_protocol::AcpEvent::Notification(
                                    nori_protocol::acp::v1::AgentNotification::SessionNotification(
                                        mut notification,
                                    ),
                                ),
                            ) = event
                            {
                                notification.session_id = view.clone();
                                orchestrator
                                    .emit(nori_protocol::SessionEvent::Acp(
                                        nori_protocol::AcpEvent::ChildNotification(notification),
                                    ))
                                    .await;
                            }
                        }
                    }
                    orchestrator.child_ready(key, &task, launched, prompt).await
                }
                Err(error) => {
                    orchestrator.state.lock().await.children.remove(&key);
                    orchestrator
                        .task_ended(&task, TaskEnd::Failed { error }, String::new())
                        .await;
                }
            }
        });
        self.after_start(&id, delegation.mode, delegation.timeout)
            .await
    }

    async fn child_ready(
        &self,
        key: ChildKey,
        id: &TaskId,
        launched: child::Launched,
        prompt: String,
    ) {
        let (view_id, progress) = {
            let mut state = self.state.lock().await;
            if state.shut_down {
                drop(state);
                let _ = launched.child.harness.shutdown().await;
                return;
            }
            state
                .children
                .insert(key, ChildSlot::Ready(launched.child.clone()));
            let Some(task) = state.tasks.iter_mut().find(|task| &task.id == id) else {
                return;
            };
            if launched.model.is_some() {
                task.model = launched.model.clone();
            }
            if task.status.is_ended() {
                // Cancelled while the agent was starting; the child stays for follow-ups.
                return;
            }
            (task.view_id.clone(), std::sync::Arc::clone(&task.progress))
        };
        self.touch();
        self.announce_task(id).await;
        if let Err(error) = launched.child.run(child::Turn {
            task: id.clone(),
            view_id,
            prompt,
            content: None,
            progress,
        }) {
            self.task_ended(id, TaskEnd::Failed { error }, String::new())
                .await;
        }
    }

    /// Start a follow-up task on the child that ran `previous`: the child keeps its conversation.
    pub(super) async fn send(
        self: &Arc<Self>,
        previous: &TaskId,
        message: String,
        mode: Mode,
        timeout: Option<Duration>,
        origin: FollowUp,
        content: Option<Vec<acp::ContentBlock>>,
    ) -> Result<TaskSnapshot, String> {
        let generation = self.generation.load(std::sync::atomic::Ordering::Acquire);
        let _admission = self.admission.lock().await;
        if generation != self.generation.load(std::sync::atomic::Ordering::Acquire) {
            return Err("The conversation was rewound before the child message started".to_owned());
        }
        self.send_locked(previous, message, mode, timeout, origin, content)
            .await
    }

    pub(super) async fn send_locked(
        self: &Arc<Self>,
        previous: &TaskId,
        message: String,
        mode: Mode,
        timeout: Option<Duration>,
        origin: FollowUp,
        content: Option<Vec<acp::ContentBlock>>,
    ) -> Result<TaskSnapshot, String> {
        let user_message = origin != FollowUp::Tool;
        if message.trim().is_empty() {
            return Err("`message` is empty".to_owned());
        }
        let (id, child, view_id, progress) = {
            let mut state = self.state.lock().await;
            let earlier = state
                .tasks
                .iter()
                .find(|task| &task.id == previous && (user_message || !task.adoption_anchor))
                .ok_or_else(|| format!("there is no task `{previous}`"))?;
            let key = earlier.child;
            if !user_message
                && state
                    .tasks
                    .iter()
                    .any(|task| task.child == key && !task.status.is_ended())
            {
                return Err(format!(
                    "the child of `{previous}` is still working; wait for its task to finish or \
                     cancel it first"
                ));
            }
            let Some(ChildSlot::Ready(child)) = state.children.get(&key) else {
                return Err(format!("the child of `{previous}` is no longer running"));
            };
            let child = child.clone();
            let template = earlier.follow_up(if user_message {
                earlier.view_id.clone()
            } else {
                view_id()
            });
            self.count_running_task();
            state.next_task += 1;
            let id = TaskId::numbered(state.next_task);
            let view_id = template.view_id.clone();
            let progress = std::sync::Arc::clone(&template.progress);
            state.tasks.push(TaskRecord {
                id: id.clone(),
                title: brief::task_title(None, &message),
                wake: if user_message {
                    Wake::Never
                } else {
                    wake_for(mode)
                },
                ..template
            });
            (id, child, view_id, progress)
        };
        self.announce_task(&id).await;
        let turn = child::Turn {
            task: id.clone(),
            view_id,
            prompt: message,
            content,
            progress,
        };
        let started = if origin == FollowUp::Steer {
            child.steer(turn).await
        } else {
            child.run(turn)
        };
        if let Err(error) = started {
            self.task_ended(&id, TaskEnd::Failed { error }, String::new())
                .await;
        }
        self.after_start(&id, mode, timeout).await
    }

    async fn after_start(
        &self,
        id: &TaskId,
        mode: Mode,
        timeout: Option<Duration>,
    ) -> Result<TaskSnapshot, String> {
        match mode {
            Mode::Background => {
                let mut snapshot = self.status(id, false).await?;
                if !snapshot_ended(&snapshot) {
                    snapshot.next = Some(wake::DELIVERED_LATER);
                }
                Ok(snapshot)
            }
            Mode::Wait => self
                .wait(std::slice::from_ref(id), timeout.unwrap_or(DEFAULT_WAIT))
                .await?
                .into_iter()
                .next()
                .ok_or_else(|| format!("there is no task `{id}`")),
        }
    }

    /// The task an earlier delegation with the same idempotency key started.
    async fn requested_before(&self, request: Option<&str>) -> Option<TaskId> {
        let request = request?;
        self.state
            .lock()
            .await
            .by_client_request
            .get(request)
            .cloned()
    }

    /// A task's snapshot. With `deliver`, reading a finished task delivers its result.
    pub(super) async fn status(&self, id: &TaskId, deliver: bool) -> Result<TaskSnapshot, String> {
        let mut state = self.state.lock().await;
        let task = state
            .tasks
            .iter_mut()
            .find(|task| &task.id == id && !task.adoption_anchor)
            .ok_or_else(|| format!("there is no task `{id}`"))?;
        if deliver && task.status.is_ended() && task.delivery == Delivery::Owed {
            task.delivery = Delivery::Delivered;
        }
        Ok(task.snapshot(Instant::now()))
    }

    /// Every task, without results.
    pub(super) async fn list(&self) -> Vec<TaskSnapshot> {
        let state = self.state.lock().await;
        let now = Instant::now();
        state
            .tasks
            .iter()
            .filter(|task| !task.adoption_anchor)
            .map(|task| TaskSnapshot {
                result: None,
                ..task.snapshot(now)
            })
            .collect()
    }

    pub(super) async fn announce_task(&self, id: &TaskId) {
        let announcement = {
            let state = self.state.lock().await;
            state.tasks.iter().find(|task| &task.id == id).map(|task| {
                (
                    task.view_id.clone(),
                    task.parent_view.clone(),
                    task.agent.clone(),
                    task.view_title(),
                    task.title.clone(),
                )
            })
        };
        if let Some((view_id, parent_view, agent, title, label)) = announcement {
            self.announce(
                &view_id,
                parent_view,
                agent,
                title,
                label,
                SubagentState::Running,
            )
            .await;
        }
    }
}

fn snapshot_ended(snapshot: &TaskSnapshot) -> bool {
    matches!(
        snapshot.status,
        "completed" | "incomplete" | "failed" | "cancelled"
    )
}

fn wake_for(mode: Mode) -> Wake {
    match mode {
        Mode::Background => Wake::Always,
        Mode::Wait => Wake::IfUnwaited,
    }
}

/// A session id for the user's view of one task, unique across every tree of delegations.
fn view_id() -> acp::SessionId {
    acp::SessionId::new(format!("nori-task-{}", uuid::Uuid::new_v4()))
}
