//! Starting tasks: on a new child, or as a follow-up on the child of an earlier task.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use nori_protocol::SubagentState;
use nori_protocol::acp::v1 as acp;
use serde::Serialize;

use super::ChildKey;
use super::ChildSlot;
use super::DEFAULT_WAIT;
use super::Inner;
use super::MAX_DEPTH;
use super::MAX_RUNNING_TASKS;
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
}

/// The agents a session can delegate to, for `agent_capabilities`.
#[derive(Debug, Serialize)]
pub(super) struct Capabilities {
    pub(super) can_delegate: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) reason: Option<String>,
    pub(super) depth: i64,
    pub(super) max_depth: i64,
    pub(super) running_tasks: i64,
    pub(super) max_running_tasks: i64,
    pub(super) agents: Vec<AgentEntry>,
}

#[derive(Debug, Serialize)]
pub(super) struct AgentEntry {
    /// The value to pass as `agent`.
    pub(super) agent: String,
    pub(super) name: String,
    /// The agent this session runs on.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(super) is_yours: bool,
    /// The model a child gets when `model` is not given, when Nori is configured with one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) default_model: Option<String>,
    /// Models the agent offered when Nori last ran it; empty when it has not run yet. Any model
    /// it offers can be passed; an unknown one fails with the list.
    pub(super) models: Vec<ModelEntry>,
    /// How long ago, in seconds, Nori last read what the agent offers; absent when it never has.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) read_seconds_ago: Option<i64>,
    /// The agent's other settings that `options` can set, with their values.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) settings: Vec<SettingEntry>,
}

#[derive(Debug, Serialize)]
pub(super) struct ModelEntry {
    pub(super) model: String,
    pub(super) name: String,
}

#[derive(Debug, Serialize)]
pub(super) struct SettingEntry {
    pub(super) setting: String,
    pub(super) name: String,
    pub(super) values: Vec<String>,
}

impl Inner {
    fn delegation_refusal(&self) -> Option<String> {
        if self.seed.depth >= MAX_DEPTH {
            return Some(format!(
                "this session is a delegated task {} levels deep, the most there can be; do the \
                 work yourself",
                self.seed.depth
            ));
        }
        let running = self.seed.tree.running.load(Ordering::Acquire);
        (running >= MAX_RUNNING_TASKS).then(|| busy(running))
    }

    /// Take one of the tree's running-task slots; [`Inner::task_ended`] gives it back.
    fn reserve(&self) -> Result<(), String> {
        let running = self.seed.tree.running.fetch_add(1, Ordering::AcqRel);
        if running >= MAX_RUNNING_TASKS {
            self.seed.tree.running.fetch_sub(1, Ordering::AcqRel);
            return Err(busy(running));
        }
        Ok(())
    }

    pub(super) fn capabilities(&self) -> Capabilities {
        let refusal = self.delegation_refusal();
        let config = &self.seed.config;
        let agents = crate::registry::list_available_agents()
            .into_iter()
            .filter(|agent| agent.is_installed)
            .map(|agent| AgentEntry {
                models: offered_models(&config.nori_home, &agent.agent_name),
                read_seconds_ago: Some(
                    super::catalog::remembered(&config.nori_home, &agent.agent_name).refreshed_at,
                )
                .filter(|refreshed_at| *refreshed_at > 0)
                .map(|refreshed_at| super::catalog::now_seconds() - refreshed_at),
                settings: super::catalog::remembered(&config.nori_home, &agent.agent_name)
                    .settings
                    .into_iter()
                    .map(|setting| SettingEntry {
                        setting: setting.id,
                        name: setting.name,
                        values: setting
                            .values
                            .into_iter()
                            .map(|value| value.value)
                            .collect(),
                    })
                    .collect(),
                default_model: config.default_models.get(&agent.agent_name).cloned(),
                is_yours: agent.agent_name == config.active_agent,
                name: agent.display_name,
                agent: agent.agent_name,
            })
            .collect();
        Capabilities {
            can_delegate: refusal.is_none(),
            reason: refusal,
            depth: self.seed.depth,
            max_depth: MAX_DEPTH,
            running_tasks: self.seed.tree.running.load(Ordering::Acquire),
            max_running_tasks: MAX_RUNNING_TASKS,
            agents,
        }
    }

    /// Start a task on a new child.
    pub(super) async fn delegate(
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
        if let Some(refusal) = self.delegation_refusal() {
            return Err(refusal);
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
        // agent is started for it.
        if let Some(model) = &delegation.model {
            let offered =
                super::catalog::remembered(&self.seed.config.nori_home, &agent_info.agent_name)
                    .models;
            if !injects_model
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
            config: Arc::new(config),
            cli_version: self.seed.cli_version.clone(),
            session_context: None,
            initial_context: None,
        };
        let (id, key) = {
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
            self.reserve()?;
            state.next_task += 1;
            state.next_child += 1;
            let id = TaskId::numbered(state.next_task);
            let key = ChildKey(state.next_child);
            state.children.insert(key, ChildSlot::Starting);
            state.tasks.push(TaskRecord {
                id: id.clone(),
                child: key,
                view_id: view_id(),
                agent: agent_info.agent_name.clone(),
                agent_name: agent_info.display_name.clone(),
                model: delegation.model.clone(),
                title: brief::task_title(delegation.title.as_deref(), &delegation.task),
                status: TaskStatus::Starting,
                result: String::new(),
                delivery: Delivery::Owed,
                wake: wake_for(delegation.mode),
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
            (id, key)
        };
        self.announce_task(&id).await;
        let prompt = brief::child_prompt(delegation.role, &delegation.task);
        let orchestrator = Arc::clone(self);
        let task = id.clone();
        let model = delegation.model.clone();
        let options = delegation.options.clone();
        tokio::spawn(async move {
            match child::launch(&orchestrator, spec, model.as_deref(), &options).await {
                Ok(launched) => orchestrator.child_ready(key, &task, launched, prompt).await,
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
        self.announce_task(id).await;
        if let Err(error) = launched.child.run(child::Turn {
            task: id.clone(),
            view_id,
            prompt,
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
    ) -> Result<TaskSnapshot, String> {
        if message.trim().is_empty() {
            return Err("`message` is empty".to_owned());
        }
        if let Some(refusal) = self.delegation_refusal() {
            return Err(refusal);
        }
        let (id, child, view_id, progress) = {
            let mut state = self.state.lock().await;
            let earlier = state
                .tasks
                .iter()
                .find(|task| &task.id == previous)
                .ok_or_else(|| format!("there is no task `{previous}`"))?;
            let key = earlier.child;
            if state
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
            let template = earlier.follow_up(view_id());
            self.reserve()?;
            state.next_task += 1;
            let id = TaskId::numbered(state.next_task);
            let view_id = template.view_id.clone();
            let progress = std::sync::Arc::clone(&template.progress);
            state.tasks.push(TaskRecord {
                id: id.clone(),
                title: brief::task_title(None, &message),
                wake: wake_for(mode),
                ..template
            });
            (id, child, view_id, progress)
        };
        self.announce_task(&id).await;
        if let Err(error) = child.run(child::Turn {
            task: id.clone(),
            view_id,
            prompt: message,
            progress,
        }) {
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
            .find(|task| &task.id == id)
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
            .map(|task| TaskSnapshot {
                result: None,
                ..task.snapshot(now)
            })
            .collect()
    }

    async fn announce_task(&self, id: &TaskId) {
        let announcement = {
            let state = self.state.lock().await;
            state
                .tasks
                .iter()
                .find(|task| &task.id == id)
                .map(|task| (task.view_id.clone(), task.view_title(), task.title.clone()))
        };
        if let Some((view_id, title, label)) = announcement {
            self.announce(&view_id, title, label, SubagentState::Running)
                .await;
        }
    }
}

/// The models `agent` offered when it last ran, then any others Nori knows it can be given.
fn offered_models(home: &std::path::Path, agent: &str) -> Vec<ModelEntry> {
    let mut models: Vec<ModelEntry> = super::catalog::remembered(home, agent)
        .models
        .into_iter()
        .map(|model| ModelEntry {
            model: model.value,
            name: model.name,
        })
        .collect();
    let known = crate::registry::AgentKind::from_slug(agent)
        .map(crate::registry::AgentKind::other_models)
        .unwrap_or_default();
    for other in known {
        if !models.iter().any(|model| model.model == other.id) {
            models.push(ModelEntry {
                model: other.id.to_owned(),
                name: other.label.to_owned(),
            });
        }
    }
    models
}

fn busy(running: i64) -> String {
    format!(
        "{running} delegated tasks are running, the most at once; wait for one to finish or \
         cancel one"
    )
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
