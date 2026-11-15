//! A child session: a full harness session on the agent a task names, driven one task at a time.
//!
//! Each child has an actor that owns its event stream. The actor runs a task's turn, relays what
//! the child does into the parent's stream under the task's view id, asks the user for the child's
//! permissions through the parent, and reports the turn's end with the child's final message.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Weak;
use std::time::Duration;

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

mod relay;

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
            match super::models::select_model(
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
    if let Err(error) = super::models::apply_options(
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
                let Some(end) = relay::relay(&orchestrator, &harness, &child_session, view.as_ref(), active.as_mut(), event).await else {
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
