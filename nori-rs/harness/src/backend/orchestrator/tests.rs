use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use pretty_assertions::assert_eq;
use tokio::sync::mpsc;

use super::*;

fn orchestrator(depth: i64) -> (Orchestrator, mpsc::UnboundedReceiver<Signal>) {
    let (events, _events_rx) = mpsc::channel(64);
    let root = Seed::root(
        Arc::new(crate::config::NoriConfig::default()),
        "test".to_owned(),
    );
    let seed = Seed { depth, ..root };
    let (orchestrator, signals) = Orchestrator::new(seed, events);
    orchestrator.bind(Binding {
        session_id: acp::SessionId::new("parent"),
        approval_policy: watch::channel(AskForApproval::Never).1,
    });
    (orchestrator, signals)
}

/// A task as it stands after starting, without a child behind it.
async fn add_task(orchestrator: &Orchestrator, wake: Wake) -> TaskId {
    let mut state = orchestrator.inner.state.lock().await;
    state.next_task += 1;
    let number = state.next_task;
    let id = TaskId::numbered(number);
    state.tasks.push(TaskRecord {
        id: id.clone(),
        child: ChildKey(number),
        view_id: acp::SessionId::new(format!("view-{number}")),
        agent: "codex".to_owned(),
        agent_name: "Codex".to_owned(),
        model: None,
        title: "Read a.txt".to_owned(),
        status: TaskStatus::Running,
        result: String::new(),
        delivery: Delivery::Owed,
        wake,
        awaiting_approval: false,
        relayed_approvals: Vec::new(),
        started_at: Instant::now(),
        ended_at: None,
        worktree: None,
        progress: Default::default(),
    });
    orchestrator
        .inner
        .seed
        .tree
        .running
        .fetch_add(1, Ordering::AcqRel);
    id
}

fn completed() -> TaskEnd {
    TaskEnd::Completed {
        stop_reason: acp::StopReason::EndTurn,
    }
}

#[tokio::test]
async fn a_finished_task_wakes_the_parent_once() {
    let (orchestrator, mut signals) = orchestrator(0);
    let id = add_task(&orchestrator, Wake::Always).await;
    orchestrator
        .inner
        .task_ended(&id, completed(), "alpha".to_owned())
        .await;
    assert!(matches!(signals.try_recv(), Ok(Signal::ResultsOwed)));
    let wake = orchestrator.take_wake().await.expect("a wake is owed");
    assert!(wake.text.contains("alpha"));
    assert_eq!(wake.delivered.tasks[0].task_id, "task-1");
    assert!(orchestrator.take_wake().await.is_none());
    assert_eq!(
        orchestrator.inner.seed.tree.running.load(Ordering::Acquire),
        0
    );
}

#[tokio::test]
async fn reading_a_finished_task_delivers_it_and_cancelling_waives_it() {
    let (orchestrator, _signals) = orchestrator(0);
    let read = add_task(&orchestrator, Wake::Always).await;
    let cancelled = add_task(&orchestrator, Wake::Always).await;
    orchestrator
        .inner
        .task_ended(&read, completed(), "alpha".to_owned())
        .await;
    let snapshot = orchestrator
        .inner
        .status(&read, true)
        .await
        .expect("status");
    assert_eq!(snapshot.result.as_deref(), Some("alpha"));
    let snapshot = orchestrator.inner.cancel(&cancelled).await.expect("cancel");
    assert_eq!(snapshot.status, "cancelled");
    assert!(orchestrator.take_wake().await.is_none());
}

#[tokio::test]
async fn a_wait_that_gives_up_leaves_the_task_to_wake_the_parent() {
    let (orchestrator, _signals) = orchestrator(0);
    let id = add_task(&orchestrator, Wake::IfUnwaited).await;
    let snapshots = orchestrator
        .inner
        .wait(std::slice::from_ref(&id), Duration::from_millis(20))
        .await
        .expect("wait");
    assert_eq!(snapshots[0].status, "running");
    assert_eq!(snapshots[0].next, Some(wake::STILL_RUNNING));
    orchestrator
        .inner
        .task_ended(&id, completed(), "late".to_owned())
        .await;
    assert!(orchestrator.take_wake().await.is_some());
}

#[tokio::test]
async fn a_wait_that_returns_the_result_delivers_it() {
    let (orchestrator, _signals) = orchestrator(0);
    let id = add_task(&orchestrator, Wake::IfUnwaited).await;
    let waiter = {
        let orchestrator = orchestrator.clone();
        let id = id.clone();
        tokio::spawn(async move {
            orchestrator
                .inner
                .wait(std::slice::from_ref(&id), Duration::from_secs(5))
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    orchestrator
        .inner
        .task_ended(&id, completed(), "alpha".to_owned())
        .await;
    let snapshots = waiter.await.expect("waiter").expect("wait");
    assert_eq!(snapshots[0].result.as_deref(), Some("alpha"));
    assert!(orchestrator.take_wake().await.is_none());
}

#[tokio::test]
async fn a_session_at_the_depth_limit_cannot_delegate() {
    let (orchestrator, _signals) = orchestrator(MAX_DEPTH);
    assert!(!orchestrator.inner.capabilities().can_delegate);
    let refused = orchestrator
        .inner
        .delegate(launch::Delegation {
            task: "anything".to_owned(),
            agent: None,
            model: None,
            title: None,
            role: None,
            isolation: launch::Isolation::Shared,
            mode: launch::Mode::Background,
            timeout: None,
            client_request_id: None,
            options: Vec::new(),
        })
        .await;
    assert!(refused.is_err_and(|error| error.contains("levels deep")));
}
