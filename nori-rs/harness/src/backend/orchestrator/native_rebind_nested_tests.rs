use super::*;
use crate::transcript::SessionEventEntry;
use crate::transcript::TranscriptEntry;
use crate::transcript::TranscriptLine;
use nori_config::AskForApproval;
use nori_conversation::Event;
use nori_conversation::Record;
use nori_protocol::NoriEvent;
use nori_protocol::Patch;
use nori_protocol::SessionEvent;
use nori_protocol::SubagentState;
use nori_protocol::SubagentUpdate;
use nori_protocol::SubagentWire;
use pretty_assertions::assert_eq;
use tokio::sync::mpsc;
use tokio::sync::watch;

#[tokio::test]
async fn selected_descendant_binding_is_staged_and_restored_without_future_children() {
    let home = tempfile::tempdir().unwrap();
    let config = crate::config::NoriConfig {
        active_agent: "mock-model".into(),
        cwd: home.path().into(),
        nori_home: home.path().into(),
        approval_policy: AskForApproval::Never,
        ..Default::default()
    };
    let (events, mut event_rx) = mpsc::channel(128);
    let (orchestrator, _) = Orchestrator::new(
        Seed::root(Arc::new(config), "nested-stage-test".into()),
        events,
    );
    orchestrator.bind(Binding {
        session_id: acp::SessionId::new("root"),
        approval_policy: watch::channel(AskForApproval::Never).1,
    });
    let spec = |id: &str| NativeRebindSpec {
        public_id: acp::SessionId::new(id),
        parent_id: acp::SessionId::new("root"),
        agent: "mock-model".into(),
        model: None,
        options: Vec::new(),
        context: "selected".into(),
        source: ("source".into(), 1, 3),
        entries: Vec::new(),
        nested_bindings: Vec::new(),
        external_managed: Vec::new(),
    };
    let descendant = orchestrator
        .prepare_native_rebind(spec("journal-source"))
        .await
        .unwrap();
    let recorder = descendant
        .handle()
        .conversation_recorder()
        .await
        .unwrap()
        .unwrap();
    let payload = serde_json::json!({"project": recorder.project_id(), "session": recorder.session_id(), "parent": "old-parent", "agent": "mock-model", "model": "mock-model-default", "options": [], "rebound": true}).to_string();
    let announcement =
        SessionEvent::Nori(NoriEvent::Subagent(SubagentWire::Update(SubagentUpdate {
            parent_session_id: acp::SessionId::new("old-parent"),
            session_id: acp::SessionId::new("selected"),
            agent: Patch::Keep,
            title: Patch::Keep,
            description: Patch::Keep,
            state: Patch::Set(SubagentState::Idle),
        })));
    let records = [
        Record {
            id: 1,
            command_id: None,
            event: Event::Operation {
                name: "transcript-entry".into(),
                args: serde_json::to_string(&TranscriptLine::new(TranscriptEntry::SessionEvent(
                    SessionEventEntry::new(announcement),
                )))
                .unwrap(),
            },
        },
        Record {
            id: 2,
            command_id: None,
            event: Event::Command {
                name: "native-child-journal:selected".into(),
                payload: payload.clone(),
            },
        },
        Record {
            id: 3,
            command_id: None,
            event: Event::Command {
                name: "native-child-journal:future".into(),
                payload,
            },
        },
    ];
    let selected = &records[..2];
    let bindings = crate::transcript::fork_context::child_bindings_from_records(selected);
    assert_eq!(bindings.len(), 1);
    assert_eq!(bindings[0].0, "native-child-journal:selected");
    let mut parent = spec("parent-view");
    parent.entries = crate::transcript::fork_context::replay_entries_with_children_from_records(
        selected, "source",
    );
    assert_eq!(parent.entries.len(), 1);
    parent.nested_bindings = bindings;
    let staged = orchestrator.prepare_native_rebind(parent).await.unwrap();
    let staged_recorder = staged
        .handle()
        .conversation_recorder()
        .await
        .unwrap()
        .unwrap();
    assert!(
        staged_recorder
            .recorded_command_payload("native-child-journal:selected")
            .await
            .is_some()
    );
    assert!(
        staged_recorder
            .recorded_command_payload("native-child-journal:future")
            .await
            .is_none()
    );
    assert!(
        event_rx.try_recv().is_err(),
        "staged descendant events must remain private"
    );
    orchestrator.commit_native_rebind(staged).await.unwrap();
    orchestrator
        .activate_native_rebind(&acp::SessionId::new("parent-view"))
        .await
        .unwrap();
    let child = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            if let crate::backend::BackendEvent::Public(SessionEvent::Nori(NoriEvent::Subagent(
                SubagentWire::Update(update),
            ))) = event_rx.recv().await.unwrap()
                && update.parent_session_id == acp::SessionId::new("parent-view")
                && update.session_id.to_string().ends_with("selected")
            {
                break update.session_id;
            }
        }
    })
    .await
    .unwrap();
    assert!(
        orchestrator
            .controlled_subagent_handle(&child)
            .await
            .is_some()
    );
    orchestrator.shutdown().await;
}
