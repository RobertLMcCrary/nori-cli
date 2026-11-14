//! Delegated tasks end to end: a parent on one mock agent delegates to a child on another, through
//! the `nori-client` MCP server the harness gives it.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use nori_config::NoriConfig;
use nori_harness::runtime::AgentPrepareSpec;
use nori_harness::runtime::LaunchedSession;
use nori_harness::runtime::SessionStart;
use nori_harness::runtime::prepare_and_launch_session;
use nori_protocol::AcpEvent;
use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;
use nori_protocol::SubagentOutcome;
use nori_protocol::SubagentState;
use nori_protocol::SubagentWire;
use nori_protocol::acp::v1 as acp;
use serial_test::serial;

/// Every variable a test here sets, cleared when it ends.
struct Env(Vec<&'static str>);

impl Env {
    fn set(vars: &[(&'static str, String)]) -> Self {
        for (key, value) in vars {
            // SAFETY: tests that mutate the mock-agent environment run serially.
            unsafe { std::env::set_var(key, value) };
        }
        Self(vars.iter().map(|(key, _)| *key).collect())
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        for key in &self.0 {
            // SAFETY: tests that mutate the mock-agent environment run serially.
            unsafe { std::env::remove_var(key) };
        }
    }
}

fn launch(home: &Path) -> LaunchedSession {
    let config = NoriConfig {
        active_agent: "mock-model".to_string(),
        cwd: home.to_path_buf(),
        nori_home: home.to_path_buf(),
        approval_policy: nori_config::AskForApproval::Never,
        ..Default::default()
    };
    prepare_and_launch_session(
        AgentPrepareSpec {
            config: Arc::new(config),
            cli_version: "delegation-test".to_string(),
            session_context: None,
            initial_context: None,
        },
        SessionStart::New,
    )
}

/// Collect events until `done` says so, failing after `limit`.
#[expect(clippy::expect_used, reason = "fixture failures should fail loudly")]
async fn collect_until(
    session: &mut LaunchedSession,
    limit: Duration,
    mut done: impl FnMut(&[SessionEvent]) -> bool,
) -> Vec<SessionEvent> {
    let mut events = Vec::new();
    let collected = tokio::time::timeout(limit, async {
        while !done(&events) {
            let event = session.events.recv().await.expect("event stream open");
            events.push(event);
        }
    })
    .await;
    assert!(collected.is_ok(), "timed out; events so far: {events:#?}");
    events
}

fn subagent_states(events: &[SessionEvent]) -> Vec<(String, SubagentState)> {
    events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::Nori(NoriEvent::Subagent(SubagentWire::Update(update))) => {
                match &update.state {
                    nori_protocol::Patch::Set(state) => {
                        Some((update.session_id.to_string(), state.clone()))
                    }
                    nori_protocol::Patch::Keep | nori_protocol::Patch::Clear => None,
                }
            }
            _ => None,
        })
        .collect()
}

fn child_text(events: &[SessionEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::Acp(AcpEvent::ChildNotification(notification)) => {
                match &notification.update {
                    acp::SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
                        acp::ContentBlock::Text(text) => Some(text.text.clone()),
                        _ => None,
                    },
                    _ => None,
                }
            }
            _ => None,
        })
        .collect()
}

fn tool_results(events: &[SessionEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::Acp(AcpEvent::Notification(
                acp::AgentNotification::SessionNotification(notification),
            )) => match &notification.update {
                acp::SessionUpdate::ToolCallUpdate(update) => {
                    update.fields.content.as_ref().map(|content| {
                        content
                            .iter()
                            .filter_map(|content| match content {
                                acp::ToolCallContent::Content(content) => match &content.content {
                                    acp::ContentBlock::Text(text) => Some(text.text.clone()),
                                    _ => None,
                                },
                                _ => None,
                            })
                            .collect::<String>()
                    })
                }
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn prompts(log: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn started_turns(events: &[SessionEvent]) -> usize {
    events
        .iter()
        .filter(|event| {
            matches!(
                event,
                SessionEvent::Nori(NoriEvent::SessionPhaseChanged(
                    nori_protocol::SessionPhase::Prompting { .. }
                ))
            )
        })
        .count()
}

fn is_idle(event: &SessionEvent) -> bool {
    matches!(
        event,
        SessionEvent::Nori(NoriEvent::SessionPhaseChanged(
            nori_protocol::SessionPhase::Idle
        ))
    )
}

async fn started(session: &mut LaunchedSession) {
    collect_until(session, Duration::from_secs(20), |events| {
        events
            .iter()
            .any(|event| matches!(event, SessionEvent::Nori(NoriEvent::SessionStarted(_))))
    })
    .await;
}

async fn prompt(session: &LaunchedSession, text: &str) {
    let content = vec![acp::ContentBlock::Text(acp::TextContent::new(text))];
    if let Err(error) = session.handle.prompt(content).await {
        panic!("prompt: {error:#}");
    }
}

/// A task delegated in the background to a different agent runs as a subagent of the parent, and
/// its result reaches the parent agent once, in a completion wake, when the parent is idle.
#[tokio::test]
#[serial]
async fn a_background_task_on_another_agent_wakes_the_parent_with_its_result() {
    let home = tempfile::tempdir().unwrap_or_else(|error| panic!("home: {error}"));
    let log = home.path().join("prompts.jsonl");
    let _env = Env::set(&[
        ("MOCK_AGENT_MCP_HTTP", "1".to_string()),
        (
            "MOCK_AGENT_MCP_CALLS_MOCK_MODEL",
            serde_json::json!([{
                "tool": "delegate_task",
                "arguments": {"task": "Read a.txt and report it", "agent": "mock-model-alt", "title": "Read a.txt"}
            }])
            .to_string(),
        ),
        ("MOCK_AGENT_RESPONSE_MOCK_MODEL_ALT", "The file holds alpha.".to_string()),
        ("MOCK_AGENT_PROMPT_LOG", log.display().to_string()),
    ]);
    let mut session = launch(home.path());
    started(&mut session).await;
    prompt(&session, "DELEGATE the reading").await;

    // The parent's own turn ends with the task handle; the child then runs and ends; then the
    // wake turn runs and ends.
    let events = collect_until(&mut session, Duration::from_secs(60), |events| {
        started_turns(events) >= 2 && events.last().is_some_and(is_idle)
    })
    .await;

    let states = subagent_states(&events);
    assert!(
        matches!(states.first(), Some((_, SubagentState::Running))),
        "{states:?}"
    );
    assert!(
        states.iter().any(|(_, state)| matches!(
            state,
            SubagentState::Ended(SubagentOutcome::Completed { .. })
        )),
        "{states:?}"
    );
    assert!(child_text(&events).contains("The file holds alpha."));
    assert!(
        tool_results(&events)
            .iter()
            .any(|result| result.contains("\"task_id\":\"task-1\"")
                && result.contains("delivered to you")),
        "{:?}",
        tool_results(&events)
    );

    let parent_prompts: Vec<String> = prompts(&log)
        .into_iter()
        .filter(|prompt| prompt["agent"] == "mock-model")
        .filter_map(|prompt| prompt["text"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(parent_prompts.len(), 2, "{parent_prompts:?}");
    assert!(parent_prompts[1].contains("<delegated_task_results>"));
    assert!(parent_prompts[1].contains("The file holds alpha."));
    assert!(parent_prompts[1].contains("agent=\"mock-model-alt\""));
    session
        .handle
        .shutdown()
        .await
        .unwrap_or_else(|error| panic!("{error:#}"));
}

/// A task the parent waits on returns its result to that call, so no wake follows; a follow-up
/// reaches the same child session; a model is chosen by name, and one the agent does not list is
/// still given to an agent that takes its model at spawn, which then runs it.
#[tokio::test]
#[serial]
async fn waits_follow_ups_and_models() {
    let home = tempfile::tempdir().unwrap_or_else(|error| panic!("home: {error}"));
    let log = home.path().join("prompts.jsonl");
    let _env = Env::set(&[
        ("MOCK_AGENT_MCP_HTTP", "1".to_string()),
        (
            "MOCK_AGENT_MCP_CALLS_MOCK_MODEL",
            serde_json::json!([
                {"tool": "delegate_task", "arguments": {"task": "first", "agent": "mock-model-alt", "mode": "wait", "model": "Mock Fast Model"}},
                {"tool": "task_send", "arguments": {"task_id": "$task_id", "message": "second", "mode": "wait"}},
                {"tool": "delegate_task", "arguments": {"task": "third", "agent": "mock-model-alt", "mode": "wait", "model": "gpt-9"}}
            ])
            .to_string(),
        ),
        ("MOCK_AGENT_RESPONSE_MOCK_MODEL_ALT", "Child answer.".to_string()),
        ("MOCK_AGENT_PROMPT_LOG", log.display().to_string()),
    ]);
    let mut session = launch(home.path());
    started(&mut session).await;
    prompt(&session, "DELEGATE twice").await;
    let events = collect_until(&mut session, Duration::from_secs(60), |events| {
        started_turns(events) >= 1 && events.last().is_some_and(is_idle)
    })
    .await;
    // Nothing is owed, so no wake starts.
    let quiet = tokio::time::timeout(Duration::from_millis(1500), session.events.recv()).await;
    assert!(
        !matches!(
            quiet,
            Ok(Some(SessionEvent::Nori(NoriEvent::SessionPhaseChanged(_))))
        ),
        "{quiet:?}"
    );

    let results = tool_results(&events);
    assert!(
        results[0].contains("\"status\":\"completed\""),
        "{results:?}"
    );
    assert!(
        results[0].contains("\"model\":\"Mock Fast Model\""),
        "{results:?}"
    );
    assert!(
        results[0].contains("\"result\":\"Child answer.\""),
        "{results:?}"
    );
    assert!(results[1].contains("\"task_id\":\"task-2\""), "{results:?}");
    assert!(
        results[1].contains("\"result\":\"Child answer.\""),
        "{results:?}"
    );
    assert!(results[2].contains("\"model\":\"gpt-9\""), "{results:?}");
    assert!(
        results[2].contains("\"status\":\"completed\""),
        "{results:?}"
    );

    let children: Vec<(i64, String)> = prompts(&log)
        .into_iter()
        .filter(|prompt| prompt["agent"] == "mock-model-alt")
        .filter_map(|prompt| {
            Some((
                prompt["pid"].as_i64()?,
                prompt["session"].as_str()?.to_owned(),
            ))
        })
        .collect();
    assert_eq!(children.len(), 3, "{children:?}");
    assert_eq!(
        children[0], children[1],
        "the follow-up reached another child"
    );
    assert_ne!(children[0].0, children[2].0, "a new task reused a child");
    session
        .handle
        .shutdown()
        .await
        .unwrap_or_else(|error| panic!("{error:#}"));
}

/// A child that needs permission asks in the parent conversation, under its own view, and the
/// answer reaches it; a request whose task is cancelled while it waits is withdrawn.
#[tokio::test]
#[serial]
async fn a_childs_permission_is_asked_through_the_parent() {
    let home = tempfile::tempdir().unwrap_or_else(|error| panic!("home: {error}"));
    let _env = Env::set(&[
        ("MOCK_AGENT_MCP_HTTP", "1".to_string()),
        (
            "MOCK_AGENT_MCP_CALLS_MOCK_MODEL",
            serde_json::json!([
                {"tool": "delegate_task", "arguments": {"task": "mock:request-permission", "agent": "mock-model-alt", "title": "Needs approval"}},
                {"tool": "delegate_task", "arguments": {"task": "mock:request-permission", "agent": "mock-model-alt", "title": "Cancelled while asking"}}
            ])
            .to_string(),
        ),
    ]);
    let config = NoriConfig {
        active_agent: "mock-model".to_string(),
        cwd: home.path().to_path_buf(),
        nori_home: home.path().to_path_buf(),
        approval_policy: nori_config::AskForApproval::OnRequest,
        ..Default::default()
    };
    let mut session = prepare_and_launch_session(
        AgentPrepareSpec {
            config: Arc::new(config),
            cli_version: "delegation-test".to_string(),
            session_context: None,
            initial_context: None,
        },
        SessionStart::New,
    );
    started(&mut session).await;
    prompt(&session, "DELEGATE with approvals").await;

    let is_child_request = |event: &SessionEvent| match event {
        SessionEvent::Acp(AcpEvent::Request {
            request: acp::AgentRequest::RequestPermissionRequest(permission),
            ..
        }) => permission.session_id.to_string().starts_with("nori-task-"),
        _ => false,
    };
    let events = collect_until(&mut session, Duration::from_secs(60), |events| {
        events
            .iter()
            .filter(|event| is_child_request(event))
            .count()
            >= 2
    })
    .await;
    let requests: Vec<acp::RequestId> = events
        .iter()
        .filter(|event| is_child_request(event))
        .filter_map(|event| match event {
            SessionEvent::Acp(AcpEvent::Request { request_id, .. }) => Some(request_id.clone()),
            _ => None,
        })
        .collect();

    session
        .handle
        .respond_to_agent(
            requests[0].clone(),
            Ok(acp::ClientResponse::RequestPermissionResponse(
                acp::RequestPermissionResponse::new(acp::RequestPermissionOutcome::Selected(
                    acp::SelectedPermissionOutcome::new("allow"),
                )),
            )),
        )
        .await
        .unwrap_or_else(|error| panic!("answer the child: {error:#}"));
    let answered = collect_until(&mut session, Duration::from_secs(60), |events| {
        child_text(events).contains("Permission granted with option: allow")
            && subagent_states(events).iter().any(|(_, state)| {
                matches!(
                    state,
                    SubagentState::Ended(SubagentOutcome::Completed { .. })
                )
            })
    })
    .await;
    assert!(!answered.is_empty());

    // Stopping the parent cancels the task still waiting for its answer, and withdraws it.
    session
        .handle
        .cancel()
        .await
        .unwrap_or_else(|error| panic!("cancel: {error:#}"));
    let events = collect_until(&mut session, Duration::from_secs(60), |events| {
        events.iter().any(|event| {
            matches!(event, SessionEvent::Nori(NoriEvent::RequestWithdrawn(id)) if *id == requests[1])
        })
    })
    .await;
    let states = subagent_states(&events);
    assert!(
        states
            .iter()
            .any(|(_, state)| matches!(state, SubagentState::Ended(SubagentOutcome::Cancelled))),
        "{states:?}"
    );
    session
        .handle
        .shutdown()
        .await
        .unwrap_or_else(|error| panic!("{error:#}"));
}
