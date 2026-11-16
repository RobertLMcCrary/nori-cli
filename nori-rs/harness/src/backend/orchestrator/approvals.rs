//! Permission requests asked of the user in the parent conversation: a child's, and Nori's own
//! when a tool is about to act outside the machine for the agent.

use std::sync::Arc;

use nori_config::AskForApproval;
use nori_protocol::acp::v1 as acp;

use super::Inner;
use super::Orchestrator;
use super::Signal;
use super::task::TaskId;
use crate::connection::DelegatedRequest;

impl Orchestrator {
    /// Whether the session's approval policy has Nori ask before acting for the agent.
    pub(crate) fn asks_for_approval(&self) -> bool {
        self.inner.approval_policy() != AskForApproval::Never
    }

    /// Ask the user, in this conversation, to allow what `title` says Nori will do for the agent,
    /// such as opening a pull request. `false` when the user declines, or there is no
    /// conversation to ask in.
    pub(crate) async fn ask_user(&self, title: String, detail: String) -> bool {
        let Some(session_id) = self.inner.parent_session() else {
            return false;
        };
        let id = format!("nori-{}", uuid::Uuid::new_v4());
        let tool_call = acp::ToolCallUpdate::new(
            acp::ToolCallId::new(id.clone()),
            acp::ToolCallUpdateFields::new()
                .title(title)
                .kind(acp::ToolKind::Other)
                .status(acp::ToolCallStatus::Pending)
                .content(vec![acp::ToolCallContent::from(detail)]),
        );
        let options = vec![
            acp::PermissionOption::new(
                acp::PermissionOptionId::new("allow"),
                "Allow",
                acp::PermissionOptionKind::AllowOnce,
            ),
            acp::PermissionOption::new(
                acp::PermissionOptionId::new("reject"),
                "Don't allow",
                acp::PermissionOptionKind::RejectOnce,
            ),
        ];
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        self.inner
            .signal(Signal::Approval(Box::new(DelegatedRequest {
                request_id: acp::RequestId::Str(id),
                request: acp::AgentRequest::RequestPermissionRequest(
                    acp::RequestPermissionRequest::new(session_id, tool_call, options),
                ),
                response_tx,
            })));
        match response_rx.await {
            Ok(Ok(acp::ClientResponse::RequestPermissionResponse(response))) => matches!(
                response.outcome,
                acp::RequestPermissionOutcome::Selected(selected)
                    if selected.option_id.to_string() == "allow"
            ),
            _ => false,
        }
    }
}

impl Inner {
    /// Relay a child's request to the user through the parent, and the answer back to the child.
    pub(super) async fn relay_request(
        self: &Arc<Self>,
        harness: &crate::runtime::HarnessHandle,
        task: Option<TaskId>,
        view_id: Option<acp::SessionId>,
        request_id: acp::RequestId,
        request: acp::AgentRequest,
    ) {
        let (Some(view_id), acp::AgentRequest::RequestPermissionRequest(mut permission)) =
            (view_id, request)
        else {
            // Only permission requests reach the client; anything else is the child's to answer.
            let _ = harness
                .respond_to_agent(request_id, Err(acp::Error::method_not_found()))
                .await;
            return;
        };
        permission.session_id = view_id.clone();
        let parent_request_id = acp::RequestId::Str(format!("{view_id}/{request_id}"));
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        self.set_awaiting_approval(task.as_ref(), true).await;
        if let Some(id) = &task {
            let mut state = self.state.lock().await;
            if let Some(record) = state.tasks.iter_mut().find(|record| &record.id == id) {
                record.relayed_approvals.push(parent_request_id.clone());
            }
        }
        self.signal(Signal::Approval(Box::new(DelegatedRequest {
            request_id: parent_request_id,
            request: acp::AgentRequest::RequestPermissionRequest(permission),
            response_tx,
        })));
        let orchestrator = Arc::clone(self);
        let harness = harness.clone();
        tokio::spawn(async move {
            let response = response_rx.await.unwrap_or_else(|_| {
                Ok(acp::ClientResponse::RequestPermissionResponse(
                    acp::RequestPermissionResponse::new(acp::RequestPermissionOutcome::Cancelled),
                ))
            });
            let _ = harness.respond_to_agent(request_id, response).await;
            orchestrator
                .set_awaiting_approval(task.as_ref(), false)
                .await;
        });
    }

    async fn set_awaiting_approval(&self, id: Option<&TaskId>, awaiting: bool) {
        let Some(id) = id else { return };
        let mut state = self.state.lock().await;
        if let Some(task) = state.tasks.iter_mut().find(|task| &task.id == id) {
            task.awaiting_approval = awaiting && !task.status.is_ended();
        }
        drop(state);
        self.touch();
    }
}
