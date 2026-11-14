use tui_pty_e2e::Key;
use tui_pty_e2e::SessionConfig;
use tui_pty_e2e::TIMEOUT;
use tui_pty_e2e::TIMEOUT_INPUT;
use tui_pty_e2e::TuiSession;

/// Ctrl+L repaints the screen from Nori's own model. It must leave the screen exactly as it was
/// when nothing was wrong with it, and must not reach for the full-screen clears that damage
/// scrollback.
#[test]
#[cfg(target_os = "linux")]
fn test_ctrl_l_repaints_the_visible_screen_unchanged() {
    let config = SessionConfig::new().with_mock_response("Redraw me please");
    let mut session = TuiSession::spawn_with_config(24, 80, config).unwrap();

    session
        .wait_for_text("›", TIMEOUT)
        .expect("Prompt did not appear");
    std::thread::sleep(TIMEOUT_INPUT);

    session.submit_input("hello").unwrap();
    session
        .wait_for_text("Redraw me please", TIMEOUT)
        .expect("Response did not appear");
    std::thread::sleep(TIMEOUT_INPUT);
    let before = session.screen_contents();

    session.send_key(Key::Ctrl('l')).unwrap();
    std::thread::sleep(TIMEOUT_INPUT);

    assert_eq!(before, session.screen_contents());
}
