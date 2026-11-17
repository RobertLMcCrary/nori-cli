//! Stage and adopt an independent harness for a native child view.
//!
//! The caller owns native-session quiescence and must keep its generation gate held through
//! validation and commit. Staging never attaches the public view or sends a prompt.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use nori_protocol::acp::v1 as acp;

use super::ChildKey;
use super::ChildSlot;
use super::Orchestrator;
use super::child;
use super::task::Delivery;
use super::task::TaskEnd;
use super::task::TaskId;
use super::task::TaskRecord;
use super::task::TaskStatus;
use super::task::Wake;
use crate::runtime::HarnessHandle;
use crate::transcript::TranscriptLine;

#[derive(Clone)]
pub(crate) struct NativeRebindSpec {
    pub(crate) public_id: acp::SessionId,
    pub(crate) parent_id: acp::SessionId,
    pub(crate) agent: String,
    pub(crate) model: Option<String>,
    pub(crate) options: Vec<(String, String)>,
    pub(crate) context: String,
    pub(crate) source: (String, i64, i64),
    pub(crate) entries: Vec<TranscriptLine>,
    pub(crate) nested_bindings: Vec<(String, String)>,
    pub(crate) external_managed: Vec<acp::SessionId>,
}

/// An isolated session, not yet reachable through the parent. Dropping it closes its actor.
pub(crate) struct StagedNativeRebind {
    public_id: acp::SessionId,
    parent_id: acp::SessionId,
    agent: String,
    agent_name: String,
    generation: i64,
    launched: child::Launched,
}

impl StagedNativeRebind {
    /// Inspect the isolated journal and session before publishing the view mapping.
    pub(crate) fn handle(&self) -> HarnessHandle {
        self.launched.child.harness.clone()
    }
}

impl Orchestrator {
    /// Fork a native child's selected prefix using its own provider configuration.
    #[expect(
        clippy::too_many_arguments,
        reason = "the source, view, and provider configuration must stay explicit"
    )]
    pub(crate) async fn fork_native_history(
        &self,
        parent_view: acp::SessionId,
        agent: String,
        model: Option<String>,
        options: Vec<(String, String)>,
        context: String,
        source: (String, i64, i64),
        entries: Vec<TranscriptLine>,
    ) -> Result<(), String> {
        self.inner
            .delegate(super::launch::Delegation {
                task: "Continue from the conversation above.".to_owned(),
                agent: Some(agent),
                model,
                title: Some("History fork".to_owned()),
                role: None,
                isolation: super::launch::Isolation::Shared,
                mode: super::launch::Mode::Background,
                timeout: None,
                client_request_id: None,
                options,
                fork_context: Some(context),
                fork_source: Some(source),
                fork_entries: entries,
                parent_view: Some(parent_view),
            })
            .await
            .map(drop)
    }

    pub(crate) async fn prepare_native_rebind(
        &self,
        mut spec: NativeRebindSpec,
    ) -> Result<StagedNativeRebind, String> {
        if spec.source.0.trim().is_empty()
            || spec.source.1 < 0
            || spec.source.2 < spec.source.1
            || spec.public_id == spec.parent_id
        {
            return Err("the selected child history range is invalid".to_owned());
        }
        // Managed fork tasks and their descendants have their own owner; do not
        // reinterpret them as descendants of the adopted provider session.
        let mut managed: std::collections::HashSet<_> = spec
            .external_managed
            .iter()
            .filter(|id| *id != &spec.public_id)
            .cloned()
            .collect();
        let owned_managed: Vec<acp::SessionId> = spec
            .nested_bindings
            .iter()
            .filter_map(|(name, _)| {
                name.strip_prefix("managed-child-journal:")
                    .map(acp::SessionId::new)
            })
            .collect();
        loop {
            let before = managed.len();
            for entry in &spec.entries {
                if let crate::transcript::TranscriptEntry::SessionEvent(event) = &entry.entry
                    && let nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::Subagent(
                        nori_protocol::SubagentWire::Update(update),
                    )) = &event.event
                    && ((update.session_id.to_string().starts_with("nori-task-")
                        && !spec.nested_bindings.iter().any(|(name, _)| {
                            name == &format!("managed-child-journal:{}", update.session_id)
                        }))
                        || managed.contains(&update.parent_session_id)
                        || managed.iter().chain(owned_managed.iter()).any(
                            |scope: &acp::SessionId| {
                                update.session_id.to_string().starts_with(&format!(
                                    "nori-relay:{}:{scope}",
                                    scope.to_string().len()
                                ))
                            },
                        ))
                {
                    managed.insert(update.session_id.clone());
                }
            }
            if managed.len() == before {
                break;
            }
        }
        spec.entries.retain(|entry| {
            let crate::transcript::TranscriptEntry::SessionEvent(event) = &entry.entry else {
                return true;
            };
            match &event.event {
                nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::Subagent(
                    nori_protocol::SubagentWire::Update(update),
                )) => update.session_id != spec.public_id && !managed.contains(&update.session_id),
                nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::Subagent(
                    nori_protocol::SubagentWire::Message(message),
                )) => {
                    !message
                        .sender_session_id
                        .as_ref()
                        .is_some_and(|id| managed.contains(id))
                        && !message
                            .recipient_session_id
                            .as_ref()
                            .is_some_and(|id| managed.contains(id))
                }
                nori_protocol::SessionEvent::Nori(
                    nori_protocol::NoriEvent::SubagentQueueChanged { session_id, .. },
                ) => !managed.contains(session_id),
                nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::ChildNotification(
                    notification,
                )) => !managed.contains(&notification.session_id),
                _ => true,
            }
        });
        for entry in &spec.entries {
            if let crate::transcript::TranscriptEntry::SessionEvent(event) = &entry.entry
                && let nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::Subagent(
                    nori_protocol::SubagentWire::Update(update),
                )) = &event.event
                && !spec.nested_bindings.iter().any(|(name, _)| {
                    name == &format!("native-child-journal:{}", update.session_id)
                        || name == &format!("managed-child-journal:{}", update.session_id)
                })
            {
                return Err(format!(
                    "the selected history for {} contains descendant {} without a durable provider binding (managed owners: {:?})",
                    spec.public_id, update.session_id, spec.external_managed
                ));
            }
        }
        let generation = self.inner.generation.load(Ordering::Acquire);
        {
            let state = self.inner.state.lock().await;
            if state.shut_down
                || state
                    .tasks
                    .iter()
                    .any(|task| task.view_id == spec.public_id)
                || state.descendants.contains_key(&spec.public_id)
            {
                return Err("the child view is unavailable for adoption".to_owned());
            }
        }
        let agent_name = crate::registry::list_available_agents()
            .into_iter()
            .find(|candidate| candidate.agent_name == spec.agent)
            .map_or_else(|| spec.agent.clone(), |candidate| candidate.display_name);
        let mut config = (*self.inner.seed.config).clone();
        config.active_agent = spec.agent.clone();
        config.approval_policy = self.inner.approval_policy();
        if let Some(model) = &spec.model {
            let offered = super::catalog::remembered(&config.nori_home, &spec.agent).models;
            let value = super::models::find(&offered, model)
                .map_or_else(|| model.trim().to_owned(), |model| model.value.clone());
            config.default_models.insert(spec.agent.clone(), value);
        }
        let prepare = crate::runtime::AgentPrepareSpec {
            handoff: None,
            config: Arc::new(config),
            cli_version: self.inner.seed.cli_version.clone(),
            session_context: None,
            initial_context: Some(spec.context.clone()),
        };
        let launched = child::launch(
            &self.inner,
            prepare,
            spec.model.as_deref(),
            &spec.options,
            true,
            spec.public_id.clone(),
        )
        .await?;
        let (session_id, start, end) = spec.source;
        if let Err(error) = launched
            .child
            .harness
            .record_fork_source(session_id, start, end, spec.context, spec.entries)
            .await
        {
            let _ = launched.child.harness.shutdown().await;
            return Err(format!(
                "could not record the selected child history: {error:#}"
            ));
        }
        match launched.child.harness.conversation_recorder().await {
            Ok(Some(recorder)) => {
                for (name, payload) in &spec.nested_bindings {
                    if let Err(error) = recorder.record_command(name, payload.clone()).await {
                        let _ = launched.child.harness.shutdown().await;
                        return Err(format!(
                            "could not copy the selected child binding: {error:#}"
                        ));
                    }
                }
                if let Err(error) = recorder.replay_session_events().await {
                    let _ = launched.child.harness.shutdown().await;
                    return Err(format!(
                        "could not validate the staged child journal: {error:#}"
                    ));
                }
            }
            Ok(None) => {
                let _ = launched.child.harness.shutdown().await;
                return Err("the staged child has no conversation journal".to_owned());
            }
            Err(error) => {
                let _ = launched.child.harness.shutdown().await;
                return Err(format!(
                    "could not open the staged child journal: {error:#}"
                ));
            }
        }
        if !spec.nested_bindings.is_empty()
            && let Err(error) = launched
                .child
                .harness
                .restore_seeded_native_children()
                .await
        {
            let _ = launched.child.harness.shutdown().await;
            return Err(format!(
                "could not restore selected descendant sessions: {error:#}"
            ));
        }
        Ok(StagedNativeRebind {
            public_id: spec.public_id,
            parent_id: spec.parent_id,
            agent: spec.agent,
            agent_name,
            generation,
            launched,
        })
    }

    /// Reserve the view mapping but keep the staged actor silent until activation.
    /// The caller must hold its native quiescence and per-child routing guard through activation.
    pub(crate) async fn commit_native_rebind(
        &self,
        staged: StagedNativeRebind,
    ) -> Result<HarnessHandle, String> {
        let mut state = self.inner.state.lock().await;
        if state.shut_down || staged.generation != self.inner.generation.load(Ordering::Acquire) {
            return Err("the conversation changed before the child was adopted".to_owned());
        }
        if state
            .tasks
            .iter()
            .any(|task| task.view_id == staged.public_id)
            || state.descendants.contains_key(&staged.public_id)
        {
            return Err("the child view is already controlled by another session".to_owned());
        }
        state.next_child += 1;
        let id = TaskId::parse(&format!("adopted-{}", uuid::Uuid::new_v4()));
        let key = ChildKey(state.next_child);
        let now = Instant::now();
        let child = staged.launched.child;
        let handle = child.harness.clone();
        let view_id = staged.public_id;
        state.tasks.push(TaskRecord {
            id: id.clone(),
            adoption_anchor: true,
            child: key,
            view_id: view_id.clone(),
            parent_view: Some(staged.parent_id),
            agent: staged.agent,
            agent_name: staged.agent_name,
            model: staged.launched.model,
            title: "Continued child session".to_owned(),
            status: TaskStatus::Ended(TaskEnd::Completed {
                stop_reason: acp::StopReason::EndTurn,
            }),
            result: String::new(),
            delivery: Delivery::Waived,
            wake: Wake::Never,
            awaiting_approval: false,
            relayed_approvals: Vec::new(),
            started_at: now,
            ended_at: Some(now),
            worktree: None,
            progress: Default::default(),
        });
        state.children.insert(key, ChildSlot::Ready(child.clone()));
        state.pending_adoptions.insert(view_id.clone());
        // Attach has no await: a failure is rolled back before any reader can take the lock.
        if let Err(error) = child.attach(view_id.clone()) {
            state.children.remove(&key);
            state.tasks.retain(|task| task.id != id);
            state.pending_adoptions.remove(&view_id);
            return Err(error);
        }
        drop(state);
        self.inner.touch();
        Ok(handle)
    }

    /// Release buffered events only after the durable binding was persisted.
    pub(crate) async fn activate_native_rebind(&self, view: &acp::SessionId) -> Result<(), String> {
        let mut state = self.inner.state.lock().await;
        if !state.pending_adoptions.contains(view) {
            return Err("the child view has no pending adoption".to_owned());
        }
        let key = state
            .tasks
            .iter()
            .find(|task| task.adoption_anchor && &task.view_id == view)
            .map(|task| task.child)
            .ok_or_else(|| "the staged child is unavailable".to_owned())?;
        let Some(ChildSlot::Ready(child)) = state.children.get(&key) else {
            return Err("the staged child is unavailable".to_owned());
        };
        child.activate()?;
        state.pending_adoptions.remove(view);
        Ok(())
    }

    /// Discard a reserved, not-yet-activated mapping after a failed durable binding.
    pub(crate) async fn rollback_native_rebind(&self, view: &acp::SessionId) -> Result<(), String> {
        let (key, child) = {
            let state = self.inner.state.lock().await;
            if !state.pending_adoptions.contains(view) {
                return Err("the child view has no pending adoption".to_owned());
            }
            let key = state
                .tasks
                .iter()
                .find(|task| task.adoption_anchor && &task.view_id == view)
                .map(|task| task.child)
                .ok_or_else(|| "the staged child is unavailable".to_owned())?;
            if state
                .tasks
                .iter()
                .any(|task| task.child == key && !task.adoption_anchor)
            {
                return Err("the staged child has already admitted work".to_owned());
            }
            let Some(ChildSlot::Ready(child)) = state.children.get(&key) else {
                return Err("the staged child is unavailable".to_owned());
            };
            (key, child.clone())
        };
        child
            .harness
            .shutdown()
            .await
            .map_err(|error| format!("could not stop the staged child: {error:#}"))?;
        let mut state = self.inner.state.lock().await;
        state.children.remove(&key);
        state
            .tasks
            .retain(|task| !(task.adoption_anchor && task.child == key));
        state.pending_adoptions.remove(view);
        drop(state);
        self.inner.touch();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nori_config::AskForApproval;
    use nori_protocol::AcpEvent;
    use nori_protocol::SessionEvent;
    use tokio::sync::mpsc;
    use tokio::sync::watch;

    #[tokio::test]
    #[expect(clippy::expect_used, reason = "fixture failures should fail clearly")]
    async fn staged_adoption_is_hidden_until_commit_and_uses_the_existing_view() {
        let home = tempfile::tempdir().expect("temporary home");
        let config = crate::config::NoriConfig {
            active_agent: "mock-model".to_owned(),
            cwd: home.path().to_path_buf(),
            nori_home: home.path().to_path_buf(),
            approval_policy: AskForApproval::Never,
            ..Default::default()
        };
        let (events, mut event_rx) = mpsc::channel(64);
        let (orchestrator, _signals) = Orchestrator::new(
            super::super::Seed::root(Arc::new(config), "adoption-test".to_owned()),
            events,
        );
        orchestrator.bind(super::super::Binding {
            session_id: acp::SessionId::new("parent"),
            approval_policy: watch::channel(AskForApproval::Never).1,
        });
        let view = acp::SessionId::new("native-child-view");
        let spec = NativeRebindSpec {
            public_id: view.clone(),
            parent_id: acp::SessionId::new("parent"),
            agent: "mock-model".to_owned(),
            model: None,
            options: Vec::new(),
            context: "selected native prefix".to_owned(),
            source: ("native-journal".to_owned(), 1, 3),
            entries: Vec::new(),
            nested_bindings: Vec::new(),
            external_managed: Vec::new(),
        };
        let stale = orchestrator
            .prepare_native_rebind(spec.clone())
            .await
            .expect("stage stale child");
        orchestrator.advance_generation();
        assert!(orchestrator.commit_native_rebind(stale).await.is_err());
        assert!(!orchestrator.owns_view(&view).await);
        assert!(
            event_rx.try_recv().is_err(),
            "a rejected stage must not publish events"
        );
        let rolled_back = orchestrator
            .prepare_native_rebind(spec.clone())
            .await
            .expect("stage rollback child");
        orchestrator
            .commit_native_rebind(rolled_back)
            .await
            .expect("reserve rollback child");
        assert!(!orchestrator.owns_view(&view).await);
        assert!(
            event_rx.try_recv().is_err(),
            "a reserved stage must stay silent"
        );
        orchestrator
            .rollback_native_rebind(&view)
            .await
            .expect("rollback child");
        assert!(!orchestrator.owns_view(&view).await);
        assert!(
            event_rx.try_recv().is_err(),
            "rollback must not publish a closed view"
        );
        let duplicate = spec.clone();
        let staged = orchestrator
            .prepare_native_rebind(spec)
            .await
            .expect("stage child");
        let recorder = staged
            .handle()
            .conversation_recorder()
            .await
            .expect("recorder request")
            .expect("child journal");
        assert_eq!(
            recorder
                .recorded_command_payload("fork-context")
                .await
                .as_deref(),
            Some("selected native prefix")
        );
        assert!(!orchestrator.owns_view(&view).await);
        assert!(
            event_rx.try_recv().is_err(),
            "staging must not publish events"
        );
        assert!(!orchestrator.has_pending_work().await);
        orchestrator
            .commit_native_rebind(staged)
            .await
            .expect("commit child");
        assert!(!orchestrator.owns_view(&view).await);
        assert!(
            event_rx.try_recv().is_err(),
            "a reserved stage must stay silent"
        );
        orchestrator
            .activate_native_rebind(&view)
            .await
            .expect("activate child");
        assert!(orchestrator.owns_view(&view).await);
        assert!(!orchestrator.has_pending_work().await);
        assert!(
            orchestrator.inner.list().await.is_empty(),
            "adoption is not an agent task"
        );
        assert!(orchestrator.prepare_native_rebind(duplicate).await.is_err());
        orchestrator
            .send_user_content_message_locked(
                view.clone(),
                vec![acp::ContentBlock::Text(acp::TextContent::new(
                    "continue the child",
                ))],
            )
            .await
            .expect("continue adopted child");
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                let event = event_rx.recv().await.expect("parent event stream");
                if let crate::backend::BackendEvent::Public(SessionEvent::Acp(
                    AcpEvent::ChildNotification(notification),
                )) = event
                    && notification.session_id == view
                {
                    break;
                }
            }
        })
        .await
        .expect("adopted child events retain the public ID");
        orchestrator.shutdown().await;
    }
}
