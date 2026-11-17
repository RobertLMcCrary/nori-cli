//! Filtering, ranking, selection and key handling, from rosters written out as literals.

use std::time::Duration;
use std::time::Instant;

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use nori_config::HotkeyBinding;
use pretty_assertions::assert_eq;
use ratatui::layout::Rect;

use super::Transition;
use super::list::ListState;
use super::roster::Body;
use super::roster::BodyQuery;
use super::roster::Entry;
use super::roster::Roster;
use super::roster::Status;
use super::roster::SubagentKey;
use super::roster::Target;
use super::switcher::Switcher;
use super::switcher::layout;

pub(super) fn key(id: i64) -> Target {
    Target::Subagent(SubagentKey(id))
}

pub(super) fn entry(id: i64, title: &str, status: Status, base: Instant) -> Entry {
    Entry {
        target: key(id),
        title: title.to_owned(),
        parent: None,
        agent: Some("general-purpose".to_owned()),
        model: None,
        status,
        detail: None,
        elapsed: Duration::from_secs(30),
        ended_ago: (!status.is_running()).then(|| Duration::from_secs(120)),
        tool_count: 2,
        revision: 1,
        active_at: base,
    }
}

pub(super) fn main_entry(base: Instant) -> Entry {
    Entry {
        target: Target::Main,
        title: "main".to_owned(),
        parent: None,
        agent: Some("Claude Code".to_owned()),
        model: Some("opus".to_owned()),
        status: Status::Idle,
        detail: None,
        elapsed: Duration::ZERO,
        ended_ago: None,
        tool_count: 0,
        revision: 0,
        active_at: base,
    }
}

pub(super) fn roster(subagents: Vec<Entry>, base: Instant) -> Roster {
    Roster {
        main: main_entry(base),
        subagents,
        replaced: Vec::new(),
        animations: false,
        tick: 0,
    }
}

pub(super) fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}

pub(super) fn ctrl_space() -> HotkeyBinding {
    HotkeyBinding::from_str("ctrl+space")
}

fn titles(list: &ListState) -> Vec<String> {
    list.rows()
        .iter()
        .map(|row| row.entry.title.clone())
        .collect()
}

fn five(base: Instant) -> Vec<Entry> {
    let ago = |seconds| base - Duration::from_secs(seconds);
    let mut audit = entry(2, "Audit the auth middleware", Status::Running, ago(5));
    audit.parent = Some("Explore the repo".to_owned());
    let mut explore = entry(1, "Explore the repo", Status::Running, ago(20));
    explore.agent = Some("Explore".to_owned());
    let mut done_old = entry(3, "Read a.txt contents", Status::Completed, ago(100));
    done_old.ended_ago = Some(Duration::from_secs(600));
    let mut done_new = entry(4, "Summarize the docs", Status::Completed, ago(90));
    done_new.ended_ago = Some(Duration::from_secs(60));
    let mut failed = entry(5, "Run the test suite", Status::Failed, ago(80));
    failed.ended_ago = Some(Duration::from_secs(300));
    vec![done_old, explore, failed, audit, done_new]
}

#[test]
fn natural_order_is_main_then_running_by_activity_then_ended_by_recency() {
    let base = Instant::now();
    let list = ListState::new(&roster(five(base), base));
    assert_eq!(
        titles(&list),
        vec![
            "main",
            "Audit the auth middleware",
            "Explore the repo",
            "Summarize the docs",
            "Run the test suite",
            "Read a.txt contents",
        ]
    );
}

#[test]
fn a_query_filters_and_marks_the_matched_characters() {
    let base = Instant::now();
    let mut list = ListState::new(&roster(five(base), base));
    for ch in "audi".chars() {
        list.push_char(ch);
    }
    assert_eq!(titles(&list), vec!["Audit the auth middleware"]);
    assert_eq!(list.rows()[0].highlights.title, vec![0, 1, 2, 3]);
    assert_eq!(list.selected(), key(2));
}

#[test]
fn matches_in_the_title_outrank_matches_in_the_agent() {
    let base = Instant::now();
    let mut explore = entry(1, "Look around", Status::Running, base);
    explore.agent = Some("Explore".to_owned());
    let other = entry(2, "Explore the repo", Status::Running, base);
    let mut list = ListState::new(&roster(vec![explore, other], base));
    for ch in "explore".chars() {
        list.push_char(ch);
    }
    assert_eq!(titles(&list), vec!["Explore the repo", "Look around"]);
    assert_eq!(list.rows()[1].highlights.agent, vec![0, 1, 2, 3, 4, 5, 6]);
}

#[test]
fn every_word_of_the_query_must_match_somewhere() {
    let base = Instant::now();
    let mut list = ListState::new(&roster(five(base), base));
    for ch in "auth running".chars() {
        list.push_char(ch);
    }
    assert_eq!(titles(&list), vec!["Audit the auth middleware"]);
    for ch in " zzz".chars() {
        list.push_char(ch);
    }
    assert_eq!(titles(&list), Vec::<String>::new());
}

#[test]
fn the_status_is_searchable() {
    let base = Instant::now();
    let mut list = ListState::new(&roster(five(base), base));
    for ch in "failed".chars() {
        list.push_char(ch);
    }
    assert_eq!(titles(&list), vec!["Run the test suite"]);
}

#[test]
fn the_selection_stays_on_its_row_when_rows_appear_above_it() {
    let base = Instant::now();
    let mut list = ListState::new(&roster(five(base), base));
    list.step(2);
    assert_eq!(list.selected(), key(1));
    let mut more = five(base);
    more.push(entry(
        9,
        "Brand new",
        Status::Running,
        base + Duration::from_secs(1),
    ));
    list.apply(&roster(more, base));
    assert_eq!(titles(&list)[1], "Brand new");
    assert_eq!(list.selected(), key(1));
    assert_eq!(list.selected_index(), Some(3));
}

#[test]
fn activity_does_not_reorder_rows_under_the_cursor() {
    let base = Instant::now();
    let mut list = ListState::new(&roster(five(base), base));
    let before = titles(&list);
    let mut busier = five(base);
    for entry in &mut busier {
        if entry.target == key(1) {
            entry.active_at = base + Duration::from_secs(60);
            entry.tool_count = 40;
        }
    }
    list.apply(&roster(busier, base));
    assert_eq!(titles(&list), before);
    assert_eq!(list.rows()[2].entry.tool_count, 40);
}

#[test]
fn a_row_that_ends_moves_to_the_ended_rows_and_stays_selected() {
    let base = Instant::now();
    let mut list = ListState::new(&roster(five(base), base));
    list.step(1);
    assert_eq!(list.selected(), key(2));
    let mut ended = five(base);
    for entry in &mut ended {
        if entry.target == key(2) {
            entry.status = Status::Completed;
            entry.ended_ago = Some(Duration::from_secs(1));
        }
    }
    list.apply(&roster(ended, base));
    assert_eq!(list.selected(), key(2));
    assert_eq!(list.selected_index(), Some(2));
}

#[test]
fn the_selection_follows_a_row_that_was_replaced() {
    let base = Instant::now();
    let launcher = entry(1, "Explore the repo", Status::Running, base);
    let mut list = ListState::new(&roster(vec![launcher], base));
    list.step(1);
    assert_eq!(list.selected(), key(1));
    let mut next = roster(
        vec![entry(7, "Explore the repo", Status::Running, base)],
        base,
    );
    next.replaced = vec![(SubagentKey(1), SubagentKey(7))];
    list.apply(&next);
    assert_eq!(list.selected(), key(7));
}

#[test]
fn a_selected_row_that_vanishes_leaves_the_selection_at_the_same_place() {
    let base = Instant::now();
    let mut list = ListState::new(&roster(five(base), base));
    list.step(3);
    assert_eq!(list.selected(), key(4));
    let without: Vec<Entry> = five(base)
        .into_iter()
        .filter(|entry| entry.target != key(4))
        .collect();
    list.apply(&roster(without, base));
    assert_eq!(list.selected(), key(5));
}

#[test]
fn typing_selects_the_best_match_and_clearing_keeps_the_selection() {
    let base = Instant::now();
    let mut list = ListState::new(&roster(five(base), base));
    for ch in "run".chars() {
        list.push_char(ch);
    }
    assert_eq!(list.selected(), key(5));
    list.clear_query();
    assert_eq!(list.rows().len(), 6);
    assert_eq!(list.selected(), key(5));
}

#[test]
fn refreshing_times_does_not_search_again() {
    let base = Instant::now();
    let mut list = ListState::new(&roster(five(base), base));
    let mut later = five(base);
    for entry in &mut later {
        entry.elapsed += Duration::from_secs(5);
    }
    list.apply(&roster(later, base));
    assert_eq!(list.rows()[2].entry.elapsed, Duration::from_secs(35));
}

fn switcher_with_five() -> Switcher {
    let base = Instant::now();
    Switcher::new(&roster(five(base), base))
}

#[test]
fn switcher_pointer_taps_swipes_and_wheel_use_visible_targets() {
    use crossterm::event::MouseButton;
    use crossterm::event::MouseEvent;
    use crossterm::event::MouseEventKind;

    let mouse = |kind, row| MouseEvent {
        kind,
        column: 2,
        row,
        modifiers: KeyModifiers::NONE,
    };
    let down = MouseEventKind::Down(MouseButton::Left);
    let drag = MouseEventKind::Drag(MouseButton::Left);
    let up = MouseEventKind::Up(MouseButton::Left);
    let area = Rect::new(0, 0, 30, 4); // one visible conversation row
    let mut switcher = switcher_with_five();

    assert_eq!(switcher.on_mouse(mouse(down, 2), area), None);
    assert_eq!(
        switcher.on_mouse(mouse(up, 2), area),
        Some(Transition::FocusMain)
    );

    assert_eq!(switcher.on_mouse(mouse(down, 2), area), None);
    assert_eq!(switcher.on_mouse(mouse(drag, 1), area), None);
    assert_eq!(switcher.list.scroll_top, 1);
    assert_eq!(switcher.on_mouse(mouse(up, 2), area), None);

    assert_eq!(switcher.on_mouse(mouse(down, 2), area), None);
    switcher.list.scroll_top = 2; // a roster or viewport update changed the visible target
    assert_eq!(switcher.on_mouse(mouse(up, 2), area), None);
    assert_eq!(switcher.on_mouse(mouse(down, 2), area), None);
    assert_eq!(switcher.on_mouse(mouse(up, 3), area), None);

    let before = switcher.list.selected();
    assert_eq!(
        switcher.on_mouse(mouse(MouseEventKind::ScrollDown, 2), area),
        None
    );
    assert_ne!(switcher.list.selected(), before);

    let tall = Rect::new(0, 0, 30, 20);
    assert_eq!(switcher.on_mouse(mouse(down, 2), tall), None);
    assert_eq!(switcher.on_mouse(mouse(up, 3), tall), None);
    assert_eq!(switcher.on_mouse(mouse(down, 18), tall), None);
    assert_eq!(switcher.on_mouse(mouse(up, 18), tall), None);
}

#[test]
fn the_toggle_and_escape_close_and_enter_picks() {
    let mut switcher = switcher_with_five();
    let toggle = ctrl_space();
    for event in [
        press(KeyCode::Char(' '), KeyModifiers::CONTROL),
        press(KeyCode::Null, KeyModifiers::NONE),
        press(KeyCode::Char('@'), KeyModifiers::CONTROL),
        press(KeyCode::Esc, KeyModifiers::NONE),
    ] {
        assert_eq!(
            switcher.on_key(event, &toggle),
            Some(Transition::Close),
            "{event:?}"
        );
    }
    // The main row selects the main conversation; a subagent opens its view.
    assert_eq!(
        switcher.on_key(press(KeyCode::Enter, KeyModifiers::NONE), &toggle),
        Some(Transition::FocusMain)
    );
    assert_eq!(
        switcher.on_key(press(KeyCode::Down, KeyModifiers::NONE), &toggle),
        None
    );
    assert_eq!(
        switcher.on_key(press(KeyCode::Enter, KeyModifiers::NONE), &toggle),
        Some(Transition::Focus(SubagentKey(2)))
    );
}

#[test]
fn a_rebound_toggle_closes_and_ctrl_space_then_types_nothing() {
    let mut switcher = switcher_with_five();
    let toggle = HotkeyBinding::from_str("alt+s");
    assert_eq!(
        switcher.on_key(press(KeyCode::Char('s'), KeyModifiers::ALT), &toggle),
        Some(Transition::Close)
    );
    assert_eq!(
        switcher.on_key(press(KeyCode::Char(' '), KeyModifiers::CONTROL), &toggle),
        None
    );
    assert_eq!(switcher.list.query(), "");
}

fn send(switcher: &mut Switcher, code: KeyCode, modifiers: KeyModifiers) {
    switcher.on_key(press(code, modifiers), &ctrl_space());
}

#[test]
fn navigation_keys_move_and_edit_keys_edit() {
    let mut switcher = switcher_with_five();
    send(&mut switcher, KeyCode::Char('n'), KeyModifiers::CONTROL);
    send(&mut switcher, KeyCode::Down, KeyModifiers::NONE);
    send(&mut switcher, KeyCode::Char('p'), KeyModifiers::CONTROL);
    send(&mut switcher, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(switcher.list.selected(), Target::Main);
    // Stepping up from the first row wraps to the last.
    send(&mut switcher, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(switcher.list.selected(), key(3));
    send(&mut switcher, KeyCode::Home, KeyModifiers::NONE);
    assert_eq!(switcher.list.selected(), Target::Main);
    send(&mut switcher, KeyCode::End, KeyModifiers::NONE);
    assert_eq!(switcher.list.selected(), key(3));
    send(&mut switcher, KeyCode::Home, KeyModifiers::NONE);
    send(&mut switcher, KeyCode::PageDown, KeyModifiers::NONE);
    assert_eq!(
        switcher.list.selected(),
        key(3),
        "a page ends at the end of a short list"
    );
    send(&mut switcher, KeyCode::PageUp, KeyModifiers::NONE);
    assert_eq!(switcher.list.selected(), Target::Main);
    // A space is a space in the query; ctrl+u clears it; backspace edits it.
    send(&mut switcher, KeyCode::Char('r'), KeyModifiers::NONE);
    send(&mut switcher, KeyCode::Char(' '), KeyModifiers::NONE);
    send(&mut switcher, KeyCode::Char('x'), KeyModifiers::SHIFT);
    assert_eq!(switcher.list.query(), "r x");
    send(&mut switcher, KeyCode::Backspace, KeyModifiers::NONE);
    assert_eq!(switcher.list.query(), "r ");
    send(&mut switcher, KeyCode::Char('w'), KeyModifiers::CONTROL);
    assert_eq!(switcher.list.query(), "");
    send(&mut switcher, KeyCode::Char('a'), KeyModifiers::NONE);
    send(&mut switcher, KeyCode::Char('u'), KeyModifiers::CONTROL);
    assert_eq!(switcher.list.query(), "");
}

#[test]
fn the_preview_shows_only_on_wide_terminals() {
    let wide = layout(Rect::new(0, 0, 120, 30));
    let preview = wide.preview.expect("preview at 120 columns");
    assert_eq!(wide.list.width + 1 + preview.width, 120);
    assert!(
        preview.width * 100 / 120 >= 54,
        "the preview takes about 55%"
    );
    assert!(layout(Rect::new(0, 0, 100, 30)).preview.is_some());
    let narrow = layout(Rect::new(0, 0, 99, 30));
    assert_eq!(narrow.preview, None);
    assert_eq!(narrow.list.width, 99);
}

#[test]
fn the_preview_body_is_asked_for_only_when_it_is_missing_or_out_of_date() {
    let base = Instant::now();
    let mut switcher = switcher_with_five();
    let wide = Rect::new(0, 0, 120, 30);
    let narrow = Rect::new(0, 0, 80, 30);
    assert_eq!(switcher.body_query(narrow, base), None);
    let query = switcher
        .body_query(wide, base)
        .expect("nothing fetched yet");
    assert_eq!(query.target, Target::Main);
    let body = |query: BodyQuery, revision, at| Body {
        query,
        revision,
        lines: Vec::new(),
        built_at: at,
    };
    switcher.set_body(body(query, 0, base));
    assert_eq!(switcher.body_query(wide, base), None, "nothing changed");
    // The selection moves to a running row: its body is asked for, and it goes stale as it runs.
    switcher.on_key(press(KeyCode::Down, KeyModifiers::NONE), &ctrl_space());
    let query = switcher.body_query(wide, base).expect("another row");
    assert_eq!(query.target, key(2));
    switcher.set_body(body(query, 1, base));
    assert_eq!(
        switcher.body_query(wide, base + Duration::from_millis(100)),
        None
    );
    assert!(
        switcher
            .body_query(wide, base + Duration::from_millis(400))
            .is_some(),
        "a running row's tools animate, so its body is rebuilt now and then"
    );
}
