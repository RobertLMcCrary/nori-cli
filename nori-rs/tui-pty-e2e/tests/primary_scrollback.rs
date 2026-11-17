use std::time::Duration;

use tui_pty_e2e::Key;
use tui_pty_e2e::SessionConfig;
use tui_pty_e2e::TuiSession;

#[test]
fn widening_and_growing_keeps_one_composer_and_its_draft() {
    let mut session = TuiSession::spawn(25, 30).expect("start TUI");
    session
        .wait_for_text("›", Duration::from_secs(20))
        .expect("prompt");
    session.send_str("resize draft marker").expect("type draft");
    session
        .wait_for_text("resize draft marker", Duration::from_secs(10))
        .expect("draft");
    session.resize(43, 122).expect("resize");
    session
        .wait_for(
            |screen| {
                screen
                    .lines()
                    .enumerate()
                    .any(|(row, line)| row >= 38 && line.contains("resize draft marker"))
            },
            Duration::from_secs(15),
        )
        .expect("draft repinned after resize");
    let screen = session.screen_contents();
    assert_eq!(screen.matches("resize draft marker").count(), 1, "{screen}");
}

#[test]
fn primary_history_survives_child_views_resize_and_exit_without_alternate_screen() {
    let response = (0..45)
        .map(|i| format!("native-history-line-{i:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    let config = SessionConfig::new().with_mock_response(response);
    let mut session = TuiSession::spawn_with_config(25, 80, config).expect("start TUI");
    session.start_output_capture();
    session
        .wait_for_text("›", Duration::from_secs(20))
        .expect("prompt");
    session
        .submit_input("native scrollback question")
        .expect("send prompt");
    session
        .wait_for_text("native-history-line-44", Duration::from_secs(20))
        .expect("response");
    let main_output = session.output_bytes().expect("capture");
    assert!(
        !main_output
            .windows(b"\x1b[?1000h".len())
            .any(|part| part == b"\x1b[?1000h"),
        "main view must leave native wheel scrolling enabled"
    );
    session.send_key(Key::Ctrl('h')).expect("focus response");
    session.send_str(" ").expect("fork response");
    session
        .wait_for_text("History fork", Duration::from_secs(20))
        .expect("fork card");
    session
        .send_str("\x1b[9;5u")
        .expect("focus child with Ctrl+Tab");
    session
        .send_str("child-only draft")
        .expect("type child draft");
    session
        .wait_for_text("child-only draft", Duration::from_secs(20))
        .expect("child draft");
    session.send_key(Key::Tab).expect("return to parent");
    session
        .wait_for(
            |screen| !screen.contains("child-only draft") && screen.contains('›'),
            Duration::from_secs(10),
        )
        .expect("parent view restored");
    session.resize(35, 100).expect("resize");
    session
        .wait_for_text("›", Duration::from_secs(10))
        .expect("parent prompt");
    session.submit_input("/quit").expect("quit");
    assert!(
        session.wait_for_process_exit(Duration::from_secs(15)),
        "TUI did not exit"
    );
    let _ = session.poll();
    let output = session.output_bytes().expect("capture");
    assert!(
        !output
            .windows(b"\x1b[?1049h".len())
            .any(|part| part == b"\x1b[?1049h"),
        "entered alternate screen"
    );
    assert!(
        !output
            .windows(b"\x1b[?1049l".len())
            .any(|part| part == b"\x1b[?1049l"),
        "left an alternate screen"
    );
    assert!(
        !output
            .windows(b"\x1b[3J".len())
            .any(|part| part == b"\x1b[3J"),
        "cleared native scrollback"
    );
    let found = [0, 10, 20, 30, 40, 50, 60, 80].into_iter().any(|offset| {
        session
            .scrollback_contents(offset)
            .contains("native-history-line-30")
    });
    assert!(
        found,
        "visible history must move into native scrollback when the child view opens"
    );
}

#[test]
fn immediate_quit_stays_fast_with_large_pending_tool_history() {
    let fixture = tempfile::NamedTempFile::new().expect("fixture file");
    let text = format!("{}\nqueue-tail", "pending output\n".repeat(2_000));
    let steps = [
        serde_json::json!({"t_ms":0,"update":{"sessionUpdate":"tool_call","toolCallId":"slow","title":"Slow command","kind":"execute","status":"in_progress","rawInput":{"command":"sleep"}}}),
        serde_json::json!({"t_ms":20,"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":text}}}),
        serde_json::json!({"t_ms":60000,"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"late"}}}),
    ];
    std::fs::write(
        fixture.path(),
        steps
            .into_iter()
            .map(|step| step.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .expect("write fixture");
    let config = SessionConfig::new().with_agent_env(
        "MOCK_AGENT_WIRE_REPLAY",
        fixture.path().to_str().expect("fixture path"),
    );
    let mut session = TuiSession::spawn_with_config(25, 80, config).expect("start TUI");
    session
        .wait_for_text("›", Duration::from_secs(20))
        .expect("prompt");
    session
        .submit_input("start slow tool")
        .expect("send prompt");
    session
        .wait_for_text("queue-tail", Duration::from_secs(20))
        .expect("pending output shown");
    session.send_key(Key::Ctrl('d')).expect("quit");
    assert!(
        session.wait_for_process_exit(Duration::from_secs(2)),
        "Ctrl+D waited for pending native history"
    );
}
