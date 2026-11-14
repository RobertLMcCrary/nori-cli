//! Invariants of the subagent tree under arbitrary interleavings, and the work a frame costs.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use nori_protocol::Patch;
use nori_protocol::SubagentMessage;
use nori_protocol::SubagentOutcome;
use nori_protocol::SubagentState;
use nori_protocol::SubagentUpdate;
use nori_protocol::acp::v1 as acp;
use pretty_assertions::assert_eq;
use proptest::prelude::*;

use super::ChildTarget;
use super::SubagentBlock;
use super::SubagentTree;

const SESSIONS: usize = 6;

fn session(index: usize) -> acp::SessionId {
    acp::SessionId::new(format!("c{index}"))
}

fn update(parent: acp::SessionId, child: usize, state: SubagentState) -> SubagentUpdate {
    SubagentUpdate {
        parent_session_id: parent,
        session_id: session(child),
        title: Patch::Set("general-purpose".into()),
        description: Patch::Set(format!("task {child}")),
        state: Patch::Set(state),
    }
}

fn text(delta: &str) -> acp::SessionUpdate {
    acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
        acp::TextContent::new(delta),
    )))
}

fn tool(id: &str) -> acp::SessionUpdate {
    acp::SessionUpdate::ToolCall(
        acp::ToolCall::new(id.to_string(), format!("Read {id}"))
            .kind(acp::ToolKind::Read)
            .raw_input(serde_json::json!({"path": format!("{id}.txt")})),
    )
}

#[derive(Debug, Clone)]
enum Op {
    /// Announce `child` under the root or under another child.
    Announce {
        child: usize,
        parent: Option<usize>,
    },
    Text {
        child: usize,
    },
    Tool {
        child: usize,
        call: usize,
    },
    End {
        child: usize,
        failed: bool,
    },
    /// A launcher tool call on the root, then the message that links it to `child`.
    Launch {
        child: usize,
    },
}

fn op() -> impl Strategy<Value = Op> {
    let child = 0..SESSIONS;
    prop_oneof![
        (child.clone(), proptest::option::of(0..SESSIONS))
            .prop_map(|(child, parent)| Op::Announce { child, parent }),
        child.clone().prop_map(|child| Op::Text { child }),
        (child.clone(), 0..4_usize).prop_map(|(child, call)| Op::Tool { child, call }),
        (child.clone(), any::<bool>()).prop_map(|(child, failed)| Op::End { child, failed }),
        child.prop_map(|child| Op::Launch { child }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Whatever order announcements, output and endings arrive in: every running child is shown
    /// exactly once, every child that ends is reported exactly once, and nothing ended runs again.
    #[test]
    fn any_interleaving_keeps_the_tree_consistent(ops in proptest::collection::vec(op(), 1..80)) {
        let base = Instant::now();
        let root = acp::SessionId::new("root");
        let mut tree = SubagentTree::new(PathBuf::from("/work"), false);
        tree.set_root(root.clone());
        let mut ended: BTreeSet<usize> = BTreeSet::new();
        let mut reported = 0;
        for (step, op) in ops.into_iter().enumerate() {
            let now = base + Duration::from_millis(u64::try_from(step).unwrap_or(0));
            match op {
                Op::Announce { child, parent } => {
                    let parent = parent.filter(|parent| *parent != child).map_or(root.clone(), session);
                    tree.on_update(update(parent, child, SubagentState::Running), now);
                }
                Op::Text { child } => {
                    tree.on_child_update(ChildTarget::Session(&session(child)), &text("more\n"), now);
                }
                Op::Tool { child, call } => {
                    tree.on_child_update(
                        ChildTarget::Session(&session(child)),
                        &tool(&format!("t{child}-{call}")),
                        now,
                    );
                }
                Op::End { child, failed } => {
                    let outcome = if failed {
                        SubagentOutcome::Failed { reason: None }
                    } else {
                        SubagentOutcome::Completed { stop_reason: None }
                    };
                    tree.on_update(update(root.clone(), child, SubagentState::Ended(outcome)), now);
                    ended.insert(child);
                }
                Op::Launch { child } => {
                    let call_id = format!("launch-{child}");
                    let call = acp::ToolCall::new(call_id.clone(), "Agent");
                    tree.on_launcher_update(&acp::SessionUpdate::ToolCall(call), true, now);
                    tree.on_message(
                        SubagentMessage {
                            session_id: session(child),
                            message_id: Some(call_id),
                            sender_session_id: Some(root.clone()),
                            recipient_session_id: Some(session(child)),
                            text: "go".into(),
                            chunk: false,
                        },
                        now,
                    );
                }
            }
            reported += tree.take_finished().len();
            let shown = tree.display_order(usize::MAX);
            prop_assert_eq!(shown.len(), tree.running_count());
            let mut seen = BTreeSet::new();
            for (node, _depth) in &shown {
                prop_assert!(seen.insert(node as *const _ as usize), "a node is shown twice");
                if let Some(id) = &node.session_id {
                    let index: usize = id.0.trim_start_matches('c').parse().unwrap_or(usize::MAX);
                    prop_assert!(!ended.contains(&index), "an ended child is running again");
                }
            }
        }
        // Each child that ended was reported exactly once.
        prop_assert_eq!(reported, ended.len());
        let running = tree.running_count();
        tree.on_root_turn_end(true, base + Duration::from_secs(60));
        prop_assert_eq!(tree.running_count(), 0);
        prop_assert_eq!(tree.take_finished().len(), running);
    }
}

#[test]
fn a_finished_grandchild_is_nested_in_its_parent_and_reported_once() {
    let now = Instant::now();
    let root = acp::SessionId::new("root");
    let mut tree = SubagentTree::new(PathBuf::from("/work"), false);
    tree.set_root(root.clone());
    tree.on_update(update(root.clone(), 1, SubagentState::Running), now);
    tree.on_update(update(session(1), 2, SubagentState::Running), now);
    assert_eq!(
        tree.display_order(usize::MAX)
            .iter()
            .map(|(node, depth)| (node.label().to_owned(), *depth))
            .collect::<Vec<_>>(),
        vec![("task 1".to_owned(), 0), ("task 2".to_owned(), 1)]
    );
    let done = SubagentState::Ended(SubagentOutcome::Completed { stop_reason: None });
    tree.on_update(update(session(1), 2, done.clone()), now);
    let finished = tree.take_finished();
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].depth, 1);
    // A late update for the finished grandchild changes nothing.
    tree.on_child_update(ChildTarget::Session(&session(2)), &text("late"), now);
    tree.on_update(update(session(1), 2, SubagentState::Running), now);
    assert_eq!(tree.running_count(), 1);
    tree.on_update(update(root, 1, done), now);
    let parent = tree.take_finished();
    assert_eq!(parent.len(), 1);
    assert_eq!(
        parent[0].detail.len(),
        1,
        "the grandchild's summary is in its parent's detail"
    );
}

#[test]
fn the_final_message_is_what_the_child_said_after_its_last_tool() {
    let now = Instant::now();
    let root = acp::SessionId::new("root");
    let mut tree = SubagentTree::new(PathBuf::from("/work"), false);
    tree.set_root(root.clone());
    tree.on_update(update(root.clone(), 1, SubagentState::Running), now);
    let child = session(1);
    tree.on_child_update(ChildTarget::Session(&child), &text("Let me look."), now);
    tree.on_child_update(ChildTarget::Session(&child), &tool("t1"), now);
    tree.on_child_update(ChildTarget::Session(&child), &text("The file holds "), now);
    tree.on_child_update(ChildTarget::Session(&child), &text("alpha."), now);
    tree.on_update(
        update(
            root,
            1,
            SubagentState::Ended(SubagentOutcome::Completed { stop_reason: None }),
        ),
        now,
    );
    let finished = tree.take_finished();
    assert_eq!(finished[0].message, "The file holds alpha.");
    assert_eq!(finished[0].tool_count, 1);
    assert_eq!(finished[0].detail.len(), 3);
}

/// Sixteen children streaming a thousand updates each cost a frame only the rows of the children
/// that changed since the previous frame, never work proportional to the updates.
#[test]
fn a_frame_costs_the_rows_that_changed_not_the_updates_that_arrived() {
    let base = Instant::now();
    let root = acp::SessionId::new("root");
    let mut tree = SubagentTree::new(PathBuf::from("/work"), false);
    tree.set_root(root.clone());
    for child in 0..16 {
        tree.on_update(update(root.clone(), child, SubagentState::Running), base);
    }
    let frame = |tree: &SubagentTree| {
        SubagentBlock {
            tree,
            now: base,
            screen_rows: 0,
        }
        .lines(80)
        .len()
    };
    assert_eq!(frame(&tree), 32);
    let after_first_frame = tree.rows_built();
    assert_eq!(after_first_frame, 16);
    for round in 0..1000 {
        for child in 0..16 {
            let delta = if round % 10 == 0 { "line\n" } else { "word " };
            tree.on_child_update(ChildTarget::Session(&session(child)), &text(delta), base);
        }
    }
    let _ = frame(&tree);
    assert_eq!(tree.rows_built(), after_first_frame + 16);
    let _ = frame(&tree);
    assert_eq!(tree.rows_built(), after_first_frame + 16);
}
