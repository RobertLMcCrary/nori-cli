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
    orchestrator.inner.count_running_task();
    let mut state = orchestrator.inner.state.lock().await;
    state.next_task += 1;
    let number = state.next_task;
    let id = TaskId::numbered(number);
    state.tasks.push(TaskRecord {
        id: id.clone(),
        adoption_anchor: false,
        child: ChildKey(number),
        view_id: acp::SessionId::new(format!("view-{number}")),
        parent_view: None,
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
    id
}

fn completed() -> TaskEnd {
    TaskEnd::Completed {
        stop_reason: acp::StopReason::EndTurn,
    }
}

#[tokio::test]
async fn native_child_fork_announcements_keep_the_selected_parent() {
    let (events, mut event_rx) = mpsc::channel(16);
    let seed = Seed::root(
        Arc::new(crate::config::NoriConfig::default()),
        "test".to_owned(),
    );
    let (orchestrator, _signals) = Orchestrator::new(seed, events);
    orchestrator.bind(Binding {
        session_id: acp::SessionId::new("root"),
        approval_policy: watch::channel(AskForApproval::Never).1,
    });
    let id = add_task(&orchestrator, Wake::Never).await;
    orchestrator.inner.state.lock().await.tasks[0].parent_view =
        Some(acp::SessionId::new("native-child"));
    orchestrator.inner.announce_task(&id).await;
    orchestrator
        .inner
        .task_ended(&id, completed(), String::new())
        .await;
    for _ in 0..2 {
        let event = event_rx.recv().await.expect("fork announcement");
        let BackendEvent::Public(SessionEvent::Nori(NoriEvent::Subagent(SubagentWire::Update(
            update,
        )))) = event
        else {
            panic!("expected subagent announcement");
        };
        assert_eq!(
            update.parent_session_id,
            acp::SessionId::new("native-child")
        );
        assert_eq!(update.session_id, acp::SessionId::new("view-1"));
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
async fn rewind_admission_waits_for_running_and_undelivered_tasks() {
    let (orchestrator, _) = orchestrator(0);
    assert!(!orchestrator.has_pending_work().await);
    let task = add_task(&orchestrator, Wake::Always).await;
    assert!(orchestrator.has_pending_work().await);
    orchestrator
        .inner
        .task_ended(&task, completed(), "result".to_owned())
        .await;
    assert!(orchestrator.has_pending_work().await);
    assert!(orchestrator.take_wake().await.is_some());
    assert!(!orchestrator.has_pending_work().await);
    let fork = add_task(&orchestrator, Wake::Never).await;
    assert!(orchestrator.has_pending_work().await);
    orchestrator
        .inner
        .task_ended(&fork, completed(), String::new())
        .await;
    assert!(!orchestrator.has_pending_work().await);
}

#[tokio::test]
async fn delegation_waiting_on_rewind_cannot_start_in_the_new_generation() {
    let (orchestrator, _) = orchestrator(0);
    let admission = orchestrator.rewind_admission().await;
    let inner = Arc::clone(&orchestrator.inner);
    let pending = inner.delegate(launch::Delegation {
        task: "stale task".to_owned(),
        agent: None,
        model: None,
        title: None,
        role: None,
        isolation: launch::Isolation::Shared,
        mode: launch::Mode::Background,
        timeout: None,
        client_request_id: None,
        options: Vec::new(),
        fork_context: None,
        fork_source: None,
        fork_entries: Vec::new(),
        parent_view: None,
    });
    tokio::pin!(pending);
    assert!(futures::poll!(&mut pending).is_pending());
    orchestrator.advance_generation();
    drop(admission);
    assert!(
        pending
            .await
            .expect_err("stale delegation must fail")
            .contains("rewound")
    );
    assert!(!orchestrator.has_pending_work().await);
}

#[tokio::test]
async fn a_history_fork_finishes_without_waking_the_parent() {
    let (orchestrator, mut signals) = orchestrator(0);
    let id = add_task(&orchestrator, Wake::Never).await;
    orchestrator
        .inner
        .task_ended(&id, completed(), "fork result".to_owned())
        .await;
    assert!(signals.try_recv().is_err());
    assert!(orchestrator.take_wake().await.is_none());
    let state = orchestrator.inner.state.lock().await;
    assert_eq!(state.tasks[0].delivery, Delivery::Waived);
    assert_eq!(
        orchestrator
            .inner
            .binding
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .session_id,
        acp::SessionId::new("parent")
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
async fn task_results_are_not_truncated_by_default() {
    let (orchestrator, _signals) = orchestrator(0);
    let id = add_task(&orchestrator, Wake::IfUnwaited).await;
    let result = "é".repeat(100_000);
    orchestrator
        .inner
        .task_ended(&id, completed(), result.clone())
        .await;

    let snapshot = orchestrator
        .inner
        .wait(std::slice::from_ref(&id), Duration::ZERO)
        .await
        .expect("wait")
        .remove(0);

    assert_eq!(snapshot.result.as_deref(), Some(result.as_str()));
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
async fn delegation_has_no_default_depth_or_concurrency_cap() {
    let (orchestrator, _signals) = orchestrator(4);
    for _ in 0..9 {
        orchestrator.inner.count_running_task();
    }

    let capabilities = orchestrator.inner.capabilities();

    assert!(capabilities.can_delegate);
    assert_eq!(capabilities.depth, 4);
    assert_eq!(capabilities.running_tasks, 9);
}
