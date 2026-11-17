//! Admit background continuations only to the conversation generation that created them.

use super::AcpBackend;
use super::orchestrator::Orchestrator;
use super::session_reducer::InboundEvent;

async fn run_current<F, Fut>(orchestrator: &Orchestrator, generation: i64, run: F)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let _admission = orchestrator.rewind_admission().await;
    if generation == orchestrator.admission_generation() {
        run().await;
    }
}

impl AcpBackend {
    pub(super) async fn apply_guarded_continuation(&self, generation: i64, event: InboundEvent) {
        run_current(&self.orchestrator, generation, || {
            self.apply_session_event(event)
        })
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::sync::Arc;
    use std::sync::atomic::AtomicI64;
    use std::sync::atomic::Ordering;

    #[tokio::test]
    async fn old_goal_and_workflow_continuations_cannot_cross_rewind_admission() {
        let (events, _) = tokio::sync::mpsc::channel(8);
        let seed = super::super::orchestrator::Seed::root(
            Arc::new(crate::config::NoriConfig::default()),
            "test".to_owned(),
        );
        let (orchestrator, _) = Orchestrator::new(seed, events);
        let generation = orchestrator.admission_generation();
        let guard = orchestrator.rewind_admission().await;
        let applied = Arc::new(AtomicI64::new(0));
        let prompt = |kind| {
            InboundEvent::PromptSubmit(crate::normalized::session_runtime::QueuedPrompt {
                event_id: "stale".to_owned(),
                kind,
                text: "discarded turn".to_owned(),
                prompt_meta: None,
                original_content: Vec::new(),
                content: Vec::new(),
                display_text: None,
            })
        };
        let goal_event =
            prompt(crate::normalized::session_runtime::QueuedPromptKind::GoalContinuation);
        let workflow_event =
            prompt(crate::normalized::session_runtime::QueuedPromptKind::WorkflowHook);
        let goal = run_current(&orchestrator, generation, || async {
            assert!(matches!(goal_event, InboundEvent::PromptSubmit(_)));
            applied.fetch_add(1, Ordering::SeqCst);
        });
        let workflow = run_current(&orchestrator, generation, || async {
            assert!(matches!(workflow_event, InboundEvent::PromptSubmit(_)));
            applied.fetch_add(10, Ordering::SeqCst);
        });
        tokio::pin!(goal);
        tokio::pin!(workflow);
        assert!(futures::poll!(&mut goal).is_pending());
        assert!(futures::poll!(&mut workflow).is_pending());
        orchestrator.advance_generation();
        drop(guard);
        goal.await;
        workflow.await;
        assert_eq!(applied.load(Ordering::SeqCst), 0);
        run_current(
            &orchestrator,
            orchestrator.admission_generation(),
            || async {
                applied.fetch_add(1, Ordering::SeqCst);
            },
        )
        .await;
        assert_eq!(applied.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn queued_old_continuations_never_reach_the_rewound_provider() {
        use super::super::session_runtime_driver::SessionRuntimeInput;
        use crate::normalized::session_runtime::QueuedPrompt;
        use crate::normalized::session_runtime::QueuedPromptKind;
        use nori_protocol::acp::v1 as acp;

        let home = tempfile::tempdir().expect("session home");
        let wire = home.path().join("wire");
        let config = crate::config::NoriConfig {
            active_agent: "mock-model".to_owned(),
            cwd: home.path().to_path_buf(),
            nori_home: home.path().to_path_buf(),
            acp_proxy: nori_config::AcpProxyConfig {
                enabled: true,
                log_dir: wire.clone(),
            },
            ..Default::default()
        };
        let prepared = crate::runtime::prepare_agent(crate::runtime::AgentPrepareSpec {
            handoff: None,
            config: Arc::new(config),
            cli_version: "continuation-guard-test".to_owned(),
            session_context: None,
            initial_context: None,
        })
        .await
        .expect("prepare mock agent");
        let (events, mut receiver) = tokio::sync::mpsc::channel(128);
        tokio::spawn(async move { while receiver.recv().await.is_some() {} });
        let backend = AcpBackend::spawn(prepared, events)
            .await
            .expect("start mock backend");
        let recorder = backend
            .transcript_recorder
            .read()
            .await
            .clone()
            .expect("journal");
        let old = backend.orchestrator.admission_generation();
        backend
            .rewind_and_rebind(recorder.session_id().to_owned(), None)
            .await
            .expect("rewind session");
        let prompt = |kind, text: &str| {
            InboundEvent::PromptSubmit(QueuedPrompt {
                event_id: text.to_owned(),
                kind,
                text: text.to_owned(),
                prompt_meta: None,
                original_content: Vec::new(),
                content: vec![acp::ContentBlock::Text(acp::TextContent::new(text))],
                display_text: None,
            })
        };
        for (kind, text) in [
            (QueuedPromptKind::GoalContinuation, "discarded goal"),
            (QueuedPromptKind::WorkflowHook, "discarded workflow"),
        ] {
            backend
                .session_event_tx
                .send(SessionRuntimeInput::GuardedContinuation {
                    generation: old,
                    event: prompt(kind, text),
                })
                .await
                .expect("queue old continuation");
        }
        let (barrier_tx, barrier_rx) = tokio::sync::oneshot::channel();
        backend
            .session_event_tx
            .send(SessionRuntimeInput::Barrier {
                response_tx: barrier_tx,
            })
            .await
            .expect("queue barrier");
        barrier_rx.await.expect("old continuations drained");
        backend
            .session_event_tx
            .send(SessionRuntimeInput::GuardedContinuation {
                generation: backend.orchestrator.admission_generation(),
                event: prompt(QueuedPromptKind::WorkflowHook, "current workflow"),
            })
            .await
            .expect("queue current continuation");
        let wire_text = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let logs = std::fs::read_dir(&wire)
                    .ok()
                    .into_iter()
                    .flatten()
                    .filter_map(Result::ok)
                    .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
                    .collect::<String>();
                if logs.contains("current workflow") {
                    break logs;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("current continuation reaches provider");
        assert!(!wire_text.contains("discarded goal"), "{wire_text}");
        assert!(!wire_text.contains("discarded workflow"), "{wire_text}");
        backend
            .shutdown(std::time::Duration::from_secs(1))
            .await
            .expect("shutdown backend");
    }
}
