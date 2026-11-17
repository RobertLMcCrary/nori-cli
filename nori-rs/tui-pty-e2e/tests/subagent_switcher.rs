use tui_pty_e2e::Key;
use tui_pty_e2e::TIMEOUT;
use tui_pty_e2e::TIMEOUT_INPUT;
use tui_pty_e2e::TuiSession;

/// Ctrl+Space (a NUL byte on the wire) opens the subagent switcher even with no subagents, shows
/// the main session and the empty child list, and closes again on Esc and on a second
/// Ctrl+Space.
#[test]
fn ctrl_space_opens_the_switcher_and_closes_it_again() {
    let mut session = TuiSession::spawn(30, 120).unwrap();
    session
        .wait_for_text("›", TIMEOUT)
        .expect("Prompt did not appear");
    std::thread::sleep(TIMEOUT_INPUT);

    session.send_key(Key::Ctrl(' ')).unwrap();
    session
        .wait_for_text("No child conversations", TIMEOUT)
        .expect("The switcher did not open");
    let open = session.screen_contents();
    assert!(open.contains("◇ Conversations"), "{open}");
    assert!(open.contains("main"), "{open}");

    session.send_key(Key::Escape).unwrap();
    session
        .wait_for(|screen| !screen.contains("No child conversations"), TIMEOUT)
        .expect("Esc did not close the switcher");
    session
        .wait_for_text("›", TIMEOUT)
        .expect("The prompt did not come back");

    session.send_key(Key::Ctrl(' ')).unwrap();
    session
        .wait_for_text("No child conversations", TIMEOUT)
        .expect("The switcher did not open again");
    session.send_key(Key::Ctrl(' ')).unwrap();
    session
        .wait_for(|screen| !screen.contains("No child conversations"), TIMEOUT)
        .expect("Ctrl+Space did not close the switcher");
}

/// Typing in the switcher filters it instead of reaching the composer, and Enter on `main`
/// returns to the chat.
#[test]
fn the_switcher_takes_typing_and_enter_on_main_returns_to_the_chat() {
    let mut session = TuiSession::spawn(30, 120).unwrap();
    session
        .wait_for_text("›", TIMEOUT)
        .expect("Prompt did not appear");
    std::thread::sleep(TIMEOUT_INPUT);

    session.send_key(Key::Ctrl(' ')).unwrap();
    session
        .wait_for_text("No child conversations", TIMEOUT)
        .expect("The switcher did not open");
    session.send_str("zzz").unwrap();
    session
        .wait_for_text("no matches", TIMEOUT)
        .expect("Typing did not filter the list");
    session.send_key(Key::Ctrl('u')).unwrap();
    session
        .wait_for(|screen| screen.contains("No child conversations"), TIMEOUT)
        .expect("Ctrl+U did not clear the query");
    session.send_key(Key::Enter).unwrap();
    session
        .wait_for(|screen| !screen.contains("No child conversations"), TIMEOUT)
        .expect("Enter on main did not return to the chat");
    assert!(
        !session.screen_contents().contains("zzz"),
        "what was typed in the switcher must not reach the composer"
    );
}

/// Ctrl+Tab remains a modified Tab while the switcher uses the primary screen.
#[test]
fn kitty_ctrl_tab_works_in_the_primary_screen_switcher() {
    let mut session = TuiSession::spawn(30, 120).unwrap();
    session
        .wait_for_text("›", TIMEOUT)
        .expect("Prompt did not appear");
    std::thread::sleep(TIMEOUT_INPUT);
    session.start_output_capture();

    session.send_key(Key::Ctrl(' ')).unwrap();
    session
        .wait_for_text("No child conversations", TIMEOUT)
        .expect("The switcher did not open");

    session.send_str("\x1b[9;5u").unwrap();
    session.send_str("\x1b[9;5:3u").unwrap();
    session
        .wait_for(|screen| !screen.contains("No child conversations"), TIMEOUT)
        .expect("Ctrl+Tab did not cycle back to the only session");

    let output = session
        .output_bytes()
        .expect("terminal output capture was started");
    for sequence in [b"\x1b[?1049h".as_slice(), b"\x1b[?1049l".as_slice()] {
        assert!(
            !output
                .windows(sequence.len())
                .any(|window| window == sequence),
            "switcher changed terminal screen"
        );
    }
}

/// A narrow fresh session accepts wheel and composer tap input before any child exists.
#[test]
fn narrow_fresh_session_accepts_wheel_and_composer_tap() {
    let mut session = TuiSession::spawn(25, 30).unwrap();
    session
        .wait_for_text("›", TIMEOUT)
        .expect("prompt did not appear");
    session.send_str("\x1b[<64;10;5M").unwrap();
    session.send_str("abcd").unwrap();
    session
        .wait_for_text("abcd", TIMEOUT)
        .expect("draft did not appear after wheel");
    let screen = session.screen_contents();
    let (row, line) = screen
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains("abcd"))
        .expect("draft row");
    let column = line.split("abcd").next().unwrap().chars().count() + 2;
    let row = row + 1;
    session
        .send_str(&format!("\x1b[<0;{column};{row}M\x1b[<0;{column};{row}m"))
        .unwrap();
    session.send_str("X").unwrap();
    session
        .wait_for_text("aXbcd", TIMEOUT)
        .expect("tap did not position the composer cursor");
}
