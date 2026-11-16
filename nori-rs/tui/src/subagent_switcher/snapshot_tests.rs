//! The switcher and the focused view drawn from a real subagent tree: running, finished and
//! failed children, one nested in another.

use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use crossterm::event::KeyCode;
use crossterm::event::KeyModifiers;
use nori_protocol::Patch;
use nori_protocol::SubagentOutcome;
use nori_protocol::SubagentState;
use nori_protocol::SubagentUpdate;
use nori_protocol::acp::v1 as acp;
use pretty_assertions::assert_eq;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::Line;

use super::Body;
use super::SubagentOverlay;
use super::Target;
use super::Transition;
use super::roster::Roster;
use super::roster::Status;
use super::roster::SubagentKey;
use super::tests::ctrl_space;
use super::tests::main_entry;
use super::tests::press;
use crate::chatwidget::subagents::ChildTarget;
use crate::chatwidget::subagents::SubagentTree;

fn child(index: usize) -> acp::SessionId {
    acp::SessionId::new(format!("c{index}"))
}

fn announce(
    tree: &mut SubagentTree,
    parent: Option<usize>,
    index: usize,
    kind: &str,
    task: &str,
    now: Instant,
) {
    tree.on_update(
        SubagentUpdate {
            parent_session_id: parent.map_or_else(|| acp::SessionId::new("root"), child),
            session_id: child(index),
            title: Patch::Set(kind.into()),
            description: Patch::Set(task.into()),
            state: Patch::Set(SubagentState::Running),
        },
        now,
    );
}

fn end(tree: &mut SubagentTree, index: usize, outcome: SubagentOutcome, now: Instant) {
    tree.on_update(
        SubagentUpdate {
            parent_session_id: acp::SessionId::new("root"),
            session_id: child(index),
            title: Patch::Keep,
            description: Patch::Keep,
            state: Patch::Set(SubagentState::Ended(outcome)),
        },
        now,
    );
}

fn read(tree: &mut SubagentTree, index: usize, id: &str, path: &str, now: Instant) {
    let call = acp::ToolCall::new(id.to_string(), format!("Read {path}"))
        .kind(acp::ToolKind::Read)
        .status(acp::ToolCallStatus::Completed)
        .raw_input(serde_json::json!({ "path": path }));
    tree.on_child_update(
        ChildTarget::Session(&child(index)),
        &acp::SessionUpdate::ToolCall(call),
        now,
    );
}

fn say(tree: &mut SubagentTree, index: usize, delta: &str, now: Instant) {
    let chunk = acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(delta)));
    tree.on_child_update(
        ChildTarget::Session(&child(index)),
        &acp::SessionUpdate::AgentMessageChunk(chunk),
        now,
    );
}

/// Five children: two running (one inside the other), one finished, one failed, one cancelled.
fn sample_tree(base: Instant) -> SubagentTree {
    let at = |seconds| base + Duration::from_secs(seconds);
    let mut tree = SubagentTree::new(PathBuf::from("/work"), false);
    tree.set_root(acp::SessionId::new("root"));
    announce(
        &mut tree,
        None,
        1,
        "general-purpose",
        "Explore the repo structure",
        at(0),
    );
    say(&mut tree, 1, "Let me look around the repository.\n", at(1));
    read(&mut tree, 1, "t1", "src/main.rs", at(2));
    say(
        &mut tree,
        1,
        "The entry point is **src/main.rs**; it wires up the TUI.",
        at(3),
    );
    announce(
        &mut tree,
        Some(1),
        2,
        "security-reviewer",
        "Audit the auth middleware",
        at(4),
    );
    read(&mut tree, 2, "t2", "src/auth.rs", at(5));
    say(&mut tree, 2, "Checking session expiry handling…", at(6));
    announce(
        &mut tree,
        None,
        3,
        "general-purpose",
        "Read a.txt contents",
        at(7),
    );
    read(&mut tree, 3, "t3", "a.txt", at(8));
    say(&mut tree, 3, "The file holds alpha.", at(9));
    end(
        &mut tree,
        3,
        SubagentOutcome::Completed { stop_reason: None },
        at(34),
    );
    announce(
        &mut tree,
        None,
        4,
        "general-purpose",
        "Run the test suite",
        at(10),
    );
    read(&mut tree, 4, "t4", "Cargo.toml", at(11));
    end(
        &mut tree,
        4,
        SubagentOutcome::Failed {
            reason: Some("usage limit".into()),
        },
        at(95),
    );
    announce(&mut tree, None, 5, "Explore", "Summarize the docs", at(12));
    end(&mut tree, 5, SubagentOutcome::Cancelled, at(100));
    tree
}

fn roster_from(tree: &SubagentTree, now: Instant) -> Roster {
    let (subagents, replaced) = tree.entries(now);
    Roster {
        main: main_entry(now),
        subagents,
        replaced,
        animations: false,
        tick: 0,
    }
}

/// What the app does before a frame: a fresh roster, and the body the overlay asks for.
fn feed(overlay: &mut SubagentOverlay, tree: &SubagentTree, area: Rect, now: Instant) {
    let roster = roster_from(tree, now);
    overlay.update(&roster, area);
    let Some(query) = overlay.body_query(area, now) else {
        return;
    };
    let body = match query.target {
        Target::Main => Some(Body {
            query,
            revision: 0,
            lines: vec![
                Line::from("› what changed in the auth code?"),
                Line::default(),
                Line::from("I will have a subagent audit it."),
            ],
            built_at: now,
        }),
        Target::Subagent(subagent) => {
            tree.body(subagent, query.width, query.rows)
                .map(|(revision, lines)| Body {
                    query,
                    revision,
                    lines,
                    built_at: now,
                })
        }
    };
    if let Some(body) = body {
        overlay.set_body(&roster, body, area);
    }
}

fn draw(
    overlay: &mut SubagentOverlay,
    tree: &SubagentTree,
    width: u16,
    height: u16,
    now: Instant,
) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    feed(overlay, tree, Rect::new(0, 0, width, height), now);
    terminal
        .draw(|frame| overlay.render(frame.area(), frame.buffer_mut()))
        .expect("draw");
    terminal
}

fn open(tree: &SubagentTree, now: Instant) -> SubagentOverlay {
    SubagentOverlay::switcher(&roster_from(tree, now), ctrl_space())
}

fn type_text(overlay: &mut SubagentOverlay, text: &str) {
    for ch in text.chars() {
        overlay.on_key(press(KeyCode::Char(ch), KeyModifiers::NONE));
    }
}

fn down(overlay: &mut SubagentOverlay, times: usize) {
    for _ in 0..times {
        overlay.on_key(press(KeyCode::Down, KeyModifiers::NONE));
    }
}

#[test]
fn wide_terminals_show_the_list_and_a_preview_of_the_selected_subagent() {
    let base = Instant::now();
    let tree = sample_tree(base);
    let now = base + Duration::from_secs(160);
    let mut overlay = open(&tree, now);
    down(&mut overlay, 1);
    let terminal = draw(&mut overlay, &tree, 120, 26, now);
    insta::assert_snapshot!("switcher_wide_running_nested", terminal.backend());
}

#[test]
fn the_preview_of_a_failed_subagent_says_why() {
    let base = Instant::now();
    let tree = sample_tree(base);
    let now = base + Duration::from_secs(160);
    let mut overlay = open(&tree, now);
    down(&mut overlay, 4);
    let terminal = draw(&mut overlay, &tree, 120, 22, now);
    insta::assert_snapshot!("switcher_wide_failed", terminal.backend());
}

#[test]
fn narrow_terminals_give_the_list_the_whole_width() {
    let base = Instant::now();
    let tree = sample_tree(base);
    let now = base + Duration::from_secs(160);
    let mut overlay = open(&tree, now);
    down(&mut overlay, 2);
    let terminal = draw(&mut overlay, &tree, 72, 16, now);
    insta::assert_snapshot!("switcher_narrow", terminal.backend());
}

#[test]
fn a_query_narrows_the_list_and_bolds_the_matched_characters() {
    let base = Instant::now();
    let tree = sample_tree(base);
    let now = base + Duration::from_secs(160);
    let mut overlay = open(&tree, now);
    type_text(&mut overlay, "rea");
    let terminal = draw(&mut overlay, &tree, 120, 18, now);
    insta::assert_snapshot!("switcher_wide_query", terminal.backend());

    // "Read a.txt contents" is the first match: its first three characters are bold.
    let buffer = terminal.backend().buffer();
    let row = (0..buffer.area.height)
        .find(|y| {
            let text: String = (0..54)
                .map(|x| buffer[(x, *y)].symbol().to_owned())
                .collect();
            text.contains("Read a.txt")
        })
        .expect("the matching row");
    let start = (0..54)
        .find(|x| buffer[(*x, row)].symbol() == "R")
        .expect("start of the title");
    for offset in 0..3 {
        assert!(
            buffer[(start + offset, row)]
                .modifier
                .contains(Modifier::BOLD),
            "column {offset} of the match is bold"
        );
    }
    assert!(!buffer[(start + 3, row)].modifier.contains(Modifier::BOLD));
}

#[test]
fn with_no_subagents_the_switcher_still_opens_on_main() {
    let base = Instant::now();
    let mut tree = SubagentTree::new(PathBuf::from("/work"), false);
    tree.set_root(acp::SessionId::new("root"));
    let mut overlay = open(&tree, base);
    let terminal = draw(&mut overlay, &tree, 110, 12, base);
    insta::assert_snapshot!("switcher_empty_wide", terminal.backend());
    let terminal = draw(&mut overlay, &tree, 60, 8, base);
    insta::assert_snapshot!("switcher_empty_narrow", terminal.backend());
    assert_eq!(
        overlay.take_transition(),
        None,
        "nothing happened until a key is pressed"
    );
    overlay.on_key(press(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(overlay.take_transition(), Some(Transition::Close));
}

#[test]
fn a_running_subagent_is_followed_live_and_its_end_is_shown_in_place() {
    let base = Instant::now();
    let mut tree = sample_tree(base);
    let now = base + Duration::from_secs(160);
    let roster = roster_from(&tree, now);
    let mut overlay = SubagentOverlay::focused(&roster, SubagentKey(2), ctrl_space(), 100)
        .expect("subagent 2 exists");
    let area = Rect::new(0, 0, 100, 18);
    let terminal = draw(&mut overlay, &tree, 100, 18, now);
    insta::assert_snapshot!("focused_running", terminal.backend());

    // More output arrives: the view follows it without being asked.
    say(
        &mut tree,
        2,
        "\n\nFound a missing expiry check in `refresh()`.",
        now,
    );
    for line in 0..30 {
        say(&mut tree, 2, &format!("\nnote {line}"), now);
    }
    feed(&mut overlay, &tree, area, now);
    let mut terminal = Terminal::new(TestBackend::new(100, 18)).expect("terminal");
    terminal
        .draw(|frame| overlay.render(frame.area(), frame.buffer_mut()))
        .expect("draw");
    terminal
        .draw(|frame| overlay.render(frame.area(), frame.buffer_mut()))
        .expect("draw");
    insta::assert_snapshot!("focused_following_new_output", terminal.backend());

    // It finishes while being watched: the view stays and shows the final state.
    end(
        &mut tree,
        2,
        SubagentOutcome::Completed { stop_reason: None },
        now + Duration::from_secs(5),
    );
    let later = now + Duration::from_secs(8);
    let terminal = draw(&mut overlay, &tree, 100, 18, later);
    insta::assert_snapshot!("focused_finished_while_watching", terminal.backend());
    assert_eq!(
        roster_from(&tree, later)
            .entry(Target::Subagent(SubagentKey(2)))
            .map(|e| e.status),
        Some(Status::Completed)
    );
}

#[test]
fn the_focused_view_goes_back_and_to_the_list() {
    let base = Instant::now();
    let tree = sample_tree(base);
    let roster = roster_from(&tree, base);
    let mut overlay =
        SubagentOverlay::focused(&roster, SubagentKey(3), ctrl_space(), 100).expect("subagent 3");
    overlay.on_key(press(KeyCode::Char(' '), KeyModifiers::CONTROL));
    assert_eq!(overlay.take_transition(), Some(Transition::OpenSwitcher));
    overlay.on_key(press(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(overlay.take_transition(), Some(Transition::Close));
    assert!(SubagentOverlay::focused(&roster, SubagentKey(99), ctrl_space(), 100).is_none());
}

#[test]
fn a_failed_subagent_view_names_the_failure() {
    let base = Instant::now();
    let tree = sample_tree(base);
    let now = base + Duration::from_secs(160);
    let roster = roster_from(&tree, now);
    let mut overlay =
        SubagentOverlay::focused(&roster, SubagentKey(4), ctrl_space(), 90).expect("subagent 4");
    let terminal = draw(&mut overlay, &tree, 90, 14, now);
    insta::assert_snapshot!("focused_failed", terminal.backend());
}
