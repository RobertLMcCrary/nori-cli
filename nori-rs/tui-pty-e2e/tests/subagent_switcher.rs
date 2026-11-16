use tui_pty_e2e::Key;
use tui_pty_e2e::TIMEOUT;
use tui_pty_e2e::TIMEOUT_INPUT;
use tui_pty_e2e::TuiSession;

/// Ctrl+Space (a NUL byte on the wire) opens the subagent switcher even with no subagents, shows
/// the main session and the hint for delegating, and closes again on Esc and on a second
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
        .wait_for_text("no subagents yet", TIMEOUT)
        .expect("The switcher did not open");
    let open = session.screen_contents();
    assert!(open.contains("S U B A G E N T S"), "{open}");
    assert!(open.contains("main"), "{open}");

    session.send_key(Key::Escape).unwrap();
    session
        .wait_for(|screen| !screen.contains("no subagents yet"), TIMEOUT)
        .expect("Esc did not close the switcher");
    session
        .wait_for_text("›", TIMEOUT)
        .expect("The prompt did not come back");

    session.send_key(Key::Ctrl(' ')).unwrap();
    session
        .wait_for_text("no subagents yet", TIMEOUT)
        .expect("The switcher did not open again");
    session.send_key(Key::Ctrl(' ')).unwrap();
    session
        .wait_for(|screen| !screen.contains("no subagents yet"), TIMEOUT)
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
        .wait_for_text("no subagents yet", TIMEOUT)
        .expect("The switcher did not open");
    session.send_str("zzz").unwrap();
    session
        .wait_for_text("no matches", TIMEOUT)
        .expect("Typing did not filter the list");
    session.send_key(Key::Ctrl('u')).unwrap();
    session
        .wait_for(|screen| screen.contains("no subagents yet"), TIMEOUT)
        .expect("Ctrl+U did not clear the query");
    session.send_key(Key::Enter).unwrap();
    session
        .wait_for(|screen| !screen.contains("no subagents yet"), TIMEOUT)
        .expect("Enter on main did not return to the chat");
    assert!(
        !session.screen_contents().contains("zzz"),
        "what was typed in the switcher must not reach the composer"
    );
}
