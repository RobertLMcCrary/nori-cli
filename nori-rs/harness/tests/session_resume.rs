//! Which reattach path a resume takes, through a real harness session backed by the mock agent.

use std::sync::Arc;
use std::time::Duration;

use nori_config::NoriConfig;
use nori_harness::runtime::AgentPrepareSpec;
use nori_harness::runtime::SessionLaunchSpec;
use nori_harness::runtime::SessionResume;
use nori_harness::runtime::SessionStart;
use nori_harness::runtime::launch_session;
use nori_harness::runtime::prepare_agent;
use nori_harness::transcript::TranscriptLoader;
use nori_harness::transcript::TranscriptRecorder;
use nori_protocol::AcpEvent;
use nori_protocol::NoriEvent;
use nori_protocol::ReplaySource;
use nori_protocol::SessionEvent;
use nori_protocol::acp::v1 as acp;
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

const ENV: &[&str] = &[
    "MOCK_AGENT_SUPPORT_LOAD_SESSION",
    "MOCK_AGENT_SUPPORT_SESSION_RESUME",
    "MOCK_AGENT_LOAD_SESSION_NOTIFICATION_COUNT",
];

/// Resume `acp_session_id` and collect events until the session has started and any replay ended.
#[expect(clippy::expect_used, reason = "fixture failures should fail loudly")]
async fn resume(
    home: &std::path::Path,
    acp_session_id: &str,
    transcript: Option<nori_harness::transcript::Transcript>,
) -> Vec<SessionEvent> {
    let config = NoriConfig {
        active_agent: "mock-model".to_string(),
        cwd: home.to_path_buf(),
        nori_home: home.to_path_buf(),
        ..Default::default()
    };
    let agent = prepare_agent(AgentPrepareSpec {
        handoff: None,
        config: Arc::new(config),
        cli_version: "resume-test".to_string(),
        session_context: None,
        initial_context: None,
    })
    .await
    .expect("prepare mock agent");
    let mut session = launch_session(SessionLaunchSpec {
        agent,
        start: SessionStart::Resume(SessionResume {
            acp_session_id: Some(acp_session_id.to_string()),
            transcript,
        }),
    });
    let mut events = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = session.events.recv().await.expect("event stream open");
            let ready = matches!(event, SessionEvent::Nori(NoriEvent::SessionReady(_)));
            events.push(event);
            if ready {
                break;
            }
        }
    })
    .await
    .expect("resume completes");
    session.handle.shutdown().await.expect("shutdown");
    events
}

fn replay_sources(events: &[SessionEvent]) -> Vec<ReplaySource> {
    events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::Nori(NoriEvent::ReplayStarted(started)) => Some(started.source),
            _ => None,
        })
        .collect()
}

fn user_texts(events: &[SessionEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::Acp(AcpEvent::Notification(
                acp::AgentNotification::SessionNotification(notification),
            )) => match &notification.update {
                acp::SessionUpdate::UserMessageChunk(chunk) => match &chunk.content {
                    acp::ContentBlock::Text(text) => Some(text.text.clone()),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// With the local transcript at hand, an agent that can both load and resume is reattached with
/// `session/resume`, which costs the same however long the session is, and the history is painted
/// from the transcript instead of being streamed back by the agent.
#[tokio::test]
#[serial]
async fn a_resume_with_a_transcript_reattaches_and_paints_from_the_transcript() {
    // SAFETY: tests that mutate the mock-agent environment run serially.
    unsafe {
        std::env::set_var("MOCK_AGENT_SUPPORT_LOAD_SESSION", "1");
        std::env::set_var("MOCK_AGENT_SUPPORT_SESSION_RESUME", "1");
        std::env::set_var("MOCK_AGENT_LOAD_SESSION_NOTIFICATION_COUNT", "3");
    }
    let _guard = EnvGuard(ENV);
    let home = tempfile::tempdir().expect("home");

    let recorder = TranscriptRecorder::new(
        home.path(),
        home.path(),
        Some("mock-model".to_string()),
        "resume-test",
        Some("acp-session-1".to_string()),
    )
    .await
    .expect("recorder");
    recorder
        .record_user_message("u1", "the earlier prompt", vec![])
        .await
        .expect("record");
    recorder.shutdown().await.expect("flush transcript");
    let transcript = TranscriptLoader::new(home.path().to_path_buf())
        .load_transcript(recorder.project_id(), recorder.session_id())
        .await
        .expect("load transcript");

    let events = resume(home.path(), "acp-session-1", Some(transcript)).await;
    assert!(
        matches!(events.last(), Some(SessionEvent::Nori(NoriEvent::SessionReady(id))) if events.iter().any(|event| matches!(event, SessionEvent::Nori(NoriEvent::SessionStarted(started)) if &started.acp_session_id == id)))
    );
    assert!(matches!(
        events.iter().rev().nth(1),
        Some(SessionEvent::Nori(NoriEvent::ReplayFinished))
    ));
    assert_eq!(replay_sources(&events), vec![ReplaySource::Transcript]);
    assert!(
        user_texts(&events)
            .iter()
            .any(|text| text.contains("the earlier prompt")),
        "{:?}",
        user_texts(&events)
    );
}

/// Without a transcript the agent's own replay is the only history there is, so `session/load` is
/// still used.
#[tokio::test]
#[serial]
async fn a_resume_without_a_transcript_loads_the_agents_history() {
    // SAFETY: tests that mutate the mock-agent environment run serially.
    unsafe {
        std::env::set_var("MOCK_AGENT_SUPPORT_LOAD_SESSION", "1");
        std::env::set_var("MOCK_AGENT_SUPPORT_SESSION_RESUME", "1");
        std::env::set_var("MOCK_AGENT_LOAD_SESSION_NOTIFICATION_COUNT", "3");
    }
    let _guard = EnvGuard(ENV);
    let home = tempfile::tempdir().expect("home");
    let events = resume(home.path(), "acp-session-2", None).await;
    assert_eq!(replay_sources(&events), vec![ReplaySource::Agent]);
}
