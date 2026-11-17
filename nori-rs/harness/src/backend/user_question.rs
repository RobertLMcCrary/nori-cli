//! Close only the outstanding question identified by a provider cancellation.
use super::AcpBackend;
use super::BackendEvent;
use nori_protocol::AcpEvent;
use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;
use nori_protocol::acp::v1 as acp;

pub(super) fn is_question_cancellation(event: &AcpEvent) -> bool {
    matches!(event, AcpEvent::Notification(acp::AgentNotification::ExtNotification(notification)) if notification.method.as_ref() == "_nori/user_input_cancelled")
}

pub(super) fn question_key(params: &serde_json::Value) -> Option<(String, String, String)> {
    Some((
        params.get("sessionId")?.as_str()?.to_owned(),
        params.get("callId")?.as_str()?.to_owned(),
        params.get("turnId")?.as_str()?.to_owned(),
    ))
}

impl AcpBackend {
    pub(super) async fn withdraw_cancelled_question(&self, event: &AcpEvent) -> bool {
        let AcpEvent::Notification(acp::AgentNotification::ExtNotification(notification)) = event
        else {
            return false;
        };
        if notification.method.as_ref() != "_nori/user_input_cancelled" {
            return false;
        }
        let Ok(cancelled) = serde_json::from_str::<serde_json::Value>(notification.params.get())
        else {
            return true;
        };
        let Some(key) = question_key(&cancelled) else {
            return true;
        };
        let mut pending = self.pending_approvals.lock().await;
        let mut ids = Vec::new();
        pending.retain(|item| {
            let acp::AgentRequest::ExtMethodRequest(request) = &item.request.request else {
                return true;
            };
            if request.method.as_ref() != "_nori/request_user_input" {
                return true;
            }
            let Ok(question) = serde_json::from_str::<serde_json::Value>(request.params.get())
            else {
                return true;
            };
            let matches = question_key(&question).as_ref() == Some(&key);
            if matches {
                ids.push(item.request.request_id.clone());
            }
            !matches
        });
        drop(pending);
        if ids.is_empty() {
            self.withdrawn_questions.lock().await.insert(key);
        }
        for id in ids {
            let _ = self
                .backend_event_tx
                .send(BackendEvent::Public(SessionEvent::Nori(
                    NoriEvent::RequestWithdrawn(id),
                )))
                .await;
        }
        true
    }
    pub(super) async fn withdraw_all_questions(&self) {
        self.withdrawn_questions.lock().await.clear();
        let mut pending = self.pending_approvals.lock().await;
        let mut ids = Vec::new();
        pending.retain(|item| {
            if matches!(&item.request.request, acp::AgentRequest::ExtMethodRequest(request) if request.method.as_ref() == "_nori/request_user_input") {
                ids.push(item.request.request_id.clone());
                false
            } else { true }
        });
        drop(pending);
        for id in ids {
            let _ = self
                .backend_event_tx
                .send(BackendEvent::Public(SessionEvent::Nori(
                    NoriEvent::RequestWithdrawn(id),
                )))
                .await;
        }
    }
}
