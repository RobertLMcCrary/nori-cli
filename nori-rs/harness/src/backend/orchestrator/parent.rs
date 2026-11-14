//! The parent session's side: what its backend does for the orchestrator.

use nori_protocol::AcpEvent;
use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;
use tokio::sync::mpsc;

use super::Signal;
use crate::backend::AcpBackend;
use crate::backend::BackendEvent;
use crate::backend::PendingApprovalRequest;
use crate::backend::session_reducer;
use crate::backend::session_runtime_driver::SessionRuntimeInput;
use crate::connection::DelegatedRequest;
use crate::normalized::session_runtime::QueuedPrompt;
use crate::normalized::session_runtime::QueuedPromptKind;

/// Results owed within this long of each other reach the agent in one turn.
const WAKE_BATCH: std::time::Duration = std::time::Duration::from_millis(600);

impl AcpBackend {
    /// Act on the orchestrator's signals until it stops sending them, at shutdown.
    pub(in crate::backend) fn serve_orchestrator(
        &self,
        mut signals: mpsc::UnboundedReceiver<Signal>,
    ) {
        let backend = self.clone();
        tokio::spawn(async move {
            let wake_scheduled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            while let Some(signal) = signals.recv().await {
                match signal {
                    Signal::Approval(request) => backend.ask_for_child(request).await,
                    Signal::ResultsOwed => {
                        if !wake_scheduled.swap(true, std::sync::atomic::Ordering::AcqRel) {
                            let backend = backend.clone();
                            let wake_scheduled = std::sync::Arc::clone(&wake_scheduled);
                            tokio::spawn(async move {
                                tokio::time::sleep(WAKE_BATCH).await;
                                wake_scheduled.store(false, std::sync::atomic::Ordering::Release);
                                backend.deliver_task_results_if_idle().await;
                            });
                        }
                    }
                    Signal::Withdraw(request_ids) => {
                        // Dropping a request's responder answers the child's relay as cancelled.
                        backend
                            .pending_approvals
                            .lock()
                            .await
                            .retain(|pending| !request_ids.contains(&pending.request.request_id));
                        for request_id in request_ids {
                            let _ = backend
                                .backend_event_tx
                                .send(BackendEvent::Public(SessionEvent::Nori(
                                    NoriEvent::RequestWithdrawn(request_id),
                                )))
                                .await;
                        }
                    }
                }
            }
        });
    }

    /// Ask the user, in this conversation, for a permission a child requested.
    async fn ask_for_child(&self, request: Box<DelegatedRequest>) {
        let request = *request;
        let title = match &request.request {
            nori_protocol::acp::v1::AgentRequest::RequestPermissionRequest(permission) => {
                permission
                    .tool_call
                    .fields
                    .title
                    .clone()
                    .unwrap_or_else(|| "tool request".to_owned())
            }
            _ => "tool request".to_owned(),
        };
        let event = SessionEvent::Acp(AcpEvent::Request {
            request_id: request.request_id.clone(),
            request: request.request.clone(),
        });
        let call_id = request.request_id.to_string();
        self.pending_approvals
            .lock()
            .await
            .push(PendingApprovalRequest { request });
        let _ = self
            .backend_event_tx
            .send(BackendEvent::Public(event))
            .await;
        self.user_notifier
            .notify(&crate::UserNotification::AwaitingApproval {
                call_id,
                command: title,
                cwd: self.cwd.display().to_string(),
            });
    }

    /// Hand the parent agent the results it is owed, in a hidden turn, if it is idle. A busy
    /// parent gets them when its turn ends.
    pub(in crate::backend) async fn deliver_task_results_if_idle(&self) {
        let busy = self.session_driver.lock().await.prompt_is_busy();
        if busy
            || self
                .is_shutting_down
                .load(std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        let Some(wake) = self.orchestrator.take_wake().await else {
            return;
        };
        let _ = self
            .backend_event_tx
            .send(BackendEvent::Public(SessionEvent::Nori(
                NoriEvent::TaskResultsDelivered(wake.delivered),
            )))
            .await;
        let _ = self
            .session_event_tx
            .send(SessionRuntimeInput::Reducer(
                session_reducer::InboundEvent::PromptSubmit(QueuedPrompt {
                    event_id: format!("task-results-{}", uuid::Uuid::new_v4()),
                    kind: QueuedPromptKind::TaskResults,
                    text: wake.text.clone(),
                    prompt_meta: None,
                    original_content: Vec::new(),
                    content: vec![nori_protocol::acp::v1::ContentBlock::Text(
                        nori_protocol::acp::v1::TextContent::new(wake.text),
                    )],
                    display_text: None,
                }),
            ))
            .await;
    }
}
