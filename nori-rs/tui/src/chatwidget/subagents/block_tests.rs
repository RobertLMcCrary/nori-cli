use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use nori_protocol::Patch;
use nori_protocol::SubagentOutcome;
use nori_protocol::SubagentState;
use nori_protocol::SubagentUpdate;
use nori_protocol::acp::v1 as acp;
use pretty_assertions::assert_eq;

use super::ChildTarget;
use super::SubagentBlock;
use super::SubagentTree;

fn announce(
    tree: &mut SubagentTree,
    parent: &str,
    child: &str,
    title: &str,
    description: &str,
    now: Instant,
) {
    tree.on_update(
        SubagentUpdate {
            parent_session_id: acp::SessionId::new(parent),
            session_id: acp::SessionId::new(child),
            title: Patch::Set(title.into()),
            description: Patch::Set(description.into()),
            state: Patch::Set(SubagentState::Running),
        },
        now,
    );
}

fn tool_call(tree: &mut SubagentTree, child: &str, id: &str, title: &str, now: Instant) {
    let call = acp::ToolCall::new(id.to_string(), title.to_string())
        .kind(acp::ToolKind::Read)
        .raw_input(serde_json::json!({"path": "a.txt"}));
    tree.on_child_update(
        ChildTarget::Session(&acp::SessionId::new(child)),
        &acp::SessionUpdate::ToolCall(call),
        now,
    );
}

fn text(tree: &mut SubagentTree, child: &str, delta: &str, now: Instant) {
    let chunk = acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(delta)));
    tree.on_child_update(
        ChildTarget::Session(&acp::SessionId::new(child)),
        &acp::SessionUpdate::AgentMessageChunk(chunk),
        now,
    );
}

fn render(tree: &SubagentTree, now: Instant, width: u16, screen_rows: u16) -> String {
    SubagentBlock {
        tree,
        now,
        screen_rows,
    }
    .lines(width)
    .iter()
    .map(|line| {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
            .trim_end()
            .to_owned()
    })
    .collect::<Vec<_>>()
    .join("\n")
}

fn three_children() -> (SubagentTree, Instant) {
    let base = Instant::now();
    let mut tree = SubagentTree::new(PathBuf::from("/work"), false);
    tree.set_root(acp::SessionId::new("root"));
    announce(
        &mut tree,
        "root",
        "c1",
        "general-purpose",
        "Read a.txt contents",
        base,
    );
    announce(
        &mut tree,
        "root",
        "c2",
        "Explore",
        "Find every caller of `parse_config` in the repository and summarize how each handles errors",
        base,
    );
    announce(
        &mut tree,
        "c2",
        "c3",
        "general-purpose",
        "Open the matching file",
        base,
    );
    tool_call(&mut tree, "c1", "t1", "Read a.txt", base);
    tool_call(&mut tree, "c1", "t2", "Read b.txt", base);
    text(
        &mut tree,
        "c2",
        "Looking at the callers now.\nFound three so far",
        base,
    );
    (tree, base + Duration::from_secs(7))
}

#[test]
fn live_block_at_width_80() {
    let (tree, now) = three_children();
    insta::assert_snapshot!("subagent_live_block_80", render(&tree, now, 80, 0));
}

#[test]
fn live_block_at_width_20() {
    let (tree, now) = three_children();
    insta::assert_snapshot!("subagent_live_block_20", render(&tree, now, 20, 0));
}

#[test]
fn live_block_at_width_240() {
    let (tree, now) = three_children();
    insta::assert_snapshot!("subagent_live_block_240", render(&tree, now, 240, 0));
}

#[test]
fn more_children_than_fit_are_counted() {
    let base = Instant::now();
    let mut tree = SubagentTree::new(PathBuf::from("/work"), false);
    tree.set_root(acp::SessionId::new("root"));
    for index in 0..12 {
        announce(
            &mut tree,
            "root",
            &format!("c{index}"),
            "general-purpose",
            &format!("Task number {index}"),
            base,
        );
        tool_call(&mut tree, &format!("c{index}"), "t", "Read file", base);
    }
    let shown = render(&tree, base + Duration::from_secs(65), 60, 30);
    insta::assert_snapshot!("subagent_live_block_overflow", shown);
    // A third of 30 rows.
    assert!(shown.lines().count() <= 10);
}

#[test]
fn a_finished_child_leaves_the_block_and_its_parent_stays() {
    let (mut tree, now) = three_children();
    tree.on_update(
        SubagentUpdate {
            parent_session_id: acp::SessionId::new("c2"),
            session_id: acp::SessionId::new("c3"),
            title: Patch::Keep,
            description: Patch::Keep,
            state: Patch::Set(SubagentState::Ended(SubagentOutcome::Completed {
                stop_reason: None,
            })),
        },
        now,
    );
    assert_eq!(tree.running_count(), 2);
    let finished = tree.take_finished();
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].label, "Open the matching file");
    assert_eq!(finished[0].depth, 1);
}

#[test]
fn only_changed_children_rebuild_their_rows() {
    let base = Instant::now();
    let mut tree = SubagentTree::new(PathBuf::from("/work"), false);
    tree.set_root(acp::SessionId::new("root"));
    for index in 0..16 {
        announce(
            &mut tree,
            "root",
            &format!("c{index}"),
            "t",
            &format!("task {index}"),
            base,
        );
    }
    let now = base + Duration::from_secs(1);
    let _ = render(&tree, now, 80, 0);
    let built = tree.rows_built();
    assert_eq!(built, 16);
    // The same frame again, and again: nothing is rebuilt.
    let _ = render(&tree, now, 80, 0);
    let _ = render(&tree, now, 80, 0);
    assert_eq!(tree.rows_built(), built);
    // One child gets an update: only its rows are rebuilt.
    text(&mut tree, "c7", "working", base);
    let _ = render(&tree, now, 80, 0);
    assert_eq!(tree.rows_built(), built + 1);
}
