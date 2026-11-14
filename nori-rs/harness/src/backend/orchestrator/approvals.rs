//! A child's permission requests, asked of the user in the parent conversation.

use std::sync::Arc;

use nori_protocol::acp::v1 as acp;

use super::Inner;
use super::Signal;
use super::task::TaskId;
use crate::connection::DelegatedRequest;

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
