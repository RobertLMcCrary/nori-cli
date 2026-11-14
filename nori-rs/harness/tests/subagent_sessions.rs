//! Subagent traffic through a real harness session backed by the mock agent, which replays wire
//! traffic recorded from a real adapter.

use std::sync::Arc;
use std::time::Duration;

use nori_config::NoriConfig;
use nori_harness::runtime::AgentPrepareSpec;
use nori_harness::runtime::LaunchedSession;
use nori_harness::runtime::SessionLaunchSpec;
use nori_harness::runtime::SessionStart;
use nori_harness::runtime::launch_session;
use nori_harness::runtime::prepare_agent;
use nori_protocol::AcpEvent;
use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;
use nori_protocol::SubagentWire;
use nori_protocol::acp::v1 as acp;
use pretty_assertions::assert_eq;
use serial_test::serial;

struct EnvGuard(&'static [&'static str]);

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for key in self.0 {
            // SAFETY: tests that mutate the mock-agent environment run serially.
            unsafe { std::env::remove_var(key) };
        }
    }
}

fn fixture_path(name: &str) -> String {
    format!(
        "{}/../mock-acp-agent/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    )
}

#[expect(clippy::expect_used, reason = "fixture failures should fail loudly")]
async fn launch(fixture: &str) -> (LaunchedSession, tempfile::TempDir, EnvGuard) {
    // SAFETY: tests that mutate the mock-agent environment run serially.
    unsafe {
        std::env::set_var("MOCK_AGENT_WIRE_REPLAY", fixture_path(fixture));
        std::env::set_var("MOCK_AGENT_REPLAY_SCALE", "0");
    }
    let guard = EnvGuard(&["MOCK_AGENT_WIRE_REPLAY", "MOCK_AGENT_REPLAY_SCALE"]);
    let temp = tempfile::tempdir().expect("session directory");
    let config = NoriConfig {
        active_agent: "mock-model".to_string(),
        cwd: temp.path().to_path_buf(),
        nori_home: temp.path().to_path_buf(),
        ..Default::default()
    };
    let agent = prepare_agent(AgentPrepareSpec {
        config: Arc::new(config),
        cli_version: "subagent-test".to_string(),
        session_context: None,
        initial_context: None,
    })
    .await
    .expect("prepare mock agent");
    let mut session = launch_session(SessionLaunchSpec {
        agent,
        start: SessionStart::New,
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        while !matches!(
            session.events.recv().await,
            Some(SessionEvent::Nori(NoriEvent::SessionStarted(_)))
        ) {}
    })
    .await
    .expect("session starts");
    (session, temp, guard)
}

/// Submit a prompt and collect every event up to the prompt's response.
#[expect(clippy::expect_used, reason = "fixture failures should fail loudly")]
async fn run_prompt(session: &mut LaunchedSession) -> Vec<SessionEvent> {
    let request_id = session
        .handle
        .prompt(vec![acp::ContentBlock::Text(acp::TextContent::new("go"))])
        .await
        .expect("prompt accepted");
    let mut events = Vec::new();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let event = session.events.recv().await.expect("event stream open");
            let done = matches!(
                &event,
                SessionEvent::Acp(AcpEvent::Response { request_id: id, .. }) if *id == request_id
            );
            events.push(event);
            if done {
                break;
            }
        }
    })
    .await
    .expect("prompt completes");
    // Anything the agent sent right behind the response.
    while let Ok(Some(event)) =
        tokio::time::timeout(Duration::from_millis(200), session.events.recv()).await
    {
        events.push(event);
    }
    events
}

fn root_text(events: &[SessionEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::Acp(AcpEvent::Notification(
                acp::AgentNotification::SessionNotification(notification),
            )) => match &notification.update {
                acp::SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
                    acp::ContentBlock::Text(text) => Some(text.text.clone()),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        })
        .collect()
}

#[tokio::test]
#[serial]
async fn announced_children_are_tagged_and_kept_out_of_the_roots_stream() {
    let (mut session, _temp, _guard) = launch("claude_subagents.jsonl").await;
    let events = run_prompt(&mut session).await;
    session.handle.shutdown().await.expect("shutdown");

    let announcements: Vec<&SubagentWire> = events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::Nori(NoriEvent::Subagent(wire)) => Some(wire),
            _ => None,
        })
        .collect();
    let updates = announcements
        .iter()
        .filter(|wire| matches!(wire, SubagentWire::Update(_)))
        .count();
    assert_eq!(updates, 4, "two announcements and two idle states");
    assert!(
        announcements
            .iter()
            .any(|wire| matches!(wire, SubagentWire::Message(_)))
    );

    let child_updates = events
        .iter()
        .filter(|event| matches!(event, SessionEvent::Acp(AcpEvent::ChildNotification(_))))
        .count();
    assert!(child_updates >= 8, "{child_updates}");

    // The children's words never appear among the root's chunks.
    let text = root_text(&events);
    assert!(
        text.contains("Both subagents are running in parallel"),
        "{text}"
    );
    assert!(text.contains("a.txt contains \"alpha\""), "{text}");
    assert!(!text.contains("The file exists and I read it"), "{text}");
    assert!(!text.contains("has **2 lines**"), "{text}");

    // The turn ended exactly once, and not because a child went idle.
    let phases: Vec<&NoriEvent> = events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::Nori(event @ NoriEvent::SessionPhaseChanged(_)) => Some(event),
            _ => None,
        })
        .collect();
    let idles = phases
        .iter()
        .filter(|event| {
            matches!(
                event,
                NoriEvent::SessionPhaseChanged(nori_protocol::SessionPhase::Idle)
            )
        })
        .count();
    assert_eq!(idles, 1, "{phases:?}");
}

#[tokio::test]
#[serial]
async fn without_announcements_everything_stays_on_the_root() {
    let (mut session, _temp, _guard) = launch("claude_plain.jsonl").await;
    let events = run_prompt(&mut session).await;
    session.handle.shutdown().await.expect("shutdown");

    assert!(
        !events.iter().any(|event| matches!(
            event,
            SessionEvent::Acp(AcpEvent::ChildNotification(_))
                | SessionEvent::Nori(NoriEvent::Subagent(_))
        )),
        "a plain adapter announces nothing"
    );
    // The tagged tool calls are root-session traffic carrying their launcher's id.
    let tagged = events
        .iter()
        .filter(|event| match event {
            SessionEvent::Acp(AcpEvent::Notification(
                acp::AgentNotification::SessionNotification(notification),
            )) => match &notification.update {
                acp::SessionUpdate::ToolCall(call) => {
                    nori_protocol::ClaudeToolMeta::read(call.meta.as_ref())
                        .parent_tool_use_id
                        .is_some()
                }
                _ => false,
            },
            _ => false,
        })
        .count();
    assert_eq!(tagged, 2);
}
