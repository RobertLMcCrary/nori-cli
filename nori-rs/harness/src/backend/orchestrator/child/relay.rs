//! Passing what a child does on to its parent's stream, and keeping the task's progress.

use std::sync::Arc;

use nori_protocol::AcpEvent;
use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;
use nori_protocol::SubagentWire;
use nori_protocol::acp::v1 as acp;

use super::Active;
use super::Inner;
use super::TaskEnd;
use crate::runtime::HarnessHandle;

/// Pass one child event on to the parent. Returns how the active turn ended, when this event
/// ended it.
pub(super) async fn relay(
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
    progress: &mut super::super::task::Progress,
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
