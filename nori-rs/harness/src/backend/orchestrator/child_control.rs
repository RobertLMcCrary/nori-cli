//! Child control handles are separate from the parent's control plane.

use super::ChildSlot;
use super::Orchestrator;
use crate::runtime::HarnessHandle;
use nori_protocol::acp;

impl Orchestrator {
    async fn child_actor(
        &self,
        session_id: &acp::v1::SessionId,
    ) -> Result<super::child::Child, String> {
        let mut changed = self.inner.changed.subscribe();
        loop {
            let state = self.inner.state.lock().await;
            if state.pending_adoptions.contains(session_id) {
                return Err("the child session is being adopted".to_owned());
            }
            let task = state
                .tasks
                .iter()
                .rev()
                .find(|task| &task.view_id == session_id)
                .ok_or_else(|| format!("No controlled child session {session_id}"))?;
            match state.children.get(&task.child) {
                Some(ChildSlot::Ready(child)) => return Ok(child.clone()),
                Some(ChildSlot::Starting) => {}
                None => return Err("The child session is no longer available".to_owned()),
            }
            drop(state);
            changed
                .changed()
                .await
                .map_err(|_| "The child session is no longer available".to_owned())?;
        }
    }

    pub(crate) async fn promote_user_queue(
        &self,
        session_id: &acp::v1::SessionId,
    ) -> Result<(), String> {
        self.child_actor(session_id).await?.promote().await
    }

    pub(crate) async fn cancel_user_turn(
        &self,
        session_id: &acp::v1::SessionId,
    ) -> Result<(), String> {
        self.child_actor(session_id).await?.cancel().await
    }

    pub(crate) async fn controlled_subagent_handle(
        &self,
        session_id: &acp::v1::SessionId,
    ) -> Option<HarnessHandle> {
        let mut changed = self.inner.changed.subscribe();
        loop {
            let state = self.inner.state.lock().await;
            if state.pending_adoptions.contains(session_id) {
                return None;
            }
            let Some(task) = state
                .tasks
                .iter()
                .rev()
                .find(|task| &task.view_id == session_id)
            else {
                let next = state.descendants.get(session_id).cloned()?;
                drop(state);
                return next
                    .handle
                    .controlled_subagent_handle(next.session_id)
                    .await
                    .ok()
                    .flatten();
            };
            match state.children.get(&task.child) {
                Some(ChildSlot::Ready(child)) => return Some(child.harness.clone()),
                Some(ChildSlot::Starting) => {}
                None => return None,
            }
            drop(state);
            changed.changed().await.ok()?;
        }
    }

    pub(crate) async fn descendant_next_hop(
        &self,
        session_id: &acp::v1::SessionId,
    ) -> Option<super::DescendantRoute> {
        self.inner
            .state
            .lock()
            .await
            .descendants
            .get(session_id)
            .cloned()
    }
}
