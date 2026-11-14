//! A child session: a full harness session on the agent a task names, driven one task at a time.
//!
//! Each child has an actor that owns its event stream. The actor runs a task's turn, relays what
//! the child does into the parent's stream under the task's view id, asks the user for the child's
//! permissions through the parent, and reports the turn's end with the child's final message.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Weak;
use std::time::Duration;

use nori_protocol::AcpEvent;
use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;
use nori_protocol::SessionPhase;
use nori_protocol::SubagentState;
use nori_protocol::SubagentWire;
use nori_protocol::acp::v1 as acp;
use tokio::sync::mpsc;

use super::Inner;
use super::task::MAX_RESULT_BYTES;
use super::task::TaskEnd;
use super::task::TaskId;
use crate::runtime::HarnessHandle;

/// How long an agent may take to start and open a session before the task fails. Agents that
/// install themselves on first use (npx, uvx) need the margin.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(180);
/// A child whose own delegations all ended is given this long to start the turn that takes their
/// results before its task ends without one.
const CHILDREN_SETTLE: Duration = Duration::from_secs(3);

/// One turn the actor is asked to run.
pub(super) struct Turn {
    pub(super) task: TaskId,
    /// The session id the parent's view shows this task's activity under.
    pub(super) view_id: acp::SessionId,
    pub(super) prompt: String,
    /// Where the actor reports what the child is doing.
    pub(super) progress: super::task::SharedProgress,
}

/// The orchestrator's side of a running child. The actor stops when the last one is dropped.
#[derive(Clone)]
pub(super) struct Child {
    pub(super) harness: HarnessHandle,
    turns: mpsc::UnboundedSender<Turn>,
}

impl Child {
    /// Hand the actor a turn. Fails only when the child is gone.
    pub(super) fn run(&self, turn: Turn) -> Result<(), String> {
        self.turns
            .send(turn)
            .map_err(|_| "the child session has ended".to_owned())
    }
}

/// What starting a child found out about it.
pub(super) struct Launched {
    pub(super) child: Child,
    /// The model the child runs, as the agent names it.
    pub(super) model: Option<String>,
}

/// Start the agent, open a session, select `model`, and start the child's actor.
pub(super) async fn launch(
    inner: &Arc<Inner>,
    spec: crate::runtime::AgentPrepareSpec,
    model: Option<&str>,
    options: &[(String, String)],
) -> Result<Launched, String> {
    let agent = spec.config.active_agent.clone();
    let nori_home = spec.config.nori_home.clone();
    let spec_approval_policy = spec.config.approval_policy;
    let launch = async {
        let agent = crate::runtime::prepare_delegated_agent(spec, &inner.seed)
            .await
            .map_err(|error| format!("could not start the agent: {error:#}"))?;
        let mut session = crate::runtime::launch_session(crate::runtime::SessionLaunchSpec {
            agent,
            start: crate::runtime::SessionStart::New,
        });
        let started = loop {
            match session.events.recv().await {
                Some(SessionEvent::Nori(NoriEvent::SessionStarted(started))) => break started,
                Some(SessionEvent::Nori(NoriEvent::SessionEnded(ended))) => {
                    return Err(ended.message.unwrap_or_else(|| {
                        "the agent ended the session as it started".to_owned()
                    }));
                }
                Some(_) => {}
                None => return Err("the agent exited before its session started".to_owned()),
            }
        };
        Ok((session, started))
    };
    let (session, started) = tokio::time::timeout(LAUNCH_TIMEOUT, launch)
        .await
        .map_err(|_| {
            format!(
                "the agent did not open a session within {}s",
                LAUNCH_TIMEOUT.as_secs()
            )
        })??;
    super::catalog::remember(&nori_home, &agent, &started.config_options);
    let injects_model = crate::registry::get_agent_config(&agent)
        .is_ok_and(|agent| agent.supports_model_injection());
    let model = match model {
        Some(requested) => {
            match select_model(
                &session.handle,
                &started.config_options,
                requested,
                injects_model,
            )
            .await
            {
                Ok(model) => Some(model),
                Err(error) => {
                    let _ = session.handle.shutdown().await;
                    return Err(error);
                }
            }
        }
        None => super::models::current_model(&started.config_options),
    };
    // A permission mode can only be set when the parent runs without approvals, so a child is
    // never given one broader than its parent's.
    let may_set_mode = spec_approval_policy == nori_config::AskForApproval::Never;
    if let Err(error) = apply_options(
        &session.handle,
        &started.config_options,
        options,
        may_set_mode,
    )
    .await
    {
        let _ = session.handle.shutdown().await;
        return Err(error);
    }
    let (turns_tx, turns_rx) = mpsc::unbounded_channel();
    let child = Child {
        harness: session.handle.clone(),
        turns: turns_tx,
    };
    tokio::spawn(run_actor(
        Arc::downgrade(inner),
        session.handle,
        started.acp_session_id,
        session.events,
        turns_rx,
    ));
    Ok(Launched { child, model })
}

/// Choose `requested` from the models the agent advertises, by value or by name. An agent that
/// lists none but was given the model at spawn (`injected`) runs that one.
async fn select_model(
    harness: &HarnessHandle,
    options: &[acp::SessionConfigOption],
    requested: &str,
    injected: bool,
) -> Result<String, String> {
    let Some(models) = super::models::Models::of(options) else {
        if injected {
            return Ok(requested.trim().to_owned());
        }
        return Err(format!(
            "this agent does not let Nori choose its model, so `{requested}` cannot be selected; \
             delegate without `model`"
        ));
    };
    // A model forced at spawn that the agent runs, though it does not list it, is the choice.
    if models.current.eq_ignore_ascii_case(requested.trim()) {
        return Ok(super::models::current_model(options).unwrap_or_else(|| requested.to_owned()));
    }
    let Some(choice) = models.find(requested) else {
        return Err(format!(
            "this agent has no model `{requested}`; it offers: {}",
            models.describe()
        ));
    };
    if choice.value != models.current {
        harness
            .set_session_config_option(models.config_id.clone(), choice.value.clone())
            .await
            .map_err(|error| format!("the agent refused model `{}`: {error:#}", choice.value))?;
    }
    Ok(choice.name.clone())
}

/// Set the other settings a task asks for, each by setting id or name and by value or name.
async fn apply_options(
    harness: &HarnessHandle,
    advertised: &[acp::SessionConfigOption],
    requested: &[(String, String)],
    may_set_mode: bool,
) -> Result<(), String> {
    for (setting, value) in requested {
        let is_mode = advertised.iter().any(|option| {
            option.category == Some(acp::SessionConfigOptionCategory::Mode)
                && super::models::Models::named(std::slice::from_ref(option), setting).is_some()
        });
        if is_mode && !may_set_mode {
            return Err(format!(
                "`{setting}` is the agent's permission mode, and you run with approvals, so a \
                 child cannot be given another"
            ));
        }
        let Some(choices) = super::models::Models::named(advertised, setting) else {
            let known: Vec<String> = super::models::Models::settings(advertised)
                .into_iter()
                .map(|setting| format!("`{}`", setting.config_id))
                .collect();
            return Err(format!(
                "this agent has no setting `{setting}`; its settings are {}",
                if known.is_empty() {
                    "none".to_owned()
                } else {
                    known.join(", ")
                }
            ));
        };
        let Some(choice) = choices.find(value) else {
            return Err(format!(
                "`{setting}` has no value `{value}`; it takes {}",
                choices.describe()
            ));
        };
        if choice.value != choices.current {
            harness
                .set_session_config_option(choices.config_id.clone(), choice.value.clone())
                .await
                .map_err(|error| format!("the agent refused {setting} `{value}`: {error:#}"))?;
        }
    }
    Ok(())
}

/// The text a turn ends with: what the child said after its last tool call, or, when it said
/// nothing after it, the last thing it said before.
#[derive(Default)]
struct FinalText {
    after_last_tool: String,
    before_last_tool: String,
}

impl FinalText {
    fn push(&mut self, text: &str) {
        // Twice the returned size is kept, so cutting it still says how much there was.
        if self.after_last_tool.len() < MAX_RESULT_BYTES * 2 {
            self.after_last_tool.push_str(text);
        }
    }

    fn tool(&mut self) {
        if !self.after_last_tool.trim().is_empty() {
            self.before_last_tool = std::mem::take(&mut self.after_last_tool);
        }
        self.after_last_tool.clear();
    }

    /// The last line written since the last tool call, shortened, when there is one.
    fn last_line(&self) -> Option<String> {
        let text = &self.after_last_tool;
        let mut start = text.len().saturating_sub(400);
        while !text.is_char_boundary(start) {
            start += 1;
        }
        let line = text[start..]
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())?;
        Some(line.trim().chars().take(160).collect())
    }

    fn finish(self) -> String {
        if self.after_last_tool.trim().is_empty() {
            self.before_last_tool
        } else {
            self.after_last_tool
        }
    }
}

struct Active {
    task: TaskId,
    view_id: acp::SessionId,
    request_id: acp::RequestId,
    text: FinalText,
    progress: super::task::SharedProgress,
    /// The turn ended while the child's own delegations ran: the task goes on until they end
    /// and the turn that takes their results ends too.
    awaiting_children: bool,
}

async fn run_actor(
    inner: Weak<Inner>,
    harness: HarnessHandle,
    child_session: acp::SessionId,
    mut events: mpsc::UnboundedReceiver<SessionEvent>,
    mut turns: mpsc::UnboundedReceiver<Turn>,
) {
    let mut active: Option<Active> = None;
    // Activity that arrives between tasks is shown under the task that caused it.
    let mut last_view: Option<acp::SessionId> = None;
    // The child's own delegated tasks still running, by their view ids.
    let mut delegations: HashSet<acp::SessionId> = HashSet::new();
    let mut settle_by: Option<tokio::time::Instant> = None;
    loop {
        tokio::select! {
            turn = turns.recv(), if active.is_none() => {
                let Some(turn) = turn else { break };
                let Some(orchestrator) = inner.upgrade() else { break };
                if orchestrator.task_has_ended(&turn.task).await {
                    // Cancelled while it waited for the child.
                    continue;
                }
                last_view = Some(turn.view_id.clone());
                let content = vec![acp::ContentBlock::Text(acp::TextContent::new(turn.prompt))];
                match harness.prompt(content).await {
                    Ok(request_id) => {
                        orchestrator.task_running(&turn.task).await;
                        active = Some(Active {
                            task: turn.task,
                            view_id: turn.view_id,
                            request_id,
                            text: FinalText::default(),
                            progress: turn.progress,
                            awaiting_children: false,
                        });
                    }
                    Err(error) => {
                        orchestrator
                            .task_ended(&turn.task, TaskEnd::Failed { error: format!("{error:#}") }, String::new())
                            .await;
                    }
                }
            }
            () = sleep_until(settle_by), if settle_by.is_some() => {
                settle_by = None;
                let Some(orchestrator) = inner.upgrade() else { break };
                if let Some(ended) = active.take() {
                    let text = ended.text.finish();
                    orchestrator.task_ended(&ended.task, completed(), text).await;
                }
            }
            event = events.recv() => {
                let Some(orchestrator) = inner.upgrade() else { break };
                let Some(event) = event else {
                    if let Some(active) = active.take() {
                        let text = active.text.finish();
                        orchestrator
                            .task_ended(&active.task, TaskEnd::Failed { error: "the child's agent exited".to_owned() }, text)
                            .await;
                    }
                    break;
                };
                // A turn starting while the task waits on the child's delegations is the one that
                // takes their results.
                if let SessionEvent::Nori(NoriEvent::SessionPhaseChanged(SessionPhase::Prompting { request_id })) = &event
                    && let Some(waiting) = active.as_mut().filter(|active| active.awaiting_children)
                {
                    waiting.request_id = request_id.clone();
                    waiting.awaiting_children = false;
                    settle_by = None;
                }
                if let SessionEvent::Nori(NoriEvent::Subagent(SubagentWire::Update(update))) = &event
                    && update.parent_session_id == child_session
                    && let nori_protocol::Patch::Set(state) = &update.state
                {
                    match state {
                        SubagentState::Running => {
                            delegations.insert(update.session_id.clone());
                        }
                        SubagentState::Ended(_) => {
                            delegations.remove(&update.session_id);
                            if delegations.is_empty()
                                && active.as_ref().is_some_and(|active| active.awaiting_children)
                            {
                                settle_by = Some(tokio::time::Instant::now() + CHILDREN_SETTLE);
                            }
                        }
                    }
                }
                let view = active.as_ref().map(|active| active.view_id.clone()).or_else(|| last_view.clone());
                let Some(end) = relay(&orchestrator, &harness, &child_session, view.as_ref(), active.as_mut(), event).await else {
                    continue;
                };
                if matches!(end, TaskEnd::Completed { .. }) && !delegations.is_empty() {
                    if let Some(waiting) = active.as_mut() {
                        waiting.awaiting_children = true;
                    }
                    continue;
                }
                if let Some(ended) = active.take() {
                    let text = ended.text.finish();
                    orchestrator.task_ended(&ended.task, end, text).await;
                }
            }
        }
    }
    // Turns handed over as the actor stopped would otherwise wait forever.
    turns.close();
    if let Some(orchestrator) = inner.upgrade() {
        while let Ok(turn) = turns.try_recv() {
            orchestrator
                .task_ended(
                    &turn.task,
                    TaskEnd::Failed {
                        error: "the child session ended".to_owned(),
                    },
                    String::new(),
                )
                .await;
        }
    }
    let _ = harness.shutdown().await;
}

async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

fn completed() -> TaskEnd {
    TaskEnd::Completed {
        stop_reason: acp::StopReason::EndTurn,
    }
}

/// Pass one child event on to the parent. Returns how the active turn ended, when this event
/// ended it.
async fn relay(
    orchestrator: &Arc<Inner>,
    harness: &HarnessHandle,
    child_session: &acp::SessionId,
    view: Option<&acp::SessionId>,
    active: Option<&mut Active>,
    event: SessionEvent,
) -> Option<TaskEnd> {
    match event {
        SessionEvent::Acp(AcpEvent::Notification(acp::AgentNotification::SessionNotification(
            mut notification,
        ))) => {
            if let Some(active) = active {
                match &notification.update {
                    acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk {
                        content: acp::ContentBlock::Text(text),
                        ..
                    }) => {
                        active.text.push(&text.text);
                        if let (Some(line), Ok(mut progress)) =
                            (active.text.last_line(), active.progress.lock())
                        {
                            progress.activity = line;
                        }
                    }
                    acp::SessionUpdate::ToolCall(call) => {
                        active.text.tool();
                        if let Ok(mut progress) = active.progress.lock() {
                            progress.tool_calls += 1;
                            progress.activity = call.title.chars().take(160).collect();
                            note_changes(&mut progress, call.kind, &call.content, &call.locations);
                        }
                    }
                    acp::SessionUpdate::ToolCallUpdate(update) => {
                        active.text.tool();
                        if let Ok(mut progress) = active.progress.lock() {
                            note_changes(
                                &mut progress,
                                update.fields.kind.unwrap_or(acp::ToolKind::Other),
                                update.fields.content.as_deref().unwrap_or_default(),
                                update.fields.locations.as_deref().unwrap_or_default(),
                            );
                        }
                    }
                    _ => {}
                }
            }
            let view = view?;
            notification.session_id = view.clone();
            orchestrator
                .emit(SessionEvent::Acp(AcpEvent::ChildNotification(notification)))
                .await;
            None
        }
        // The child's own subagents are the parent's grandchildren; nested deeper they keep their
        // own ids, which are unique already.
        SessionEvent::Acp(AcpEvent::ChildNotification(notification)) => {
            orchestrator
                .emit(SessionEvent::Acp(AcpEvent::ChildNotification(notification)))
                .await;
            None
        }
        SessionEvent::Nori(NoriEvent::Subagent(mut wire)) => {
            if let Some(view) = view {
                retarget(&mut wire, child_session, view);
            }
            orchestrator
                .emit(SessionEvent::Nori(NoriEvent::Subagent(wire)))
                .await;
            None
        }
        SessionEvent::Acp(AcpEvent::Request {
            request_id,
            request,
        }) => {
            let task = active.map(|active| active.task.clone());
            orchestrator
                .relay_request(harness, task, view.cloned(), request_id, request)
                .await;
            None
        }
        SessionEvent::Acp(AcpEvent::Response {
            request_id,
            response,
        }) => {
            let active = active?;
            if request_id != active.request_id {
                return None;
            }
            Some(match response {
                Ok(acp::AgentResponse::PromptResponse(response)) => {
                    match response.stop_reason {
                        // A cancel Nori asked for has ended the task already; this one the child
                        // chose, and the parent must hear of it.
                        acp::StopReason::Cancelled => TaskEnd::Failed {
                            error: "the child stopped before finishing".to_owned(),
                        },
                        stop_reason => TaskEnd::Completed { stop_reason },
                    }
                }
                Ok(_) => TaskEnd::Completed {
                    stop_reason: acp::StopReason::EndTurn,
                },
                Err(error) => TaskEnd::Failed {
                    error: error.message,
                },
            })
        }
        SessionEvent::Nori(NoriEvent::RequestFailed(failure)) => {
            let active = active?;
            (failure.request_id.as_ref() == Some(&active.request_id)).then_some(TaskEnd::Failed {
                error: failure.message,
            })
        }
        SessionEvent::Nori(NoriEvent::SessionEnded(ended)) => {
            active.map(|_| TaskEnd::Failed {
                error: ended
                    .message
                    .unwrap_or_else(|| "the child's session ended".to_owned()),
            })
        }
        SessionEvent::Acp(AcpEvent::Notification(_))
        | SessionEvent::Nori(
            NoriEvent::SessionStarted(_)
            | NoriEvent::SessionPhaseChanged(_)
            | NoriEvent::QueueChanged(_)
            | NoriEvent::ReplayStarted(_)
            | NoriEvent::ReplayFinished
            | NoriEvent::ContextCompacted(_)
            | NoriEvent::SessionForked(_)
            | NoriEvent::GoalChanged(_)
            | NoriEvent::CapabilitiesChanged(_)
            | NoriEvent::Undo(_)
            | NoriEvent::UserShell(_)
            | NoriEvent::HookOutput(_)
            | NoriEvent::PromptSummaryUpdated(_)
            | NoriEvent::Notice(_)
            // Results the child's own tasks handed to it are the child's business.
            | NoriEvent::TaskResultsDelivered(_)
            // The child's requests reach the parent relayed, with ids of the parent's own.
            | NoriEvent::RequestWithdrawn(_),
        ) => None,
    }
}

/// Record the files a tool call changes: those its diffs are of, and the places an edit, delete,
/// or move names.
fn note_changes(
    progress: &mut super::task::Progress,
    kind: acp::ToolKind,
    content: &[acp::ToolCallContent],
    locations: &[acp::ToolCallLocation],
) {
    let diffs = content.iter().filter_map(|content| match content {
        acp::ToolCallContent::Diff(diff) => Some(diff.path.display().to_string()),
        _ => None,
    });
    let edited = matches!(
        kind,
        acp::ToolKind::Edit | acp::ToolKind::Delete | acp::ToolKind::Move
    );
    let places = locations
        .iter()
        .filter(|_| edited)
        .map(|location| location.path.display().to_string());
    for path in diffs.chain(places) {
        if !progress.files_changed.contains(&path) {
            progress.files_changed.push(path);
        }
    }
}

/// Re-parent the child's own subagent traffic onto the task's view of the child.
fn retarget(wire: &mut SubagentWire, child_session: &acp::SessionId, view: &acp::SessionId) {
    let swap = |id: &mut acp::SessionId| {
        if id == child_session {
            *id = view.clone();
        }
    };
    match wire {
        SubagentWire::Update(update) => swap(&mut update.parent_session_id),
        SubagentWire::Message(message) => {
            swap(&mut message.session_id);
            if let Some(sender) = message.sender_session_id.as_mut() {
                swap(sender);
            }
            if let Some(recipient) = message.recipient_session_id.as_mut() {
                swap(recipient);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn the_final_text_is_what_came_after_the_last_tool() {
        let mut text = FinalText::default();
        text.push("Looking at the files.");
        text.tool();
        text.push("They hold ");
        text.push("alpha.");
        assert_eq!(text.finish(), "They hold alpha.");

        let mut silent_after_tool = FinalText::default();
        silent_after_tool.push("Done: alpha.");
        silent_after_tool.tool();
        assert_eq!(silent_after_tool.finish(), "Done: alpha.");
    }
}
