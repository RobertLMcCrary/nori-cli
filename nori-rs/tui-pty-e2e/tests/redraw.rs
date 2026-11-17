/// Ctrl+Shift+L hides output while keeping the input prompt ready.
#[test]
#[cfg(target_os = "linux")]
fn ctrl_shift_l_hides_the_visible_response() {
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

    session.send_str("\x1b[108;6u").unwrap();
    std::thread::sleep(TIMEOUT_INPUT);

    assert!(!session.screen_contents().contains("Redraw me please"));
    assert!(session.screen_contents().contains("›"));
}
